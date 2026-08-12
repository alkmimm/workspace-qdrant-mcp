//! Diff-to-database application logic for code_lines updates.
//!
//! All FTS5 index updates are performed incrementally inline with code_lines
//! changes, avoiding the need for a full FTS5 rebuild after each file.
//!
//! # Insertion ordering (F-018)
//!
//! Inserted lines are position-aware: each insertion records its `new_index`
//! in the target file. After all inserts, `renumber_after_changes` re-assigns
//! both `seq` and `line_number` in correct new-file order. This prevents
//! middle-of-file insertions from being appended at the end of the seq space.

use std::collections::HashSet;

use tracing::warn;
use wqm_common::hashing::normalize_line_endings;

use crate::code_lines_schema::initial_seq;
use crate::code_lines_schema::{FTS5_DELETE_ROW_SQL, FTS5_INSERT_ROW_SQL};
use crate::line_diff::{DiffOp, DiffResult};
use crate::search_db::SearchDbError;

/// Per-file statistics from applying a diff.
#[derive(Debug, Default)]
pub(super) struct FileDiffStats {
    pub(super) lines_inserted: usize,
    pub(super) lines_updated: usize,
    pub(super) lines_deleted: usize,
    pub(super) lines_unchanged: usize,
    /// Internal: tracks retained line_ids during diff application (cleared before return).
    retained_ids: HashSet<i64>,
    /// Internal: tracks line_ids already deleted by explicit Deleted ops (cleared before return).
    explicitly_deleted_ids: HashSet<i64>,
    /// Desired line_id ordering (new_index -> line_id) for correct seq renumbering.
    /// Built during diff application and used by `renumber_after_changes`.
    ordered_line_ids: Vec<i64>,
}

/// Apply a diff result to the code_lines table within a transaction.
///
/// For files that already have code_lines, this applies the minimal set of
/// INSERT/UPDATE/DELETE operations. For new files (no existing lines),
/// all lines from `new_content` are inserted with standard seq gaps.
///
/// FTS5 index entries are maintained incrementally — no full rebuild needed.
pub(super) async fn apply_diff_to_code_lines(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    file_id: i64,
    diff: &DiffResult,
    old_content: &str,
    new_content: &str,
) -> Result<FileDiffStats, SearchDbError> {
    // One read, not two: the old `SELECT COUNT(*)` only existed to fork on zero,
    // and the guard below needs the rows anyway. In batch mode that saved
    // `FTS5_BATCH_SIZE` round-trips per transaction, all taken while holding the
    // search.db write lock (`begin_immediate`, PRs #329/#330).
    let existing_lines = fetch_existing_lines(tx, file_id).await?;

    if existing_lines.is_empty() {
        return insert_all_lines(tx, file_id, new_content).await;
    }

    // `diff` is computed against the caller's `old_content` — a CLAIM about what
    // is indexed, read from the `indexed_content` cache in state.db, a different
    // database from these rows in search.db. Every old-side index in the diff
    // (`DiffOp::Unchanged/Changed/Deleted.old_index`) is used below to subscript
    // `existing_lines`, so the two must describe the same sequence. They do not
    // when the cache misses or its read fails: `fetch_old_content` yields an
    // EMPTY string for both, and the batch/enqueue lane hands it straight here
    // while the file's rows are still present (the single-file lane guards at
    // `execute_fts_update`). The differ then reports every new line as Inserted
    // and pairs the old side's lone "" with the new side's TRAILING "" as
    // `Unchanged{old_index: 0, new_index: N}` — retaining the row for line 1 and
    // renumbering it to the LAST position, so the file's first line reappears
    // where the empty final line belongs. Measured live 2026-08-12: 322 of 324
    // sampled DOC-V2 files carried that shape, making every grep for first-line
    // content return a phantom hit past the end of the file.
    //
    // The stored rows are the authority for what is indexed, so on any
    // disagreement discard the diff and rebuild the file's lines from scratch.
    //
    // Compare the CONTENT, not just the line count. A count check passes for a
    // stale-but-same-length claim (a pure reorder, or an `indexed_content` upsert
    // lost after the search.db commit — different databases, no cross-DB
    // atomicity) and the old-side indices then address the wrong rows, corrupting
    // the file exactly as before. It also matters for REPAIR: a file already in
    // the corrupted shape has the RIGHT row count (N+1) with the first line
    // parked in the final slot, so a count check would keep re-applying the
    // damage on every later edit. Comparing content heals it on the next one.
    //
    // `old_content` is the cache's raw bytes while the rows hold normalized EOL
    // (`insert_all_lines`), so normalize before comparing — otherwise every CRLF
    // file mismatches and rebuilds forever.
    let claimed = normalize_line_endings(old_content);
    let rows_match_claim = claimed
        .split('\n')
        .eq(existing_lines.iter().map(|(_, _, stored)| stored.as_str()));
    if !rows_match_claim {
        warn!(
            file_id,
            claimed_lines = claimed.split('\n').count(),
            stored_rows = existing_lines.len(),
            "FTS5: indexed_content disagrees with stored code_lines; rebuilding the \
             file's lines instead of applying a diff against rows it does not describe"
        );
        return rebuild_all_lines(tx, file_id, &existing_lines, new_content).await;
    }

    let mut stats = FileDiffStats::default();
    apply_diff_ops(tx, diff, &existing_lines, file_id, &mut stats).await?;
    let orphan_deleted = delete_orphaned_lines(
        tx,
        &existing_lines,
        &stats.retained_ids,
        &stats.explicitly_deleted_ids,
    )
    .await?;
    stats.lines_deleted += orphan_deleted;
    renumber_after_changes(tx, file_id, &mut stats).await?;

    // Clear internal tracking fields before returning
    stats.retained_ids.clear();
    stats.explicitly_deleted_ids.clear();
    stats.ordered_line_ids.clear();
    Ok(stats)
}

