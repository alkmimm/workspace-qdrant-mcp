//! The FTS5 batch writer's per-item handshake: once a batch has committed (or
//! failed), record each item's search sink and resolve the queue item.
//!
//! The writer is a SECOND finalizer: the processor's success path
//! (`finalize_after_success`) resolves the same items concurrently, and
//! whichever sees both sinks done first deletes the row. The sink update and
//! the resolution therefore run in one transaction
//! ([`QueueManager::resolve_destination`]), and a row that is already gone is
//! an outcome, not an error.

use tracing::{debug, error};

use crate::queue_operations::QueueManager;
use crate::unified_queue_schema::{DestinationStatus, QueueStatus};

/// Error message recorded when an FTS5 batch reports a per-item failure.
///
/// The `[transient_fts5]` prefix is load-bearing: it makes the item eligible
/// for the idle resurrection pass (`QueueManager::resurrect_failed_transient`,
/// which selects `WHERE error_message LIKE '[transient_%'`). FTS5 batch
/// failures are typically transient — historically `SQLITE_BUSY` write-lock
/// contention (see the batch writer's module doc), or a poisoned sibling in
/// the same batch. Resurrection bounds retries via `max_resurrections`, then
/// promotes the row to `[permanent_exhausted]`. WITHOUT the prefix the item is
/// neither resurrected nor triaged and sits in `failed` forever.
pub(super) fn fts5_failure_message(queue_id: &str) -> String {
    format!("[transient_fts5] FTS5 batch reported search_status=failed for queue_id={queue_id}")
}

/// What resolving one item's search sink did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Resolved {
    /// Both sinks done: the item left the queue.
    Completed,
    /// A sink failed: the item went to the retry path.
    Failed,
    /// The Qdrant sink is still pending: the processor resolves the item.
    Waiting,
    /// The row was already gone (a cancel or prune removed the item).
    Gone,
    /// The queue database refused; the item is left as it was (logged).
    Error,
}

