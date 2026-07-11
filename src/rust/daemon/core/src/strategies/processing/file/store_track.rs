//! Qdrant upsert and tracked_files/qdrant_chunks SQLite transaction.
//!
//! After chunk embedding and keyword extraction, this module handles the
//! atomic persistence: batch upsert to Qdrant, then tracked_files +
//! qdrant_chunks insert/update within a single SQLite transaction.

use std::path::Path;

use sqlx::SqlitePool;
use tracing::{debug, info, warn};

use crate::context::ProcessingContext;
use crate::file_classification::{get_extension_for_storage, is_test_file};
use crate::storage::DocumentPoint;
use crate::tracked_files_schema::{self, ProcessingStatus, TrackedFile};
use crate::unified_queue_processor::UnifiedProcessorError;
use crate::unified_queue_schema::{DestinationStatus, UnifiedQueueItem};
use crate::DocumentContent;

use super::chunk_embed::ChunkRecord;
use super::delete;

/// Chunk tuple format expected by `insert_qdrant_chunks_tx`.
type ChunkTuple = (
    String,
    i32,
    String,
    Option<tracked_files_schema::ChunkType>,
    Option<String>,
    Option<i32>,
    Option<i32>,
);

/// Upsert points to Qdrant and record in tracked_files + qdrant_chunks atomically.
///
/// Returns the `file_id` from `tracked_files` on success.
#[allow(clippy::too_many_arguments)]
pub(super) async fn upsert_and_track(
    ctx: &ProcessingContext,
    item: &UnifiedQueueItem,
    pool: &SqlitePool,
    points: Vec<DocumentPoint>,
    chunk_records: &[ChunkRecord],
    watch_folder_id: &str,
    relative_path: &str,
    base_point: &str,
    file_hash: &str,
    file_path: &Path,
    document_content: &DocumentContent,
    lsp_status: ProcessingStatus,
    treesitter_status: ProcessingStatus,
    chunker_version: &str,
    payload_file_type: Option<&str>,
    component: Option<String>,
) -> Result<i64, UnifiedProcessorError> {
    // #224: snapshot the branch set that OTHER branches tagged on this shared
    // base_point BEFORE the upsert overwrites each point's `branch` payload with
    // only the current branch, then restore it right after so Qdrant keeps the
    // union. Sourced from the UNION of what Qdrant currently holds AND the
    // `tracked_files` authority for this base_point: reading Qdrant alone only
    // prevents NEW drift — a base_point whose payload had already fallen behind
    // the authority (e.g. lost `main`) would preserve its own drift forever. The
    // authority leg makes a re-ingest re-SYNC Qdrant up to the source of truth
    // (so it self-heals, and a reembed restores `main` regardless of which branch
    // it runs on). Additive: only branches the authority holds are added, none
    // removed. Both legs are cheap (one indexed scroll + one indexed SQLite
    // query, both empty for a brand-new base_point). Skipped when no points.
    let prior_branches = if points.is_empty() {
        Vec::new()
    } else {
        let mut set = ctx
            .storage_client
            .read_branch_set(&item.collection, base_point)
            .await
            .unwrap_or_default();
        match read_authority_branches(pool, base_point).await {
            Ok(authority) => {
                for b in authority {
                    if !set.contains(&b) {
                        set.push(b);
                    }
                }
            }
            Err(e) => warn!(
                "#224: authority branch read failed for base_point {}: {} — preserve falls back to Qdrant tags only",
                base_point, e
            ),
        }
        set
    };

    upsert_to_qdrant(
        ctx,
        item,
        pool,
        points,
        chunk_records,
        watch_folder_id,
        relative_path,
    )
    .await?;

    // Restore the branches the upsert dropped (everything but the current branch,
    // which the upsert already wrote as the sole tag). Best-effort: a failure
    // leaves the shared point tagged only with the current branch until the next
    // reconcile/reembed, never blocking ingestion.
    let to_restore: Vec<String> = prior_branches
        .into_iter()
        .filter(|b| b != &item.branch)
        .collect();
    if !to_restore.is_empty() {
        if let Err(e) = ctx
            .storage_client
            .merge_branches_into_base_point(&item.collection, base_point, &to_restore)
            .await
        {
            warn!(
                "branch-preserve merge failed for {} (base_point {}): {} — Qdrant may drop prior branches until reconcile",
                relative_path, base_point, e
            );
        }
    }

    let existing = tracked_files_schema::lookup_tracked_file(
        pool,
        watch_folder_id,
        relative_path,
        Some(item.branch.as_str()),
    )
    .await
    .map_err(|e| {
        UnifiedProcessorError::QueueOperation(format!("tracked_files lookup failed: {}", e))
    })?;

    let chunk_tuples = build_chunk_tuples(chunk_records);

    let tx_result = run_tracking_transaction(
        pool,
        item,
        &existing,
        &chunk_tuples,
        chunk_records.len(),
        watch_folder_id,
        relative_path,
        base_point,
        file_hash,
        file_path,
        document_content,
        lsp_status,
        treesitter_status,
        chunker_version,
        payload_file_type,
        component.as_deref(),
    )
    .await;

    // Handle transaction failure: Qdrant has points but SQLite state is inconsistent.
    if let Err(ref e) = tx_result {
        warn!(
            "SQLite transaction failed after Qdrant upsert for {}: {}. Queue item will be retried.",
            relative_path, e
        );
        if let Some(existing_file) = &existing {
            let _ = tracked_files_schema::mark_needs_reconcile(
                pool,
                existing_file.file_id,
                &format!("ingest_tx_failed: {}", e),
            )
            .await;
        }
    }

    tx_result
}

