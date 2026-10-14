//! Cross-branch ingestion fast-path (Layer 2 — share one point across branches).
//!
//! With a branch-agnostic `base_point` (`SHA256(tenant|relative_path|file_hash)`),
//! identical content on different branches maps to ONE physical Qdrant point.
//! When a `file/add` or `file/update` item is processed whose content another
//! branch already indexed (same `(watch_folder_id, relative_path, file_hash)`),
//! the expensive parse + embed is skipped AND no vectors are copied — the
//! file's existing chunk set is REUSED in place:
//!
//! 1. Add the current branch to the shared points' `branch` array payload
//!    (`set_payload`), so branch-scoped search returns them on this branch.
//! 2. Insert a `tracked_files` row for this branch pointing at the same
//!    `base_point` (+ chunk_count, source `dedup_share`).
//! 3. Copy the `qdrant_chunks` mirror rows from the source row (same point_ids,
//!    since the base_point — hence every point_id — is shared).
//! 4. Enqueue FTS5 work so search.db's `file_metadata.branches` gains the
//!    current branch. The content-row's `code_lines` already hold this content,
//!    so the change diffs against the cached claim and leaves them untouched.
//! 5. Flip qdrant_status=done, search_status=in_progress.
//!
//! This makes `git checkout` between branches near-free on the indexed-data
//! side and — unlike Layer 1 — adds NO Qdrant storage per branch.
//!
//! See [docs/specs/21-cross-branch-dedup.md](../../../../../../../docs/specs/21-cross-branch-dedup.md).

use std::path::Path;

use sqlx::SqlitePool;
use tracing::{info, warn};

use crate::context::ProcessingContext;
use crate::fts_batch_processor::FileChange;
use crate::search_db::Fts5WorkItem;
use crate::tracked_files_schema::{self, ProcessingStatus};
use crate::unified_queue_processor::UnifiedProcessorError;
use crate::unified_queue_schema::{
    DestinationStatus, FilePayload, QueueOperation, UnifiedQueueItem,
};
use wqm_common::hashing::{compute_base_point, compute_content_hash};

