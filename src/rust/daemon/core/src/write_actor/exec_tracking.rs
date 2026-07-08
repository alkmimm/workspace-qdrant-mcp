//! SQL execution for TrackingWriteService commands.

use super::actor::WriteActor;
use super::commands::*;
use wqm_common::timestamps;

/// Ceiling for plausible token-economy byte counts (1 TiB). This is the
/// single write point for `search_events.bytes_in`/`bytes_out` across every
/// producer (MCP server tools today, CLI or others tomorrow), so implausible
/// values are rejected HERE rather than per-producer: a buggy client once
/// summed gRPC int64s delivered as strings (digit concatenation), the result
/// overflowed i64 on the wire and was clamped to i64::MAX — and a single such
/// row makes `SUM(bytes_in)` in the `token_savings` view raise
/// "integer overflow". Out-of-range values are stored as NULL ("unknown",
/// excluded from aggregates per spec 20 §2) and logged.
const MAX_PLAUSIBLE_ECONOMY_BYTES: i64 = 1 << 40;

fn sanitize_economy_bytes(value: i64, field: &str, event_id: &str) -> Option<i64> {
    if (0..=MAX_PLAUSIBLE_ECONOMY_BYTES).contains(&value) {
        Some(value)
    } else {
        tracing::warn!(
            event_id = %event_id,
            field = field,
            value = value,
            "implausible token-economy byte count from producer; storing NULL"
        );
        None
    }
}

impl WriteActor {
    pub(super) async fn exec_log_search_event(&self, data: LogSearchEventData) -> WriteResult<()> {
        let now = timestamps::now_utc();

        sqlx::query(
            "INSERT INTO search_events (
                id, ts, session_id, project_id, actor, tool, op,
                query_text, filters, top_k, result_count, latency_ms,
                top_result_refs, outcome, parent_event_id, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?2)",
        )
        .bind(&data.id)
        .bind(&now)
        .bind(&data.session_id)
        .bind(&data.project_id)
        .bind(&data.actor)
        .bind(&data.tool)
        .bind(&data.op)
        .bind(&data.query_text)
        .bind(&data.filters)
        .bind(data.top_k)
        .bind(data.result_count)
        .bind(data.latency_ms)
        .bind(&data.top_result_refs)
        .bind(&data.outcome)
        .bind(&data.parent_event_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("failed to log search event: {}", e))?;

        // Read-path activity touch: refresh `last_activity_at` for the project
        // this event targets, so cross-project reads (search/grep/list/retrieve
        // issued from another repo's cwd, which all funnel through
        // LogSearchEvent) surface in the admin UI / project list instead of
        // freezing at the last session registration.
        //
        // Deliberately does NOT touch `is_active`: activity is not an active
        // session. `is_active` remains the leak-proof projection of live
        // `project_sessions` rows (migration v42), so this touch never spins up
        // LSP servers or reorders the dequeue priority. Best-effort — a failure
        // here must not fail the already-persisted search event.
        if let Some(project_id) = data.project_id.as_deref() {
            if let Err(e) = sqlx::query(
                "UPDATE watch_folders \
                 SET last_activity_at = ?1, updated_at = ?1 \
                 WHERE tenant_id = ?2 AND collection = 'projects'",
            )
            .bind(&now)
            .bind(project_id)
            .execute(&self.pool)
            .await
            {
                tracing::warn!(
                    project_id = %project_id,
                    "failed to touch last_activity_at on search event (non-fatal): {}",
                    e
                );
            }
        }