/// Union of every branch the `tracked_files` authority holds for `base_point`,
/// across all its rows (Layer 2 shares one base_point across branches and clones,
/// so the full membership set is spread over several rows). This is the source of
/// truth the shared Qdrant point's `branch` array must match — used to restore
/// tags the current-branch full-ingest upsert overwrites, INCLUDING any the
/// Qdrant payload had already drifted below the authority (#224). Uses the
/// `idx_tracked_files_bp` index; `json_each` iterates each row's branches array
/// (same idiom as `branch_held_by_other`).
async fn read_authority_branches(
    pool: &SqlitePool,
    base_point: &str,
) -> Result<Vec<String>, sqlx::Error> {
    let rows: Vec<(String,)> = sqlx::query_as(
        r#"
        SELECT DISTINCT je.value
        FROM tracked_files AS tf, json_each(tf.branches) AS je
        WHERE tf.base_point = ?1
        "#,
    )
    .bind(base_point)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(b,)| b).collect())
}

/// Upsert document points to Qdrant. On failure, cleans up stale SQLite state
/// and returns an error.
async fn upsert_to_qdrant(
    ctx: &ProcessingContext,
    item: &UnifiedQueueItem,
    pool: &SqlitePool,
    points: Vec<DocumentPoint>,
    chunk_records: &[ChunkRecord],
    watch_folder_id: &str,
    relative_path: &str,
) -> Result<(), UnifiedProcessorError> {
    if points.is_empty() {
        return Ok(());
    }

    info!("Inserting {} points into {}", points.len(), item.collection);
    let upsert_start = std::time::Instant::now();
    match ctx
        .storage_client
        .insert_points_batch(&item.collection, points, Some(100))
        .await
    {
        Ok(_stats) => {
            info!(
                "Qdrant upsert completed: {} points in {}ms",
                chunk_records.len(),
                upsert_start.elapsed().as_millis()
            );
            Ok(())
        }
        Err(e) => {
            // Task 555: clean up stale SQLite chunk records before propagating the error
            let qdrant_err = e.to_string();
            delete::handle_qdrant_failure(
                ctx,
                item,
                pool,
                watch_folder_id,
                relative_path,
                &qdrant_err,
            )
            .await;
            let _ = ctx
                .queue_manager
                .update_destination_status(&item.queue_id, "qdrant", DestinationStatus::Failed)
                .await;
            Err(UnifiedProcessorError::Storage(qdrant_err))
        }
    }
}

/// Convert `ChunkRecord` slice to the tuple format expected by `insert_qdrant_chunks_tx`.
fn build_chunk_tuples(chunk_records: &[ChunkRecord]) -> Vec<ChunkTuple> {
    chunk_records
        .iter()
        .map(|cr| {
            (
                cr.point_id.clone(),
                cr.chunk_index,
                cr.content_hash.clone(),
                cr.chunk_type,
                cr.symbol_name.clone(),
                cr.start_line,
                cr.end_line,
            )
        })
        .collect()
}