/// Outcome of [`try_branch_dedup`] — `Some` means the dedup fast-path completed
/// and the caller must return early; `None` means the file is novel (or the
/// shared points are missing) and the normal ingest pipeline should run.
pub(super) struct DedupHit {
    /// The shared content generation (its graph is this branch's too).
    pub base_point: String,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn try_branch_dedup(
    ctx: &ProcessingContext,
    item: &UnifiedQueueItem,
    payload: &FilePayload,
    file_path: &Path,
    abs_file_path: &str,
    base_path: &str,
    relative_path: &str,
    watch_folder_id: &str,
) -> Result<Option<DedupHit>, UnifiedProcessorError> {
    // Uplift demands a fresh extraction pass — reusing another branch's chunk
    // set is exactly what the capability/extractor upgrade is replacing.
    if item.op == QueueOperation::Uplift {
        return Ok(None);
    }

    // Same mtime fast path as prepare_update (which already ran for this item
    // and made the row hot): a worktree baseline pass reads the MAIN tree's
    // copy of a file the index already tracks at this exact mtime, so this is
    // the second full read the old code spent on a hash it already had.
    let (file_hash, _reused) = tracked_files_schema::content_hash_reusing_mtime(
        &ctx.pool,
        watch_folder_id,
        relative_path,
        file_path,
    )
    .await
    .map_err(|e| UnifiedProcessorError::ProcessingFailed(e.to_string()))?;

    // ── 1. Is this content already indexed (any branch)? ──
    // Layer 2 stage 2: one content-row per (watch, relative_path, file_hash). If
    // it exists, the shared Qdrant points exist too — we add this branch to them
    // instead of re-embedding. (prepare_update already skipped the case where
    // THIS branch holds the content with a current chunker fingerprint.)
    type DedupRow = (
        i32,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let existing: Option<DedupRow> = sqlx::query_as(
        "SELECT chunk_count, file_type, language, chunker_version, treesitter_status
             FROM tracked_files
             WHERE watch_folder_id = ?1
               AND relative_path = ?2
               AND file_hash = ?3
               AND base_point IS NOT NULL
             ORDER BY updated_at DESC
             LIMIT 1",
    )
    .bind(watch_folder_id)
    .bind(relative_path)
    .bind(&file_hash)
    .fetch_optional(&ctx.pool)
    .await
    .map_err(|e| UnifiedProcessorError::ProcessingFailed(format!("dedup lookup: {e}")))?;

    let Some((chunk_count, file_type, language, src_chunker_version, src_treesitter_status)) =
        existing
    else {
        return Ok(None);
    };

    // Carry the source row's tree-sitter status verbatim. The dedup clone shares
    // the source's `base_point` — hence the exact same Qdrant points, which were
    // semantically chunked (or not) once, at the source's real ingest. Forcing
    // `None` here would REBASE an already-`done` file back to `none`: the metric
    // (`tracked_files_by_chunking`) would under-report semantic coverage, and the
    // capability-upgrade query (`treesitter_status IN ('none','failed','skipped')`)
    // would re-enqueue the file forever with nothing to re-chunk. See
    // docs/specs/21-cross-branch-dedup.md.
    let carried_treesitter_status = src_treesitter_status
        .as_deref()
        .and_then(ProcessingStatus::from_str)
        .unwrap_or(ProcessingStatus::None);

    // Stale-generation guard: only reuse chunks produced by the CURRENT chunking
    // configuration. Reusing a pre-upgrade generation would carry stale chunks
    // (and the stale fingerprint) into this branch, and the fingerprint gate
    // would re-trigger on every later visit without converging. NULL (legacy
    // source) is grandfathered, same as the gate.
    let overrides = super::component::get_gitattributes(ctx, base_path).await;
    let detected =
        crate::tree_sitter::detect_language_with_overrides(file_path, relative_path, &overrides);
    let current_fp = crate::tree_sitter::chunker::chunking_fingerprint(detected);
    if !crate::tree_sitter::chunker::stored_fingerprint_is_current(
        src_chunker_version.as_deref(),
        &current_fp,
    ) {
        info!(
            "branch_dedup: source row for {} was chunked with stale configuration {:?} (current {}) — falling back to full ingest",
            relative_path, src_chunker_version, current_fp
        );
        return Ok(None);
    }

    // base_point is branch-agnostic: the source row's base_point IS ours.
    let base_point = compute_base_point(&item.tenant_id, relative_path, &file_hash);

    // ── 2. Add this branch to the shared points' `branch` array ──
    let point_count = ctx
        .storage_client
        .add_branch_to_base_point(&item.collection, &base_point, &item.branch)
        .await
        .map_err(|e| UnifiedProcessorError::Storage(e.to_string()))?;
    if point_count == 0 {
        // tracked_files claims chunks but Qdrant has none — stale row / partial
        // cleanup. Fall back to a full ingest so the file is embedded fresh.
        warn!(
            "branch_dedup: content row for {} has base_point {} but Qdrant returned 0 points — falling back to normal ingest",
            relative_path, base_point
        );
        return Ok(None);
    }

    // ── 3. tracked_files row + qdrant_chunks mirror for this branch ──
    // Same formatter as store_track: the mtime fast path compares this stamp
    // with the file's current one, and until 2026-09-19 this path wrote epoch
    // seconds while every other writer wrote millisecond ISO-8601 — so no
    // dedup-shared row ever matched and every restart re-read them all.
    let file_mtime = tracked_files_schema::get_file_mtime(file_path).unwrap_or_default();
    let extension = file_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_lowercase());
    let is_test = crate::file_classification::is_test_file(file_path);

    // BEGIN IMMEDIATE + busy retry (see db_retry) to avoid SQLITE_BUSY_SNAPSHOT.
    let mut tx = crate::db_retry::begin_immediate(&ctx.pool)
        .await
        .map_err(|e| UnifiedProcessorError::ProcessingFailed(format!("dedup tx begin: {e}")))?;
    let file_id = tracked_files_schema::insert_tracked_file_tx(
        &mut tx,
        watch_folder_id,
        relative_path,
        Some(item.branch.as_str()),
        file_type.as_deref(),
        language.as_deref(),
        &file_mtime,
        &file_hash,
        chunk_count,
        Some("dedup_share"),
        // The clone IS the source generation — carry its fingerprint verbatim
        // (the guard above proved it is current or grandfathered).
        src_chunker_version.as_deref(),
        // lsp_status: chunk-level LSP enrichment is retired; uniformly not-done.
        ProcessingStatus::None,
        // treesitter_status: carry the source's status (see note above) instead
        // of clobbering an already-`done` file back to `none`.
        carried_treesitter_status,
        Some(item.collection.as_str()),
        extension.as_deref(),
        is_test,
        Some(&base_point),
        None,
    )
    .await
    .map_err(|e| UnifiedProcessorError::ProcessingFailed(format!("insert_tracked_file: {e}")))?;

    // No qdrant_chunks copy: insert_tracked_file_tx merged this branch into the
    // EXISTING content-row, whose mirror already references the shared points.
    tx.commit()
        .await
        .map_err(|e| UnifiedProcessorError::ProcessingFailed(format!("dedup tx commit: {e}")))?;

    // ── 4. Enqueue FTS5 work (batch writer owns search.db writes) ──
    if let Some(sender) = crate::search_db::batch_writer::global_sender() {
        // The shared reader (normalised, so this enqueue matches the
        // base_point identity and never re-stores a stale '\r'; redacted, so
        // the clone's FTS5 lines match what its source generation holds).
        match crate::document_processor::redaction::read_for_index(file_path).await {
            Ok((new_content, _redacted_lines)) => {
                let new_hash = compute_content_hash(&new_content);
                let change = dedup_fts_change(
                    &ctx.pool,
                    file_id,
                    &new_content,
                    &item.tenant_id,
                    &item.branch,
                    abs_file_path,
                    &base_point,
                    relative_path,
                    &file_hash,
                )
                .await;
                let work = Fts5WorkItem {
                    change,
                    new_content_bytes: new_content.into_bytes(),
                    new_hash,
                    queue_id: item.queue_id.clone(),
                };
                let _ = ctx
                    .queue_manager
                    .update_destination_status(
                        &item.queue_id,
                        "search",
                        DestinationStatus::InProgress,
                    )
                    .await;
                if let Err(e) = sender.send(work).await {
                    warn!(
                        "branch_dedup: failed to enqueue FTS5 work for {}: {} — marking search=failed",
                        relative_path, e
                    );
                    let _ = ctx
                        .queue_manager
                        .update_destination_status(
                            &item.queue_id,
                            "search",
                            DestinationStatus::Failed,
                        )
                        .await;
                }
            }
            Err(e) => {
                // Binary or unreadable — skip search but qdrant work still
                // counts as done.
                super::fts5_index::log_index_read_skip("branch_dedup", abs_file_path, &e);
                let _ = ctx
                    .queue_manager
                    .update_destination_status(&item.queue_id, "search", DestinationStatus::Done)
                    .await;
            }
        }
    } else {
        // Library/test mode with no batch writer — mark search=done so the
        // orchestration-only path completes.
        let _ = ctx
            .queue_manager
            .update_destination_status(&item.queue_id, "search", DestinationStatus::Done)
            .await;
    }

    // ── 5. Destination markers + return ──
    let _ = ctx
        .queue_manager
        .update_destination_status(&item.queue_id, "qdrant", DestinationStatus::Done)
        .await;

    info!(
        "branch_dedup hit: {} (+= {}) skipped embed, shared {} points at base_point {}",
        relative_path, item.branch, point_count, base_point
    );

    // Suppress unused warnings on payload — kept in the signature to mirror the
    // normal ingest entry-point and ease future field reuse.
    let _ = payload;
    Ok(Some(DedupHit { base_point }))
}

/// The FTS5 change for a dedup hit. `insert_tracked_file_tx` merged the branch
/// into the EXISTING content-row, so `file_id`'s `code_lines` already hold this
/// exact content: the old side must be the cache's claim about those rows. An
/// empty claim against present rows trips the guard in
/// `apply_diff_to_code_lines`, which then rewrites every line of the file.
/// Measured 2026-10-05: each new DOC-V2 worktree branch (~5.2k dedup hits)
/// rewrote ~5.2k files — 15.7k "indexed_content disagrees" rebuilds in one
/// burst. With the cached base the diff is a no-op and the change only adds the
/// branch to `file_metadata.branches`, which is what this step is for.
#[allow(clippy::too_many_arguments)]
async fn dedup_fts_change(
    state_pool: &SqlitePool,
    file_id: i64,
    new_content: &str,
    tenant_id: &str,
    branch: &str,
    abs_file_path: &str,
    base_point: &str,
    relative_path: &str,
    file_hash: &str,
) -> FileChange {
    let old_content = super::fts5_index::cached_diff_base(state_pool, file_id)
        .await
        .map(|(content, _hash)| content)
        .unwrap_or_default();
    FileChange {
        file_id,
        size_bytes: Some(new_content.len() as i64),
        old_content,
        new_content: new_content.to_string(),
        tenant_id: tenant_id.to_string(),
        branch: Some(branch.to_string()),
        file_path: abs_file_path.to_string(),
        base_point: Some(base_point.to_string()),
        relative_path: Some(relative_path.to_string()),
        file_hash: Some(file_hash.to_string()),
    }
}

// (copy_qdrant_chunks removed in Layer 2 stage 2: the content-row is shared, so
// `insert_tracked_file_tx` merges the branch into the existing row whose mirror
// already references the shared points — there is nothing to copy.)

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fts_batch_processor::{FtsBatchConfig, FtsBatchProcessor};
    use crate::indexed_content_schema::{self, CREATE_INDEXED_CONTENT_SQL};
    use crate::search_db::SearchDbManager;
    use sqlx::sqlite::SqlitePoolOptions;