/// Drop every stored line for a file and re-insert it from `new_content`.
///
/// The recovery path for a diff whose old side does not describe the stored rows
/// (see the guard in `apply_diff_to_code_lines`). Costlier than an incremental
/// apply, but it cannot corrupt: no old-side index is trusted. `existing_lines`
/// is the already-fetched row set, needed to retire each FTS5 entry by its exact
/// stored content — the external-content index cannot delete rows it no longer
/// has the text for.
async fn rebuild_all_lines(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    file_id: i64,
    existing_lines: &[(i64, f64, String)],
    new_content: &str,
) -> Result<FileDiffStats, SearchDbError> {
    for (line_id, _, old_content) in existing_lines {
        sqlx::query(FTS5_DELETE_ROW_SQL)
            .bind(*line_id)
            .bind(old_content.as_str())
            .execute(&mut **tx)
            .await?;
    }
    sqlx::query("DELETE FROM code_lines WHERE file_id = ?1")
        .bind(file_id)
        .execute(&mut **tx)
        .await?;

    let mut stats = insert_all_lines(tx, file_id, new_content).await?;
    // `+=`, not `=`: states the intent and survives `insert_all_lines` ever
    // touching this field itself.
    stats.lines_deleted += existing_lines.len();
    Ok(stats)
}