struct FileTrackMeta<'a> {
    file_mtime: String,
    language: Option<String>,
    chunking_method: Option<&'a str>,
    extension: Option<String>,
    is_test: bool,
}

fn build_file_track_meta<'a>(
    file_path: &Path,
    document_content: &DocumentContent,
    treesitter_status: ProcessingStatus,
) -> FileTrackMeta<'a> {
    let file_mtime = tracked_files_schema::get_file_mtime(file_path)
        .unwrap_or_else(|_| wqm_common::timestamps::now_utc());
    let language = document_content.metadata.get("language").cloned();
    let chunking_method = if treesitter_status == ProcessingStatus::Done {
        Some("tree_sitter")
    } else {
        Some("text")
    };
    let extension = get_extension_for_storage(file_path);
    let is_test = is_test_file(file_path);
    FileTrackMeta {
        file_mtime,
        language,
        chunking_method,
        extension,
        is_test,
    }
}

/// Execute the SQLite transaction that records tracked_files + qdrant_chunks.
///
/// Returns the `file_id` assigned to this file.
#[allow(clippy::too_many_arguments)]
async fn run_tracking_transaction(
    pool: &SqlitePool,
    item: &UnifiedQueueItem,
    existing: &Option<TrackedFile>,
    chunk_tuples: &[ChunkTuple],
    chunk_count: usize,
    watch_folder_id: &str,
    relative_path: &str,
    base_point: &str,
    file_hash: &str,
    file_path: &Path,
    document_content: &DocumentContent,
    lsp_status: ProcessingStatus,
    treesitter_status: ProcessingStatus,
    chunker_version: &str,
    payload_file_type: Option<&str>,
    component: Option<&str>,
) -> Result<i64, UnifiedProcessorError> {
    let meta = build_file_track_meta(file_path, document_content, treesitter_status);

    // BEGIN IMMEDIATE (see db_retry): takes the SQLite write lock up front so the
    // read→write body can't hit SQLITE_BUSY_SNAPSHOT (517), and retries the BEGIN
    // on transient busy/locked with jittered backoff.
    let mut tx = crate::db_retry::begin_immediate(pool).await.map_err(|e| {
        UnifiedProcessorError::QueueOperation(format!("Failed to begin transaction: {}", e))
    })?;

    let file_id = match existing {
        Some(existing_file) => {
            upsert_existing_tracked_file(
                &mut tx,
                existing_file,
                &meta.file_mtime,
                file_hash,
                chunk_count,
                meta.chunking_method,
                chunker_version,
                lsp_status,
                treesitter_status,
                base_point,
                component,
            )
            .await?
        }
        None => {
            insert_new_tracked_file(
                &mut tx,
                item,
                watch_folder_id,
                relative_path,
                payload_file_type,
                meta.language.as_deref(),
                &meta.file_mtime,
                file_hash,
                chunk_count,
                meta.chunking_method,
                chunker_version,
                lsp_status,
                treesitter_status,
                meta.extension.as_deref(),
                meta.is_test,
                base_point,
                component,
            )
            .await?
        }
    };

    // The Qdrant upsert beside this mirror write is idempotent (identical
    // base_point ⇒ identical point IDs that overwrite), so the mirror must be
    // too. `file_id` can already carry chunk rows — an in-place update OR a
    // Layer 2 upsert-merge that returned an EXISTING content-row's file_id — so
    // clear them first; otherwise the re-insert collides on
    // UNIQUE(file_id, chunk_index). A harmless no-op for a brand-new file_id.
    tracked_files_schema::delete_qdrant_chunks_tx(&mut tx, file_id)
        .await
        .map_err(|e| {
            UnifiedProcessorError::QueueOperation(format!("qdrant_chunks delete failed: {}", e))
        })?;

    if !chunk_tuples.is_empty() {
        tracked_files_schema::insert_qdrant_chunks_tx(&mut tx, file_id, chunk_tuples)
            .await
            .map_err(|e| {
                UnifiedProcessorError::QueueOperation(format!("qdrant_chunks insert failed: {}", e))
            })?;
    }

    tx.commit().await.map_err(|e| {
        UnifiedProcessorError::QueueOperation(format!("Transaction commit failed: {}", e))
    })?;

    debug!(
        "Recorded {} chunks in tracked_files for file_id={} ({})",
        chunk_count, file_id, relative_path
    );
    Ok(file_id)
}

