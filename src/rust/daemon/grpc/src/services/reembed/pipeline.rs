use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tonic::Status;
use tracing::info;
use workspace_qdrant_core::SearchDbManager;
use uuid::Uuid;

use crate::proto::TriggerReembedResponse;

use super::context::{ReembedContext, CANONICAL_COLLECTIONS};
use super::enqueue::{enqueue_folder_scans, enqueue_rules_mirror, enqueue_scratchpad_mirror};
use super::recreator::{collection_reembed_idempotency_key, CollectionRecreator};

/// Pause the queue and poll until all in-progress items complete or timeout.
///
/// Sets `ctx.pause_flag` to `true` on entry. Clears it (back to `false`) on
/// both timeout and internal query error before returning `Err`.
async fn drain_to_quiescence(
    ctx: &ReembedContext,
    timeout: Duration,
    poll_interval: Duration,
) -> Result<(), Status> {
    let drain_started = Instant::now();
    loop {
        let in_flight: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM unified_queue \
             WHERE status = 'in_progress'",
        )
        .fetch_one(&ctx.pool)
        .await
        .map_err(|e| {
            ctx.pause_flag.store(false, Ordering::SeqCst);
            Status::internal(format!("drain query failed: {e}"))
        })?;

        if in_flight == 0 {
            return Ok(());
        }

        if drain_started.elapsed() >= timeout {
            ctx.pause_flag.store(false, Ordering::SeqCst);
            return Err(Status::failed_precondition(format!(
                "drain-to-quiescence timeout: {} items still in_progress after {}s; pause flag released",
                in_flight,
                timeout.as_secs()
            )));
        }

        tokio::time::sleep(poll_interval).await;
    }
}


async fn clear_search_db_for_watch_tenants(
    search_db: &Arc<SearchDbManager>,
    pool: &sqlx::SqlitePool,
) -> Result<(), Status> {
    let tenants: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT tenant_id FROM watch_folders \
         WHERE collection IN ('projects','libraries','rules','scratchpad')",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| Status::internal(format!("list search-db tenants for reembed: {e}")))?;

    if tenants.is_empty() {
        return Ok(());
    }

    let search_pool = search_db.pool();
    let mut total_lines = 0u64;
    let mut total_files = 0u64;

    for chunk in tenants.chunks(500) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        // `file_metadata` has no line-count column (see `code_lines_schema`), so
        // the line total is counted from `code_lines` itself — before the DELETEs
        // below remove those rows. Selecting a nonexistent
        // `file_metadata.line_count` here failed the whole function with
        // Status::internal, so the search.db clear and the FTS rebuild it gates
        // never ran: a reembed reported success while leaving the text index
        // stale, which also made it useless as a repair lever.
        let sql = format!("SELECT COUNT(*) FROM file_metadata WHERE tenant_id IN ({placeholders})");
        let mut q = sqlx::query_scalar::<_, i64>(&sql);
        for tenant in chunk {
            q = q.bind(tenant);
        }
        let files = q
            .fetch_one(search_pool)
            .await
            .map_err(|e| Status::internal(format!("count search-db files for reembed: {e}")))?;

        let sql = format!(
            "SELECT COUNT(*) FROM code_lines WHERE file_id IN \
             (SELECT file_id FROM file_metadata WHERE tenant_id IN ({placeholders}))"
        );
        let mut q = sqlx::query_scalar::<_, i64>(&sql);
        for tenant in chunk {
            q = q.bind(tenant);
        }
        let lines = q
            .fetch_one(search_pool)
            .await
            .map_err(|e| Status::internal(format!("count search-db lines for reembed: {e}")))?;

        total_files += files as u64;
        total_lines += lines as u64;

        let sql = format!(
            "DELETE FROM code_lines WHERE file_id IN \
             (SELECT file_id FROM file_metadata WHERE tenant_id IN ({placeholders}))"
        );
        let mut q = sqlx::query(&sql);
        for tenant in chunk {
            q = q.bind(tenant);
        }
        q.execute(search_pool)
            .await
            .map_err(|e| Status::internal(format!("clear search-db code_lines for reembed: {e}")))?;

        let sql = format!("DELETE FROM file_metadata WHERE tenant_id IN ({placeholders})");
        let mut q = sqlx::query(&sql);
        for tenant in chunk {
            q = q.bind(tenant);
        }
        q.execute(search_pool)
            .await
            .map_err(|e| Status::internal(format!("clear search-db file_metadata for reembed: {e}")))?;
    }

    if total_files > 0 {
        search_db
            .rebuild_and_maybe_optimize_fts(total_lines as usize)
            .await
            .map_err(|e| Status::internal(format!("rebuild search-db FTS after reembed clear: {e}")))?;
        info!(
            tenants = tenants.len(),
            files = total_files,
            lines = total_lines,
            "reembed: cleared search.db content for watch-folder tenants"
        );
    }

    Ok(())
}