/// Insert all lines for first-time ingestion (no existing code_lines).
/// Also inserts corresponding FTS5 index entries.
async fn insert_all_lines(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    file_id: i64,
    content: &str,
) -> Result<FileDiffStats, SearchDbError> {
    let mut stats = FileDiffStats::default();
    // Normalize EOL so first-ingestion code_lines carry no trailing '\r'.
    let normalized = normalize_line_endings(content);
    let lines: Vec<&str> = normalized.split('\n').collect();
    for (i, line) in lines.iter().enumerate() {
        let seq = initial_seq(i);
        let line_number = (i + 1) as i64;
        let result = sqlx::query(
            "INSERT INTO code_lines (file_id, seq, content, line_number) VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(file_id)
        .bind(seq)
        .bind(*line)
        .bind(line_number)
        .execute(&mut **tx)
        .await?;

        // Incrementally update FTS5 index
        let line_id = result.last_insert_rowid();
        sqlx::query(FTS5_INSERT_ROW_SQL)
            .bind(line_id)
            .bind(*line)
            .execute(&mut **tx)
            .await?;

        stats.lines_inserted += 1;
    }
    Ok(stats)
}

/// Fetch existing line_id, seq, and content values for a file, ordered by seq.
/// Content is needed for FTS5 incremental delete operations.
async fn fetch_existing_lines(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    file_id: i64,
) -> Result<Vec<(i64, f64, String)>, SearchDbError> {
    let rows =
        sqlx::query("SELECT line_id, seq, content FROM code_lines WHERE file_id = ?1 ORDER BY seq")
            .bind(file_id)
            .fetch_all(&mut **tx)
            .await?;

    Ok(rows
        .iter()
        .map(|r| {
            use sqlx::Row;
            (
                r.get::<i64, _>("line_id"),
                r.get::<f64, _>("seq"),
                r.get::<String, _>("content"),
            )
        })
        .collect())
}

/// Process diff operations: update changed lines, insert new lines, delete
/// removed lines. Builds `stats.ordered_line_ids` in new-file order so
/// `renumber_after_changes` can assign correct `seq` and `line_number`.
///
/// FTS5 index is updated incrementally for Changed, Inserted and Deleted ops.
async fn apply_diff_ops(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    diff: &DiffResult,
    existing_lines: &[(i64, f64, String)],
    file_id: i64,
    stats: &mut FileDiffStats,
) -> Result<(), SearchDbError> {
    // Pre-size: one slot per new-file line (exact count not known,
    // but ops.len() is an upper-bound approximation).
    let mut ordered: Vec<(usize, i64)> = Vec::with_capacity(diff.ops.len());

    for op in &diff.ops {
        match op {
            DiffOp::Unchanged {
                old_index,
                new_index,
            } => {
                if let Some(&(line_id, _, _)) = existing_lines.get(*old_index) {
                    stats.retained_ids.insert(line_id);
                    stats.lines_unchanged += 1;
                    ordered.push((*new_index, line_id));
                }
            }
            DiffOp::Changed {
                old_index,
                new_index,
                new_content: content,
            } => {
                if let Some((line_id, _, old_content)) = existing_lines.get(*old_index) {
                    let line_id = *line_id;

                    // FTS5: delete old content, then insert new content
                    sqlx::query(FTS5_DELETE_ROW_SQL)
                        .bind(line_id)
                        .bind(old_content.as_str())
                        .execute(&mut **tx)
                        .await?;

                    sqlx::query("UPDATE code_lines SET content = ?1 WHERE line_id = ?2")
                        .bind(content.as_str())
                        .bind(line_id)
                        .execute(&mut **tx)
                        .await?;

                    sqlx::query(FTS5_INSERT_ROW_SQL)
                        .bind(line_id)
                        .bind(content.as_str())
                        .execute(&mut **tx)
                        .await?;

                    stats.retained_ids.insert(line_id);
                    stats.lines_updated += 1;
                    ordered.push((*new_index, line_id));
                }
            }
            DiffOp::Inserted {
                new_index,
                new_content: content,
            } => {
                let line_id = insert_single_line(tx, file_id, content, *new_index).await?;
                stats.lines_inserted += 1;
                ordered.push((*new_index, line_id));
            }
            DiffOp::Deleted { old_index } => {
                if let Some((line_id, _, old_content)) = existing_lines.get(*old_index) {
                    let line_id = *line_id;

                    // FTS5: delete before removing from code_lines
                    sqlx::query(FTS5_DELETE_ROW_SQL)
                        .bind(line_id)
                        .bind(old_content.as_str())
                        .execute(&mut **tx)
                        .await?;

                    sqlx::query("DELETE FROM code_lines WHERE line_id = ?1")
                        .bind(line_id)
                        .execute(&mut **tx)
                        .await?;
                    stats.lines_deleted += 1;
                    stats.explicitly_deleted_ids.insert(line_id);
                }
            }
        }
    }

    // Sort by new_index to get correct new-file order
    ordered.sort_by_key(|(idx, _)| *idx);
    stats.ordered_line_ids = ordered.into_iter().map(|(_, id)| id).collect();

    Ok(())
}

/// Insert a single line into code_lines with a temporary seq and line_number.
/// The correct seq and line_number will be assigned by `renumber_after_changes`.
///
/// `new_index` is the line's position in the new file content; it's used to
/// derive a UNIQUE temporary seq within the transaction. Two or more
/// `DiffOp::Inserted` operations against the same file in one transaction
/// would otherwise collide on the `UNIQUE(file_id, seq)` constraint —
/// previously every insert used `f64::MAX / 2.0`, so any diff that produced
/// multiple Inserted ops (e.g. multi-line edits, common in batch mode)
/// failed before `renumber_after_changes` had a chance to re-assign seqs.
async fn insert_single_line(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    file_id: i64,
    content: &str,
    new_index: usize,
) -> Result<i64, SearchDbError> {
    // Temp seq base sits far above any real seq (real ones come from
    // `initial_seq(i)`, gap-based around 1e3–1e6 in practice); adding
    // `new_index` keeps each insert distinct within the transaction.
    // Renumber overwrites all of these immediately after.
    let temp_seq = 1.0e15_f64 + new_index as f64;
    let result = sqlx::query(
        "INSERT INTO code_lines (file_id, seq, content, line_number) VALUES (?1, ?2, ?3, 0)",
    )
    .bind(file_id)
    .bind(temp_seq)
    .bind(content)
    .execute(&mut **tx)
    .await?;

    let line_id = result.last_insert_rowid();

    // Incrementally update FTS5 index
    sqlx::query(FTS5_INSERT_ROW_SQL)
        .bind(line_id)
        .bind(content)
        .execute(&mut **tx)
        .await?;

    Ok(line_id)
}

/// Delete existing lines not referenced by any Unchanged or Changed op.
/// Also removes corresponding FTS5 index entries.
/// Returns the number of orphaned lines deleted.
async fn delete_orphaned_lines(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    existing_lines: &[(i64, f64, String)],
    retained_ids: &std::collections::HashSet<i64>,
    explicitly_deleted_ids: &std::collections::HashSet<i64>,
) -> Result<usize, SearchDbError> {
    let mut deleted = 0;
    for (line_id, _, old_content) in existing_lines {
        if !retained_ids.contains(line_id) && !explicitly_deleted_ids.contains(line_id) {
            // FTS5: delete before removing from code_lines
            sqlx::query(FTS5_DELETE_ROW_SQL)
                .bind(*line_id)
                .bind(old_content.as_str())
                .execute(&mut **tx)
                .await?;

            let result = sqlx::query("DELETE FROM code_lines WHERE line_id = ?1")
                .bind(*line_id)
                .execute(&mut **tx)
                .await?;
            if result.rows_affected() > 0 {
                deleted += 1;
            }
        }
    }
    Ok(deleted)
}

/// Renumber seq and line_number values based on the correct new-file ordering.
///
/// If `stats.ordered_line_ids` was populated (incremental diff path), it
/// provides the authoritative ordering. Otherwise falls back to the existing
/// `seq` ordering from the database (bulk-insert path).
async fn renumber_after_changes(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    file_id: i64,
    stats: &mut FileDiffStats,
) -> Result<(), SearchDbError> {
    if stats.lines_inserted == 0 && stats.lines_deleted == 0 {
        return Ok(());
    }

    // Use the authoritative ordering from diff application when available.
    let line_ids = if !stats.ordered_line_ids.is_empty() {
        std::mem::take(&mut stats.ordered_line_ids)
    } else {
        // Fallback: fetch from DB ordered by seq (insert-all path)
        sqlx::query_scalar("SELECT line_id FROM code_lines WHERE file_id = ?1 ORDER BY seq")
            .bind(file_id)
            .fetch_all(&mut **tx)
            .await?
    };

    // Two-pass renumber to avoid transient `UNIQUE(file_id, seq)` collisions.
    //
    // Assigning the final seqs in place, row by row, can collide: when lines
    // are reordered or inserted mid-file, an UPDATE may set seq=initial_seq(i)
    // while a not-yet-renumbered row still holds that exact seq (its old
    // value). That surfaced as `(code: 2067) UNIQUE constraint failed:
    // code_lines.file_id, code_lines.seq`, failing the FTS5 batch.
    //
    // Pass 1 parks every target row in a temporary seq range far above any
    // real seq (real seqs come from `initial_seq` ~1e3–1e6 and the insert temp
    // base 1e15). Pass 2 then assigns the final gap-based seqs into the
    // now-vacated low range, where no live row can conflict.
    const RENUMBER_TEMP_SEQ_BASE: f64 = 2.0e15;
    for (i, line_id) in line_ids.iter().enumerate() {
        sqlx::query("UPDATE code_lines SET seq = ?1 WHERE line_id = ?2")
            .bind(RENUMBER_TEMP_SEQ_BASE + i as f64)
            .bind(*line_id)
            .execute(&mut **tx)
            .await?;
    }
    for (i, line_id) in line_ids.iter().enumerate() {
        let new_seq = initial_seq(i);
        let new_line_number = (i + 1) as i64;
        sqlx::query("UPDATE code_lines SET seq = ?1, line_number = ?2 WHERE line_id = ?3")
            .bind(new_seq)
            .bind(new_line_number)
            .bind(*line_id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}