/// Update an existing `tracked_files` row within `tx`. The chunk mirror is
/// cleared unconditionally by the caller right before the re-insert, so this
/// does not touch `qdrant_chunks`.
#[allow(clippy::too_many_arguments)]
async fn upsert_existing_tracked_file(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    existing_file: &TrackedFile,
    file_mtime: &str,
    file_hash: &str,
    chunk_count: usize,
    chunking_method: Option<&str>,
    chunker_version: &str,
    lsp_status: ProcessingStatus,
    treesitter_status: ProcessingStatus,
    base_point: &str,
    component: Option<&str>,
) -> Result<i64, UnifiedProcessorError> {
    tracked_files_schema::update_tracked_file_tx(
        tx,
        existing_file.file_id,
        file_mtime,
        file_hash,
        chunk_count as i32,
        chunking_method,
        Some(chunker_version),
        lsp_status,
        treesitter_status,
        Some(base_point),
        component,
    )
    .await
    .map_err(|e| {
        UnifiedProcessorError::QueueOperation(format!("tracked_files update failed: {}", e))
    })?;

    Ok(existing_file.file_id)
}

/// Insert a new `tracked_files` row within `tx`.
#[allow(clippy::too_many_arguments)]
async fn insert_new_tracked_file(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    item: &UnifiedQueueItem,
    watch_folder_id: &str,
    relative_path: &str,
    payload_file_type: Option<&str>,
    language: Option<&str>,
    file_mtime: &str,
    file_hash: &str,
    chunk_count: usize,
    chunking_method: Option<&str>,
    chunker_version: &str,
    lsp_status: ProcessingStatus,
    treesitter_status: ProcessingStatus,
    extension: Option<&str>,
    is_test: bool,
    base_point: &str,
    component: Option<&str>,
) -> Result<i64, UnifiedProcessorError> {
    tracked_files_schema::insert_tracked_file_tx(
        tx,
        watch_folder_id,
        relative_path,
        Some(item.branch.as_str()),
        payload_file_type,
        language,
        file_mtime,
        file_hash,
        chunk_count as i32,
        chunking_method,
        Some(chunker_version),
        lsp_status,
        treesitter_status,
        Some(&item.collection),
        extension,
        is_test,
        Some(base_point),
        component,
    )
    .await
    .map_err(|e| {
        UnifiedProcessorError::QueueOperation(format!("tracked_files insert failed: {}", e))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn mem_pool() -> SqlitePool {
        // Single connection so every query hits the same in-memory database.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open in-memory sqlite");
        sqlx::query(
            "CREATE TABLE tracked_files (file_id INTEGER PRIMARY KEY, base_point TEXT, branches TEXT)",
        )
        .execute(&pool)
        .await
        .expect("create tracked_files");
        pool
    }

    #[tokio::test]
    async fn read_authority_branches_unions_across_rows_for_a_base_point() {
        let pool = mem_pool().await;
        for (bp, branches) in [
            ("bp1", r#"["main","featA"]"#),
            ("bp1", r#"["featB","main"]"#), // another row (branch/clone) shares bp1
            ("bp2", r#"["dev"]"#),          // unrelated base_point
        ] {
            sqlx::query("INSERT INTO tracked_files (base_point, branches) VALUES (?1, ?2)")
                .bind(bp)
                .bind(branches)
                .execute(&pool)
                .await
                .expect("insert row");
        }

        // Deduplicated union of every bp1 row's branches — this is the authority
        // set the Qdrant `branch` array must be restored up to (#224).
        let mut got = read_authority_branches(&pool, "bp1").await.expect("read bp1");
        got.sort();
        assert_eq!(
            got,
            vec!["featA".to_string(), "featB".to_string(), "main".to_string()]
        );

        // A base_point with no rows → empty (brand-new file, the common case).
        assert!(read_authority_branches(&pool, "bp_absent")
            .await
            .expect("read absent")
            .is_empty());
    }
}