    const CONTENT: &str = "package com.doc.model;\n\nimport java.util.List;\n\nclass A {}\n";

    /// state.db with one tracked content-row; returns the pool and its file_id.
    async fn state_with_row() -> (SqlitePool, i64) {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        for sql in [
            "PRAGMA foreign_keys = ON",
            crate::watch_folders_schema::CREATE_WATCH_FOLDERS_SQL,
            crate::tracked_files_schema::CREATE_TRACKED_FILES_V41_SQL,
            CREATE_INDEXED_CONTENT_SQL,
            "INSERT INTO watch_folders (watch_id, path, collection, tenant_id, created_at, updated_at)
             VALUES ('w1', '/repo', 'projects', 't1', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        ] {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
        let file_id = sqlx::query(
            "INSERT INTO tracked_files (watch_folder_id, relative_path, file_mtime, file_hash, created_at, updated_at)
             VALUES ('w1', 'A.java', '2026-01-01T00:00:00Z', 'h', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap()
        .last_insert_rowid();
        (pool, file_id)
    }

    async fn dedup_change_for(state: &SqlitePool, file_id: i64, branch: &str) -> FileChange {
        dedup_fts_change(
            state,
            file_id,
            CONTENT,
            "t1",
            branch,
            "/repo/A.java",
            "bp",
            "A.java",
            "h",
        )
        .await
    }

    #[tokio::test]
    async fn dedup_share_diffs_against_the_cached_content() {
        // Live 2026-10-05: every dedup hit sent `old_content: ""` for a row whose
        // lines were already indexed, so the guard rewrote the whole file — one
        // full rewrite per file per new worktree branch (15.7k in one burst).
        let (state, file_id) = state_with_row().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let search = SearchDbManager::new(&tmp.path().join("search.db"))
            .await
            .unwrap();
        let mut fts = FtsBatchProcessor::new(&search, FtsBatchConfig::default());

        // The content-row as its first ingest left it: lines + cache claim.
        let mut first = dedup_change_for(&state, file_id, "main").await;
        first.old_content = String::new();
        fts.add_change(first);
        fts.flush_forced_batch().await.unwrap();
        indexed_content_schema::upsert_indexed_content(
            &state,
            file_id,
            CONTENT.as_bytes(),
            &compute_content_hash(CONTENT),
        )
        .await
        .unwrap();

        // Another branch shares the same content (the batch lane, as the
        // batch writer runs it).
        let change = dedup_change_for(&state, file_id, "feat/x").await;
        assert_eq!(
            change.old_content, CONTENT,
            "the diff base is the cached claim"
        );
        fts.add_change(change);
        let stats = fts.flush_forced_batch().await.unwrap();

        assert_eq!(
            (
                stats.lines_inserted,
                stats.lines_deleted,
                stats.lines_updated
            ),
            (0, 0, 0),
            "sharing content another branch indexed must not rewrite its lines"
        );
        let lines: Vec<String> =
            sqlx::query_scalar("SELECT content FROM code_lines WHERE file_id = ?1 ORDER BY seq")
                .bind(file_id)
                .fetch_all(search.pool())
                .await
                .unwrap();
        assert_eq!(lines, CONTENT.split('\n').collect::<Vec<_>>());
        let branches: String =
            sqlx::query_scalar("SELECT branches FROM file_metadata WHERE file_id = ?1")
                .bind(file_id)
                .fetch_one(search.pool())
                .await
                .unwrap();
        let mut branches: Vec<String> = serde_json::from_str(&branches).unwrap();
        branches.sort();
        assert_eq!(
            branches,
            vec!["feat/x", "main"],
            "the step's job: add the branch"
        );
        search.close().await;
    }

    #[tokio::test]
    async fn dedup_share_without_a_cache_entry_claims_nothing() {
        // No cache row = no claim: `""`, which the diff guard turns into a
        // rebuild when the file does have rows (never a diff against a guess).
        let (state, file_id) = state_with_row().await;
        assert_eq!(
            dedup_change_for(&state, file_id, "feat/x")
                .await
                .old_content,
            ""
        );
    }
}
