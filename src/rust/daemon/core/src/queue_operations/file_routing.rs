//! Format-based routing of `File` items, applied where every producer's item
//! enters the queue.
//!
//! A library-format document inside a project folder (`.pdf`, `.docx`, …; see
//! [`crate::allowed_extensions::is_library_routed`]) is stored in the
//! `libraries` collection under the project's `-refs` library, with metadata
//! naming the project it came from (the processor resolves the project's watch
//! folder through it). Until 2026-10-05 only the file watcher applied that
//! routing. Every other producer — branch switch and membership reconcile,
//! worktree discovery, the branch tip follower, the branch prune, the startup
//! reconcilers, the folder scan, the exclusion cleanup — enqueued such a file
//! under `projects`, where the ingest guard drops an add and a delete removes
//! points from the wrong Qdrant collection. Routing here makes the rule
//! impossible to forget for the next producer.

use std::borrow::Cow;

use wqm_common::constants::{COLLECTION_LIBRARIES, COLLECTION_PROJECTS};

use crate::allowed_extensions::is_library_routed;
use crate::format_routing::build_routing_metadata;
use crate::unified_queue_schema::ItemType;

/// Where an item is actually enqueued.
pub(super) struct RoutedItem<'a> {
    pub(super) tenant_id: Cow<'a, str>,
    pub(super) collection: Cow<'a, str>,
    pub(super) metadata: Cow<'a, str>,
}

/// Route a `File` item of a project folder that names a library-format
/// document to the project's library; leave every other item as given.
///
/// The routing keys are merged into the producer's metadata, so flags such as
/// `worktree_membership`, `read_root` or the branch prune's reason survive.
pub(super) fn route_file_item<'a>(
    item_type: ItemType,
    tenant_id: &'a str,
    collection: &'a str,
    payload: &serde_json::Value,
    metadata: &'a str,
) -> RoutedItem<'a> {
    let routes = item_type == ItemType::File
        && collection == COLLECTION_PROJECTS
        && payload
            .get("file_path")
            .and_then(serde_json::Value::as_str)
            .is_some_and(is_library_routed);
    if !routes {
        return RoutedItem {
            tenant_id: Cow::Borrowed(tenant_id),
            collection: Cow::Borrowed(collection),
            metadata: Cow::Borrowed(metadata),
        };
    }
    let routing = build_routing_metadata(tenant_id);
    let mut merged: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(metadata).unwrap_or_default();
    merged.insert("source_project_id".into(), routing.source_project_id.into());
    merged.insert("routing_reason".into(), routing.routing_reason.into());
    merged.insert("library_name".into(), routing.library_name.clone().into());
    RoutedItem {
        tenant_id: Cow::Owned(routing.library_name),
        collection: Cow::Borrowed(COLLECTION_LIBRARIES),
        metadata: Cow::Owned(serde_json::Value::Object(merged).to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue_operations::QueueManager;
    use crate::unified_queue_schema::{
        QueueOperation, CREATE_UNIFIED_QUEUE_INDEXES_SQL, CREATE_UNIFIED_QUEUE_SQL,
    };

    fn file(path: &str) -> serde_json::Value {
        serde_json::json!({ "file_path": path })
    }

    #[test]
    fn a_project_document_goes_to_its_library_keeping_the_producers_flags() {
        let meta = r#"{"worktree_membership":true,"read_root":"/stage/t/b/tip","git_stage":true}"#;
        let routed = route_file_item(
            ItemType::File,
            "t1",
            COLLECTION_PROJECTS,
            &file("docs/guide.docx"),
            meta,
        );
        assert_eq!(routed.collection, COLLECTION_LIBRARIES);
        assert_eq!(routed.tenant_id, "t1-refs");
        let merged: serde_json::Value = serde_json::from_str(&routed.metadata).unwrap();
        assert_eq!(merged["source_project_id"], "t1");
        assert_eq!(merged["routing_reason"], "format_based");
        assert_eq!(merged["library_name"], "t1-refs");
        assert_eq!(merged["worktree_membership"], true);
        assert_eq!(merged["read_root"], "/stage/t/b/tip");
        assert_eq!(merged["git_stage"], true);
    }

    #[test]
    fn code_keys_libraries_and_other_items_are_left_as_given() {
        let same = |item_type, collection, payload: serde_json::Value| {
            let r = route_file_item(item_type, "t1", collection, &payload, "{}");
            r.tenant_id == "t1" && r.collection == collection && r.metadata == "{}"
        };
        assert!(same(
            ItemType::File,
            COLLECTION_PROJECTS,
            file("src/main.rs")
        ));
        assert!(same(
            ItemType::File,
            COLLECTION_PROJECTS,
            file("ca/server.key")
        ));
        assert!(same(ItemType::File, COLLECTION_LIBRARIES, file("book.pdf")));
        assert!(same(
            ItemType::Folder,
            COLLECTION_PROJECTS,
            file("docs/a.pdf")
        ));
        assert!(same(
            ItemType::File,
            COLLECTION_PROJECTS,
            serde_json::json!({})
        ));
    }

    /// Every producer's item passes through `enqueue_unified` or its batch form;
    /// both must route, and the routed item is what the queue holds.
    #[tokio::test]
    async fn both_enqueue_forms_route_a_project_document() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(CREATE_UNIFIED_QUEUE_SQL)
            .execute(&pool)
            .await
            .unwrap();
        for idx in CREATE_UNIFIED_QUEUE_INDEXES_SQL {
            sqlx::query(idx).execute(&pool).await.unwrap();
        }
        let qm = QueueManager::new(pool.clone());
        qm.enqueue_unified(
            ItemType::File,
            QueueOperation::Add,
            "t1",
            COLLECTION_PROJECTS,
            r#"{"file_path":"docs/guide.docx"}"#,
            Some("feat"),
            Some(r#"{"worktree_membership":true}"#),
        )
        .await
        .unwrap();
        qm.enqueue_unified_batch(
            ItemType::File,
            QueueOperation::Delete,
            "t1",
            COLLECTION_PROJECTS,
            &[
                r#"{"file_path":"docs/old.pdf"}"#.to_string(),
                r#"{"file_path":"src/lib.rs"}"#.to_string(),
            ],
            Some("main"),
        )
        .await
        .unwrap();

        let rows: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT file_path, tenant_id, collection, metadata FROM unified_queue ORDER BY file_path",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let by_path = |p: &str| rows.iter().find(|r| r.0 == p).unwrap().clone();
        for doc in ["docs/guide.docx", "docs/old.pdf"] {
            let (_, tenant, collection, meta) = by_path(doc);
            assert_eq!(
                (tenant.as_str(), collection.as_str()),
                ("t1-refs", "libraries")
            );
            assert!(meta.contains(r#""source_project_id":"t1""#), "{meta}");
        }
        let (_, _, _, meta) = by_path("docs/guide.docx");
        assert!(meta.contains(r#""worktree_membership":true"#), "{meta}");
        let (_, tenant, collection, meta) = by_path("src/lib.rs");
        assert_eq!(
            (tenant.as_str(), collection.as_str(), meta.as_str()),
            ("t1", "projects", "{}")
        );
    }
}