/// Flush ALL queue rows for the canonical collections and clear
/// vector-derived SQLite state.
///
/// Deletes every `unified_queue` row (not just `pending`) for the four
/// canonical collections. The re-embed re-enqueues a fresh `File/Add` for every
/// watched file, and `enqueue_unified` dedups on `idempotency_key` via
/// `INSERT OR IGNORE`; leaving completed (`done`) rows behind silently dropped
/// those re-enqueues, so files were never re-ingested and a re-embed could
/// "complete" without rebuilding the index or code graph. Clearing the `done`
/// rows too lets the re-enqueue actually take effect.
///
/// Returns the number of rows deleted for logging.
pub(super) async fn flush_and_clear_state(ctx: &ReembedContext) -> Result<u32, Status> {
    let stale_deleted = sqlx::query(
        "DELETE FROM unified_queue \
         WHERE collection IN ('projects','libraries','rules','scratchpad')",
    )
    .execute(&ctx.pool)
    .await
    .map_err(|e| Status::internal(format!("flush queue rows failed: {e}")))?;
    info!(
        rows = stale_deleted.rows_affected(),
        "reembed: flushed all canonical-collection queue rows (incl. done, to clear idempotency dedup)"
    );

    let mut tx = ctx
        .pool
        .begin()
        .await
        .map_err(|e| Status::internal(format!("clear-state tx begin failed: {e}")))?;
    sqlx::query("DELETE FROM tag_hierarchy_edges")
        .execute(&mut *tx)
        .await
        .map_err(|e| Status::internal(format!("clear tag_hierarchy_edges: {e}")))?;
    sqlx::query("DELETE FROM canonical_tags")
        .execute(&mut *tx)
        .await
        .map_err(|e| Status::internal(format!("clear canonical_tags: {e}")))?;
    // Clear per-file content-hash tracking for the canonical collections.
    // Recreating the Qdrant collections empties them, but `prepare_update`
    // skips re-ingesting a file whose `tracked_files.file_hash` still matches
    // ("File unchanged (hash match), skipping update") — so without this the
    // re-enqueued scans would no-op against the now-empty collections and the
    // index/graph would never repopulate. Delete the dependent `qdrant_chunks`
    // first (don't rely on the FK CASCADE pragma being enabled on this pool),
    // then the `tracked_files` rows; re-ingestion then re-tracks every file.
    let chunks_cleared = sqlx::query(
        "DELETE FROM qdrant_chunks WHERE file_id IN \
         (SELECT file_id FROM tracked_files \
          WHERE collection IN ('projects','libraries','rules','scratchpad'))",
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| Status::internal(format!("clear qdrant_chunks: {e}")))?;
    let tracked_cleared = sqlx::query(
        "DELETE FROM tracked_files \
         WHERE collection IN ('projects','libraries','rules','scratchpad')",
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| Status::internal(format!("clear tracked_files: {e}")))?;
    tx.commit()
        .await
        .map_err(|e| Status::internal(format!("clear-state tx commit failed: {e}")))?;

    if let Some(search_db) = &ctx.search_db {
        clear_search_db_for_watch_tenants(search_db, &ctx.pool).await?;
    }

    info!(
        tracked_files = tracked_cleared.rows_affected(),
        qdrant_chunks = chunks_cleared.rows_affected(),
        "reembed: cleared per-file hash tracking so re-ingestion repopulates"
    );

    Ok(stale_deleted.rows_affected() as u32)
}