/// Record the search sink's `status` for `queue_id` and act on the item's
/// resolution: delete on Done, hand to the retry path on Failed, leave it to
/// the processor otherwise. Mirrors `handle_item_success` in
/// batch_processing.rs.
pub(super) async fn resolve_search(
    queue_manager: &QueueManager,
    queue_id: &str,
    status: DestinationStatus,
) -> Resolved {
    let overall = match queue_manager
        .resolve_destination(queue_id, "search", status)
        .await
    {
        Ok(Some(overall)) => overall,
        Ok(None) => {
            debug!(
                "queue_id={queue_id} was removed before its FTS5 batch resolved it \
                 (search={status}) — nothing to finalize"
            );
            return Resolved::Gone;
        }
        Err(e) => {
            error!(
                "resolving search={status} failed for queue_id={queue_id}: {e} — item left \
                 in current state"
            );
            return Resolved::Error;
        }
    };
    match overall {
        QueueStatus::Done => {
            // The processor may see the resolved row and delete it first;
            // `delete_unified_item` reports that as `Ok(false)`, and only the
            // deleter runs the post-completion side effects.
            if let Err(e) = queue_manager.delete_unified_item(queue_id).await {
                error!("delete_unified_item failed for queue_id={queue_id}: {e}");
                return Resolved::Error;
            }
            Resolved::Completed
        }
        QueueStatus::Failed => {
            // mark_unified_failed handles retry vs permanent. max_retries is
            // hardcoded to 3 here because the actor doesn't carry the
            // processor config; the wider queue cfg uses the same default
            // (see UnifiedProcessorConfig).
            //
            // Residual window: a processor finalize landing between the
            // resolution above and this call also sees Failed and marks the
            // item too (one extra retry_count). Closing it needs the retry
            // bookkeeping inside the resolving transaction.
            let err_msg = fts5_failure_message(queue_id);
            if let Err(e) = queue_manager
                .mark_unified_failed(queue_id, &err_msg, false, 3)
                .await
            {
                error!("mark_unified_failed for {queue_id} after FTS5 batch error: {e}");
                return Resolved::Error;
            }
            Resolved::Failed
        }
        QueueStatus::InProgress | QueueStatus::Pending => {
            debug!("queue_id={queue_id} still in_progress after FTS5 batch (qdrant pending)");
            Resolved::Waiting
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unified_queue_schema::{ItemType, QueueOperation};
    use sqlx::SqlitePool;
    use wqm_common::queue_types::QueueDecision;

    /// The FTS5 failure message MUST carry a `[transient_` prefix so the
    /// idle resurrection pass (`resurrect_failed_transient`, which matches
    /// `error_message LIKE '[transient_%'`) re-queues it. Regression guard:
    /// dropping the prefix would silently strand failed items in `failed`
    /// forever (the bug that left 346 SQLITE_BUSY items dead).
    #[test]
    fn fts5_failure_message_is_classified_transient() {
        let msg = fts5_failure_message("abc123");
        assert!(
            msg.starts_with("[transient_"),
            "must match resurrection's LIKE '[transient_%' pattern, got: {msg}"
        );
        assert!(
            msg.contains("abc123"),
            "must embed the queue_id, got: {msg}"
        );
    }

    /// A file item shaped like the ingest handlers leave it when they hand
    /// FTS5 work to the batch writer: a state-machine item (decision stored),
    /// search sink `in_progress`, Qdrant sink as given. The temp dir holds
    /// the database: keep it alive for the test.
    async fn handed_to_writer(
        qdrant: DestinationStatus,
    ) -> (tempfile::TempDir, QueueManager, SqlitePool, String) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("batch_finalize.db");
        let pool = SqlitePool::connect(&format!("sqlite://{}?mode=rwc", db_path.display()))
            .await
            .unwrap();
        let manager = QueueManager::new(pool.clone());
        manager.init_unified_queue().await.unwrap();
        sqlx::query(crate::watch_folders_schema::CREATE_WATCH_FOLDERS_SQL)
            .execute(&pool)
            .await
            .unwrap();
        let (queue_id, _) = manager
            .enqueue_unified(
                ItemType::File,
                QueueOperation::Add,
                "tenant-fts",
                "projects",
                r#"{"file_path":"src/lib.rs","file_type":"code"}"#,
                Some("main"),
                None,
            )
            .await
            .unwrap();
        let decision = QueueDecision {
            delete_old: false,
            old_base_point: None,
            new_base_point: "bp".to_string(),
            old_file_hash: None,
            new_file_hash: "hash".to_string(),
        };
        manager
            .store_queue_decision(&queue_id, &decision)
            .await
            .unwrap();
        for (sink, status) in [
            ("search", DestinationStatus::InProgress),
            ("qdrant", qdrant),
        ] {
            manager
                .update_destination_status(&queue_id, sink, status)
                .await
                .unwrap();
        }
        (dir, manager, pool, queue_id)
    }

    async fn row_count(pool: &SqlitePool, queue_id: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM unified_queue WHERE queue_id = ?1")
            .bind(queue_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// The 2026-10-07 race: the processor's success path resolves the item
    /// around the writer's handshake. Whichever finalizer runs first, the
    /// item leaves the queue exactly once and neither side errors.
    #[tokio::test]
    async fn writer_and_processor_finalize_the_same_item_in_either_order() {
        // Writer first: it completes the item; the processor then finds no
        // row, which it treats as done, and its delete is a no-op.
        let (_dir, manager, pool, id) = handed_to_writer(DestinationStatus::Done).await;
        assert_eq!(
            resolve_search(&manager, &id, DestinationStatus::Done).await,
            Resolved::Completed
        );
        assert_eq!(row_count(&pool, &id).await, 0);
        assert!(manager.finalize_after_success(&id).await.is_err());
        assert!(!manager.delete_unified_item(&id).await.unwrap());

        // Processor first: the search sink is still in_progress, so it keeps
        // the row; the writer then completes it.
        let (_dir, manager, pool, id) = handed_to_writer(DestinationStatus::Done).await;
        assert_eq!(
            manager.finalize_after_success(&id).await.unwrap(),
            QueueStatus::InProgress
        );
        assert_eq!(
            resolve_search(&manager, &id, DestinationStatus::Done).await,
            Resolved::Completed
        );
        assert_eq!(row_count(&pool, &id).await, 0);
    }

    /// A row removed before the writer resolves it (a cancel, a prune) is an
    /// outcome, not an error — and nothing is written for it.
    #[tokio::test]
    async fn an_item_already_gone_is_not_an_error() {
        let (_dir, manager, pool, id) = handed_to_writer(DestinationStatus::Done).await;
        assert!(manager.delete_unified_item(&id).await.unwrap());
        assert_eq!(
            resolve_search(&manager, &id, DestinationStatus::Done).await,
            Resolved::Gone
        );
        assert_eq!(
            resolve_search(&manager, &id, DestinationStatus::Failed).await,
            Resolved::Gone
        );
        assert_eq!(row_count(&pool, &id).await, 0);
    }

    /// Qdrant still pending: the writer records its sink and leaves the item
    /// to the processor.
    #[tokio::test]
    async fn the_writer_waits_for_a_pending_qdrant_sink() {
        let (_dir, manager, pool, id) = handed_to_writer(DestinationStatus::InProgress).await;
        assert_eq!(
            resolve_search(&manager, &id, DestinationStatus::Done).await,
            Resolved::Waiting
        );
        assert_eq!(row_count(&pool, &id).await, 1);
        let (_, search) = manager.read_destination_statuses(&id).await.unwrap();
        assert_eq!(search.as_deref(), Some("done"));
    }

    /// A failed batch sends the item to the retry path with the transient
    /// message the resurrection pass looks for.
    #[tokio::test]
    async fn a_failed_batch_sends_the_item_to_retry() {
        let (_dir, manager, pool, id) = handed_to_writer(DestinationStatus::Done).await;
        assert_eq!(
            resolve_search(&manager, &id, DestinationStatus::Failed).await,
            Resolved::Failed
        );
        let (status, retries, message): (String, i32, Option<String>) = sqlx::query_as(
            "SELECT status, retry_count, error_message FROM unified_queue WHERE queue_id = ?1",
        )
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((status.as_str(), retries), ("pending", 1));
        assert!(message.unwrap().starts_with("[transient_fts5]"));
    }
}
