//! `file_mtime`: the stamp that lets the index prove a file unchanged with a
//! `stat` instead of a read.
//!
//! `tracked_files.file_mtime` records the file's modification time at ingest
//! next to its content hash. When the file carries exactly that mtime NOW, its
//! recorded `file_hash` IS its hash: content cannot change without the mtime
//! changing (the 10 s watcher debounce also rules out two writes inside one
//! stamp reaching the index as one). Every ingest path — store_track, the
//! zero-byte path, branch dedup — writes the stamp through [`get_file_mtime`],
//! and every reader goes through [`MtimeStamps`], so the two sides can never
//! drift apart again.
//!
//! They did drift (2026-09-19): branch dedup wrote epoch SECONDS
//! (`1784474124`) while everything else wrote millisecond ISO-8601
//! (`2026-07-19T15:15:24.473Z`). On repos that live in worktrees most rows
//! came from the dedup path, so the mtime fast path (#397) matched 12 of
//! 4 896 unchanged files on one repo and re-read the rest on every daemon
//! start — the churn it existed to remove. Rows written before the fix keep
//! the seconds form; [`MtimeStamps::matches`] accepts both, so no rewrite of
//! the table is needed, and the recovery refreshes a row's stamp whenever the
//! hash proves the file unchanged, so a file whose mtime moved without its
//! content (a `git checkout`, a copy) converges onto the fast path instead of
//! being re-read forever.

use sqlx::SqlitePool;
use std::path::Path;
use wqm_common::hashing::compute_file_hash;
use wqm_common::timestamps;

/// The two spellings of one modification time: the millisecond ISO-8601 form
/// every writer produces today, and the epoch-seconds form the branch-dedup
/// path wrote until 2026-09-19 and that older rows still carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MtimeStamps {
    pub iso_millis: String,
    pub epoch_secs: String,
}

impl MtimeStamps {
    /// Read the file's mtime once and spell it both ways.
    pub fn of(path: &Path) -> std::io::Result<Self> {
        let mtime = std::fs::metadata(path)?.modified()?;
        let datetime: chrono::DateTime<chrono::Utc> = mtime.into();
        let epoch_secs = mtime
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs().to_string())
            .unwrap_or_default();
        Ok(Self {
            iso_millis: timestamps::format_utc(&datetime),
            epoch_secs,
        })
    }

    /// Does a stored `file_mtime` name this same instant? An empty stamp never
    /// matches — a row without an mtime proves nothing.
    pub fn matches(&self, stored: &str) -> bool {
        !stored.is_empty() && (stored == self.iso_millis || stored == self.epoch_secs)
    }
}

/// Get file modification time as ISO 8601 string — THE spelling every
/// `tracked_files` writer must use.
pub fn get_file_mtime(path: &Path) -> std::io::Result<String> {
    MtimeStamps::of(path).map(|s| s.iso_millis)
}

/// The content hash of `abs_path`, WITHOUT reading the file whenever the
/// index already proves it unchanged.
///
/// When a row for this `(watch_folder, relative_path)` — any branch, any
/// content generation — carries the mtime the file has NOW (in either
/// spelling, see [`MtimeStamps`]), its `file_hash` is returned without a
/// read. That turns the check into a `stat` plus one indexed SELECT.
///
/// Why this matters (2026-09-18): every daemon start re-hashed the whole
/// corpus — the startup recovery walks every tracked file, the progressive
/// scan re-enqueues every file as `add`, and worktree branch-membership
/// reconciles enqueue every file of every linked worktree (33 worktrees ×
/// ~4 650 files on one repo = ~150 000 items) — each one a full read of a
/// file that turned out identical. Inside a WSL2 VM that churn is what the
/// Windows host pays for (a 60 GB `vmmemWSL` for a stack whose processes
/// held 17 GB), and on 2026-09-16 it took the host down.
///
/// Returns `(hash, reused)`; `reused` is true when no byte was read. Any
/// lookup error falls back to hashing — the fast path is an optimisation,
/// never a reason to fail an ingest.
pub async fn content_hash_reusing_mtime(
    pool: &SqlitePool,
    watch_folder_id: &str,
    relative_path: &str,
    abs_path: &Path,
) -> std::io::Result<(String, bool)> {
    if let Ok(now) = MtimeStamps::of(abs_path) {
        let hit: Result<Option<String>, sqlx::Error> = sqlx::query_scalar(
            "SELECT file_hash FROM tracked_files
             WHERE watch_folder_id = ?1 AND relative_path = ?2
               AND file_mtime IN (?3, ?4)
             LIMIT 1",
        )
        .bind(watch_folder_id)
        .bind(relative_path)
        .bind(&now.iso_millis)
        .bind(&now.epoch_secs)
        .fetch_optional(pool)
        .await;
        match hit {
            Ok(Some(hash)) => {
                tracing::debug!(
                    "mtime fast path: {} unchanged since {} — reusing recorded hash",
                    relative_path,
                    now.iso_millis
                );
                return Ok((hash, true));
            }
            Ok(None) => {}
            Err(e) => tracing::debug!(
                "mtime fast path lookup failed for {} ({}); hashing instead",
                relative_path,
                e
            ),
        }
    }
    compute_file_hash(abs_path).map(|h| (h, false))
}

/// One `(relative_path, file_hash, iso_millis)` whose hash was just proven
/// equal to the bytes on disk while its stored `file_mtime` was not the
/// file's current one.
pub type MtimeRefresh = (String, String, String);

/// Re-stamp rows whose content the caller has just proven unchanged, so the
/// next start settles them by mtime. Only rows still holding exactly that
/// hash are touched — a stamp must never be paired with a hash it does not
/// prove — and the whole batch is one immediate transaction (a per-file
/// UPDATE at startup would be thousands of autocommits against a database the
/// queue processor is already writing to). Returns the number of rows
/// re-stamped.
pub async fn refresh_mtime_for_unchanged(
    pool: &SqlitePool,
    watch_folder_id: &str,
    refresh: &[MtimeRefresh],
) -> Result<u64, sqlx::Error> {
    if refresh.is_empty() {
        return Ok(0);
    }
    let mut tx = crate::db_retry::begin_immediate(pool).await?;
    let mut rows = 0u64;
    for (relative_path, file_hash, iso_millis) in refresh {
        rows += sqlx::query(
            "UPDATE tracked_files SET file_mtime = ?3
             WHERE watch_folder_id = ?1 AND relative_path = ?2
               AND file_hash = ?4 AND file_mtime != ?3",
        )
        .bind(watch_folder_id)
        .bind(relative_path)
        .bind(iso_millis)
        .bind(file_hash)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    }
    tx.commit().await?;
    Ok(rows)
}