/// Execute the full reembed flow.
///
/// Returns the populated [`TriggerReembedResponse`] on success or a
/// pre-mapped `tonic::Status` describing the failure mode (typically
/// `failed_precondition` for dim mismatch / drain timeout).
pub async fn execute_reembed<R: CollectionRecreator + ?Sized>(
    ctx: &ReembedContext,
    recreator: &R,
    drain_timeout: Duration,
    poll_interval: Duration,
) -> Result<TriggerReembedResponse, Status> {
    // ── 1. Pre-flight dim check ──────────────────────────────────────────
    let cfg_dim = ctx.settings.output_dim;
    let provider_dim = ctx.provider.output_dim();
    if cfg_dim != provider_dim {
        return Err(Status::failed_precondition(format!(
            "provider output_dim mismatch: settings.output_dim={} but provider.output_dim()={}",
            cfg_dim, provider_dim
        )));
    }

    // ── 2–3. Pause flag + drain to quiescence ───────────────────────────
    ctx.pause_flag.store(true, Ordering::SeqCst);
    info!("reembed: pause flag set; awaiting queue quiescence");
    drain_to_quiescence(ctx, drain_timeout, poll_interval).await?;

    // ── 4–5. Flush stale pending + clear vector-derived state ───────────
    flush_and_clear_state(ctx).await?;

    // ── 6. Recreate the four canonical collections at settings.output_dim
    //      while workers are still paused ────────────────────────────────
    let recreate_dim = cfg_dim as u64;
    for name in CANONICAL_COLLECTIONS {
        recreator.recreate(name, recreate_dim).await?;
        info!(
            collection = name,
            dim = recreate_dim,
            "reembed: recreated canonical collection (dropped + created)"
        );
    }

    // ── 7. Enqueue 4 collection-reembed traceability items ───────────────
    let now = wqm_common::timestamps::now_utc();
    for collection in CANONICAL_COLLECTIONS {
        let queue_id = Uuid::new_v4().to_string();
        let idem_key = collection_reembed_idempotency_key(collection);
        sqlx::query(
            "INSERT OR IGNORE INTO unified_queue \
             (queue_id, idempotency_key, item_type, op, tenant_id, collection, \
              status, payload_json, created_at, updated_at) \
             VALUES (?1, ?2, 'collection', 'reembed', '_system', ?3, 'pending', '{}', ?4, ?5)",
        )
        .bind(&queue_id)
        .bind(&idem_key)
        .bind(collection)
        .bind(&now)
        .bind(&now)
        .execute(&ctx.pool)
        .await
        .map_err(|e| Status::internal(format!("enqueue reembed/{collection}: {e}")))?;
    }

    // ── 8. Re-enqueue from watch_folders, rules_mirror, scratchpad_mirror
    let files_enqueued = enqueue_folder_scans(&ctx.pool, &now)
        .await
        .map_err(|e| Status::internal(format!("re-enqueue folder scans: {e}")))?;
    let rules_enqueued = enqueue_rules_mirror(&ctx.pool, &now)
        .await
        .map_err(|e| Status::internal(format!("re-enqueue rules_mirror: {e}")))?;
    let scratchpad_enqueued = enqueue_scratchpad_mirror(&ctx.pool, &now)
        .await
        .map_err(|e| Status::internal(format!("re-enqueue scratchpad_mirror: {e}")))?;

    // ── 9. Resume queue workers ──────────────────────────────────────────
    ctx.pause_flag.store(false, Ordering::SeqCst);
    info!(
        files = files_enqueued,
        rules = rules_enqueued,
        scratchpad = scratchpad_enqueued,
        "reembed: complete; pause flag cleared"
    );

    Ok(TriggerReembedResponse {
        files_enqueued,
        rules_enqueued,
        scratchpad_enqueued,
        message: format!(
            "reembed complete at output_dim={cfg_dim}: {files_enqueued} files, \
             {rules_enqueued} rules, {scratchpad_enqueued} scratchpad items re-enqueued"
        ),
    })
}
