//! `resolve_destination`: one sink's status and the item's resolution in one
//! transaction — the finalize step of a finalizer outside the handler (the
//! FTS5 batch writer).

use super::*;

/// A fresh queue with one file item; the temp dir holds the database.
async fn queue_with_item(name: &str) -> (tempfile::TempDir, QueueManager, String) {
    let temp_dir = tempdir().unwrap();
    let config = QueueConnectionConfig::with_database_path(temp_dir.path().join(name));
    let pool = config.create_pool().await.unwrap();
    apply_sql_script(&pool, include_str!("../../schema/watch_folders_schema.sql"))
        .await
        .unwrap();
    let manager = QueueManager::new(pool);
    manager.init_unified_queue().await.unwrap();
    let (queue_id, _) = manager
        .enqueue_unified(
            ItemType::File,
            UnifiedOp::Add,
            "resolve-tenant",
            "projects",
            r#"{"file_path":"/test/resolve.rs"}"#,
            Some("main"),
            None,
        )
        .await
        .unwrap();
    (temp_dir, manager, queue_id)
}

async fn statuses(manager: &QueueManager, queue_id: &str) -> (String, String, String) {
    sqlx::query_as(
        "SELECT status, COALESCE(qdrant_status, ''), COALESCE(search_status, '')
         FROM unified_queue WHERE queue_id = ?1",
    )
    .bind(queue_id)
    .fetch_one(manager.pool())
    .await
    .unwrap()
}

/// The other sink done: recording this one resolves the item in the same
/// step — there is no moment at which both sinks read done while the overall
/// status does not.
#[tokio::test]
async fn test_resolve_destination_records_the_sink_and_resolves_the_item() {
    let (_dir, manager, queue_id) = queue_with_item("resolve_done.db").await;
    manager
        .update_destination_status(&queue_id, "qdrant", DestinationStatus::Done)
        .await
        .unwrap();

    let overall = manager
        .resolve_destination(&queue_id, "search", DestinationStatus::Done)
        .await
        .unwrap();
    assert_eq!(overall, Some(QueueStatus::Done));
    let (status, _, search) = statuses(&manager, &queue_id).await;
    assert_eq!((status.as_str(), search.as_str()), ("done", "done"));
}

#[tokio::test]
async fn test_resolve_destination_keeps_an_item_with_a_pending_sink() {
    let (_dir, manager, queue_id) = queue_with_item("resolve_pending.db").await;
    let (before, _, _) = statuses(&manager, &queue_id).await;

    let overall = manager
        .resolve_destination(&queue_id, "search", DestinationStatus::Done)
        .await
        .unwrap();
    assert_eq!(overall, Some(QueueStatus::InProgress));
    let (status, _, search) = statuses(&manager, &queue_id).await;
    assert_eq!(status, before, "the overall status is left as it was");
    assert_eq!(search, "done", "the sink itself is recorded");
}

#[tokio::test]
async fn test_resolve_destination_fails_the_item_on_a_failed_sink() {
    let (_dir, manager, queue_id) = queue_with_item("resolve_failed.db").await;
    manager
        .update_destination_status(&queue_id, "qdrant", DestinationStatus::Done)
        .await
        .unwrap();

    let overall = manager
        .resolve_destination(&queue_id, "search", DestinationStatus::Failed)
        .await
        .unwrap();
    assert_eq!(overall, Some(QueueStatus::Failed));
    let (status, _, search) = statuses(&manager, &queue_id).await;
    assert_eq!((status.as_str(), search.as_str()), ("failed", "failed"));
}

/// A row another path already removed is `None`, not an error — unlike
/// `check_and_finalize`, whose caller owns the row. An unknown destination
/// is still an error.
#[tokio::test]
async fn test_resolve_destination_on_a_removed_item_is_none() {
    let (_dir, manager, queue_id) = queue_with_item("resolve_gone.db").await;
    assert!(manager.delete_unified_item(&queue_id).await.unwrap());

    let overall = manager
        .resolve_destination(&queue_id, "search", DestinationStatus::Done)
        .await
        .unwrap();
    assert_eq!(overall, None);
    assert!(manager
        .resolve_destination(&queue_id, "elsewhere", DestinationStatus::Done)
        .await
        .is_err());
}