        Ok(())
    }

    pub(super) async fn exec_update_search_event(
        &self,
        data: UpdateSearchEventData,
    ) -> WriteResult<()> {
        sqlx::query(
            "UPDATE search_events \
             SET result_count = ?1, latency_ms = ?2, top_result_refs = ?3, outcome = ?4 \
             WHERE id = ?5",
        )
        .bind(data.result_count)
        .bind(data.latency_ms)
        .bind(&data.top_result_refs)
        .bind(&data.outcome)
        .bind(&data.event_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("failed to update search event: {}", e))?;

        Ok(())
    }

    pub(super) async fn exec_update_search_event_economy(
        &self,
        data: UpdateSearchEventEconomyData,
    ) -> WriteResult<()> {
        sqlx::query(
            "UPDATE search_events \
             SET bytes_in = ?1, bytes_out = ?2, hits_truncated = ?3, \
                 shape_mode = ?4, tool_version = ?5 \
             WHERE id = ?6",
        )
        .bind(sanitize_economy_bytes(
            data.bytes_in,
            "bytes_in",
            &data.event_id,
        ))
        .bind(sanitize_economy_bytes(
            data.bytes_out,
            "bytes_out",
            &data.event_id,
        ))
        .bind(data.hits_truncated)
        .bind(&data.shape_mode)
        .bind(&data.tool_version)
        .bind(&data.event_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("failed to update search event economy: {}", e))?;

        Ok(())
    }

    pub(super) async fn exec_upsert_rule_mirror(
        &self,
        data: UpsertRuleMirrorData,
    ) -> WriteResult<()> {
        sqlx::query(
            "INSERT INTO rules_mirror (rule_id, rule_text, scope, tenant_id, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(rule_id) DO UPDATE SET \
                 rule_text = excluded.rule_text, \
                 scope = excluded.scope, \
                 tenant_id = excluded.tenant_id, \
                 updated_at = excluded.updated_at",
        )
        .bind(&data.rule_id)
        .bind(&data.rule_text)
        .bind(&data.scope)
        .bind(&data.tenant_id)
        .bind(&data.created_at)
        .bind(&data.updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("failed to upsert rules mirror: {}", e))?;

        Ok(())
    }

    pub(super) async fn exec_delete_rule_mirror(
        &self,
        data: DeleteRuleMirrorData,
    ) -> WriteResult<()> {
        sqlx::query("DELETE FROM rules_mirror WHERE rule_id = ?1")
            .bind(&data.rule_id)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("failed to delete rules mirror: {}", e))?;

        Ok(())
    }

    pub(super) async fn exec_upsert_scratchpad_mirror(
        &self,
        data: UpsertScratchpadMirrorData,
    ) -> WriteResult<()> {
        sqlx::query(
            "INSERT INTO scratchpad_mirror \
             (scratchpad_id, title, content, tags, tenant_id, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT(scratchpad_id) DO UPDATE SET \
                 title = excluded.title, \
                 content = excluded.content, \
                 tags = excluded.tags, \
                 tenant_id = excluded.tenant_id, \
                 updated_at = excluded.updated_at",
        )
        .bind(&data.scratchpad_id)
        .bind(&data.title)
        .bind(&data.content)
        .bind(&data.tags)
        .bind(&data.tenant_id)
        .bind(&data.created_at)
        .bind(&data.updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("failed to upsert scratchpad mirror: {}", e))?;

        Ok(())
    }

    pub(super) async fn exec_delete_scratchpad_mirror(
        &self,
        data: DeleteScratchpadMirrorData,
    ) -> WriteResult<()> {
        sqlx::query("DELETE FROM scratchpad_mirror WHERE scratchpad_id = ?1")
            .bind(&data.scratchpad_id)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("failed to delete scratchpad mirror: {}", e))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_economy_bytes_accepts_plausible_values() {
        assert_eq!(sanitize_economy_bytes(0, "bytes_in", "e1"), Some(0));
        assert_eq!(sanitize_economy_bytes(8192, "bytes_in", "e1"), Some(8192));
        assert_eq!(
            sanitize_economy_bytes(MAX_PLAUSIBLE_ECONOMY_BYTES, "bytes_in", "e1"),
            Some(MAX_PLAUSIBLE_ECONOMY_BYTES)
        );
    }

    #[test]
    fn sanitize_economy_bytes_nulls_garbage() {
        // Negative (e.g. u64→i64 wrap) and beyond-plausible values (e.g. the
        // digit-concatenation bug clamped to i64::MAX on the wire) → NULL.
        assert_eq!(sanitize_economy_bytes(-1, "bytes_in", "e1"), None);
        assert_eq!(
            sanitize_economy_bytes(MAX_PLAUSIBLE_ECONOMY_BYTES + 1, "bytes_out", "e1"),
            None
        );
        assert_eq!(sanitize_economy_bytes(i64::MAX, "bytes_in", "e1"), None);
    }
}
