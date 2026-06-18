//! Advanced queue operations: cascade rename, library documents, reference counting.

use tracing::{error, info};
use wqm_common::constants::COLLECTION_LIBRARIES;

use crate::unified_queue_schema::{ItemType, ProjectPayload, QueueOperation as UnifiedOp};

use super::{QueueError, QueueManager, QueueResult};

impl QueueManager {
    /// Enqueue cascade rename operations for all specified collections.
    ///
    /// Creates a rename queue item for each collection that needs its
    /// tenant_id payloads updated in Qdrant. Called after SQLite state
    /// (watch_folders, tracked_files) has already been updated.
    pub async fn enqueue_cascade_rename(
        &self,
        old_tenant_id: &str,
        new_tenant_id: &str,
        collections: &[&str],
        reason: &str,
    ) -> QueueResult<Vec<String>> {
        let mut queue_ids = Vec::new();

        for collection in collections {
            let payload = ProjectPayload {
                project_root: String::new(), // Not needed for rename
                git_remote: None,
                project_type: None,
                old_tenant_id: Some(old_tenant_id.to_string()),
                is_active: None,
            };

            let payload_json = serde_json::to_string(&payload)
                .map_err(|e| QueueError::InvalidPayloadJson(e.to_string()))?;

            let metadata = serde_json::json!({
                "reason": reason,
            })
            .to_string();

            let (queue_id, _is_new) = self
                .enqueue_unified(
                    ItemType::Tenant,
                    UnifiedOp::Rename,
                    new_tenant_id,
                    collection,
                    &payload_json,
                    None,
                    Some(&metadata),
                )
                .await?;

            queue_ids.push(queue_id);
        }

        info!(
            "Enqueued {} cascade rename items: {} -> {} (reason: {})",
            queue_ids.len(),
            old_tenant_id,
            new_tenant_id,
            reason
        );

        Ok(queue_ids)
    }

    /// Enqueue a library document for ingestion.
    ///
    /// Convenience wrapper around `enqueue_unified` that accepts a
    /// `LibraryDocumentPayload` and routes to the libraries collection.
    pub async fn enqueue_library_document(
        &self,
        payload: &wqm_common::payloads::LibraryDocumentPayload,
        op: UnifiedOp,
        branch: Option<&str>,
    ) -> QueueResult<(String, bool)> {
        let payload_json = serde_json::to_string(payload)?;
        self.enqueue_unified(
            ItemType::File,
            op,
            &payload.library_name,
            COLLECTION_LIBRARIES,
            &payload_json,
            branch,
            None,
        )
        .await
    }

    /// Validate a library document payload has required fields.
    ///
    /// Called during library document processing to ensure the payload
    /// contains the document family taxonomy fields.
    pub fn validate_library_document_payload(payload: &serde_json::Value) -> QueueResult<()> {
        let required_fields = [
            ("document_path", "Library document missing 'document_path'"),
            ("library_name", "Library document missing 'library_name'"),
            ("document_type", "Library document missing 'document_type'"),
            ("source_format", "Library document missing 'source_format'"),
            ("doc_id", "Library document missing 'doc_id'"),
        ];

        for (field, msg) in &required_fields {
            if !payload.get(*field).map_or(false, |v| {
                v.is_string() && !v.as_str().unwrap_or("").is_empty()
            }) {
                error!("Queue validation failed: {}", msg);
                return Err(QueueError::MissingPayloadField {
                    item_type: "file".to_string(),
                    field: field.to_string(),
                });
            }
        }

        // Validate document_type is one of the known families
        if let Some(doc_type) = payload.get("document_type").and_then(|v| v.as_str()) {
            if doc_type != "page_based" && doc_type != "stream_based" {
                error!("Queue validation failed: invalid document_type '{}', must be 'page_based' or 'stream_based'", doc_type);
                return Err(QueueError::InvalidOperation(format!(
                    "Invalid document_type: '{}', must be 'page_based' or 'stream_based'",
                    doc_type
                )));
            }
        }

        Ok(())
    }

    /// Check if any OTHER tracked_files row references the same base_point,
    /// excluding the row identified by `exclude_file_id`.
    ///
    /// Used for reference-counted deletion: before deleting the Qdrant points
    /// for a row, check whether anyone else still needs them. Layer 2 makes a
    /// single physical point shared across branches (and across multi-clone
    /// watch folders) — every row holding the same content shares ONE
    /// `base_point`. So the guard must exclude the specific row being removed
    /// (by `file_id`), NOT a whole watch folder: another BRANCH's row in the
    /// SAME watch folder still references the point and must keep it alive.
    pub async fn has_other_references(
        &self,
        base_point: &str,
        exclude_file_id: i64,
    ) -> QueueResult<bool> {
        let count: i32 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*) FROM tracked_files
            WHERE base_point = ?1
              AND file_id != ?2
            "#,
        )
        .bind(base_point)
        .bind(exclude_file_id)
        .fetch_one(&self.pool)
        .await?;

        Ok(count > 0)
    }
}
