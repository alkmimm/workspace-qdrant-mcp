//! Tenant rotation for the scheduler's anti-starvation turns.
//!
//! A global delete-first queue can spend hours pruning one project's dead
//! branches while unrelated scratchpad writes and scans never run. Rotate the
//! tenant on low-priority turns and serve inline writes before file maintenance.
//! Scoped dequeue retains delete precedence within each item type and lease checks.

use super::{FairnessResult, FairnessScheduler};
use crate::queue_operations::QueueError;
use crate::unified_queue_schema::{ItemType, UnifiedQueueItem};

impl FairnessScheduler {
    pub(super) async fn dequeue_rotating_tenant_batch(
        &self,
        max_items: i32,
    ) -> FairnessResult<Vec<UnifiedQueueItem>> {
        let now = wqm_common::timestamps::now_utc();
        let tenant = {
            let mut state = self.alternation_state.write().await;
            let after = state.last_fair_tenant.as_deref().unwrap_or("");
            let next = match self.claimable_tenant_after(after, &now).await? {
                Some(tenant) => Some(tenant),
                None if !after.is_empty() => self.claimable_tenant_after("", &now).await?,
                None => None,
            };
            if let Some(ref tenant) = next {
                state.last_fair_tenant = Some(tenant.clone());
            }
            next
        };
        if let Some(tenant) = tenant {
            // Inline writes are independent of file/branch maintenance. A
            // tenant's own delete backlog must not block its scratchpad/rules
            // after rotation has protected it from OTHER tenants' deletes.
            let text = self
                .dequeue_project_batch(&tenant, max_items, false, Some(ItemType::Text))
                .await?;
            if !text.is_empty() {
                return Ok(text);
            }
            let items = self
                .dequeue_project_batch(&tenant, max_items, false, None)
                .await?;
            if !items.is_empty() {
                return Ok(items);
            }
        }
        // No pending tenant, or another worker claimed its head meanwhile:
        // retain global expired-lease recovery instead of reporting idle.
        self.dequeue_global_batch(max_items, false).await
    }

    /// One range seek in idx_unified_queue_status_group; never sort the whole
    /// pending queue. A future lease must not pin the rotation on an idle tenant.
    async fn claimable_tenant_after(
        &self,
        after: &str,
        now: &str,
    ) -> FairnessResult<Option<String>> {
        sqlx::query_scalar(
            "SELECT tenant_id FROM unified_queue \
             WHERE status = 'pending' AND tenant_id > ?1 \
               AND (lease_until IS NULL OR lease_until < ?2) \
             ORDER BY tenant_id LIMIT 1",
        )
        .bind(after)
        .bind(now)
        .fetch_optional(self.queue_manager.pool())
        .await
        .map_err(|error| QueueError::Database(error).into())
    }
}
