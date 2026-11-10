//! The dequeue ranking query.
//!
//! [`rank_candidates`] orders a SHORT candidate list (see
//! `dequeue_candidates`) by the full dequeue priority and returns the top
//! `batch_size` queue ids. The ranking SQL is the single definition of dequeue
//! priority; the candidate list only bounds how many rows it has to sort.

use sqlx::SqliteConnection;
use wqm_common::constants::{COLLECTION_LIBRARIES, COLLECTION_PROJECTS, COLLECTION_RULES};

use crate::unified_queue_schema::ItemType;

use super::QueueResult;

/// Op-order is always DESC: delete always takes precedence over all other
/// operations regardless of which priority pass is running. Delete is a
/// correctness concern (stale data removal), not just a performance preference.
const OP_ORDER: &str = "DESC";

/// Everything one dequeue call ranks by.
pub(super) struct DequeueParams<'a> {
    pub now_str: &'a str,
    pub tenant_id: Option<&'a str>,
    pub item_type: Option<ItemType>,
    /// `true` = normal pass (high priority first, FIFO); `false` = the
    /// anti-starvation pass (low priority first, LIFO).
    pub descending: bool,
    pub batch_size: i32,
    pub age_warning_seconds: i64,
    pub age_critical_seconds: i64,
}

impl DequeueParams<'_> {
    fn priority_order(&self) -> &'static str {
        if self.descending {
            "DESC"
        } else {
            "ASC"
        }
    }

    /// FIFO on the normal pass, LIFO on the anti-starvation pass.
    fn created_at_order(&self) -> &'static str {
        if self.descending {
            "ASC"
        } else {
            "DESC"
        }
    }
}

/// Rank `candidates` by dequeue priority and return the top `batch_size` ids.
///
/// The WHERE clause still re-checks claimability, so a candidate that was
/// claimed or finished since it was collected simply drops out.
pub(super) async fn rank_candidates(
    conn: &mut SqliteConnection,
    params: &DequeueParams<'_>,
    candidates: &[String],
) -> QueueResult<Vec<String>> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let query = build_dequeue_query(
        params.tenant_id,
        params.item_type,
        params.priority_order(),
        OP_ORDER,
        params.created_at_order(),
    );
    let candidates_json = serde_json::to_string(candidates)?;
    let mut builder = sqlx::query_scalar::<_, String>(&query).bind(params.now_str);
    if let Some(tid) = params.tenant_id {
        builder = builder.bind(tid);
    }
    if let Some(itype) = params.item_type {
        builder = builder.bind(itype.to_string());
    }
    let ids = builder
        .bind(params.age_warning_seconds)
        .bind(params.age_critical_seconds)
        .bind(params.batch_size)
        .bind(candidates_json)
        .fetch_all(&mut *conn)
        .await?;
    Ok(ids)
}

/// Build the dequeue SELECT query with filters substituted in.
///
/// Parameter binding order (positional):
/// - `?1`           = `now_str` (lease expiry comparison)
/// - tenant filter  = `?2` (tenant_id) and/or `?3` (item_type), see match below
/// - age thresholds = next two positions (warning, critical) — bound BEFORE the limit
/// - limit          = next position
/// - candidates     = last position: a JSON array of queue ids to rank
///
/// The age-promotion CASE is placed AFTER the delete-first and tenant/add priority
/// rows but BEFORE the project-active priority. This ensures:
/// - delete/reset still take precedence (correctness)
/// - tenant registrations still jump the line (UX)
/// - old items DO override the active-vs-inactive ranking once they cross the
///   warning/critical thresholds — preventing tenants whose pending items have
///   low op-weight (e.g. folder/scan) from being starved indefinitely when other
///   tenants have larger queues with higher-weight ops.
///
/// The age-promotion CASE uses `{priority_order}` so the DESC/ASC fairness flip
/// still inverts the ranking on the anti-starvation pass.
pub(super) fn build_dequeue_query(
    tenant_id: Option<&str>,
    item_type: Option<ItemType>,
    priority_order: &str,
    op_order: &str,
    created_at_order: &str,
) -> String {
    let (tenant_filter, age_warning_param, age_critical_param, limit_param, candidates_param) =
        match (tenant_id, item_type) {
            (Some(_), Some(_)) => (
                "AND q.tenant_id = ?2 AND q.item_type = ?3",
                "?4",
                "?5",
                "?6",
                "?7",
            ),
            (Some(_), None) => ("AND q.tenant_id = ?2", "?3", "?4", "?5", "?6"),
            (None, Some(_)) => ("AND q.item_type = ?2", "?3", "?4", "?5", "?6"),
            (None, None) => ("", "?2", "?3", "?4", "?5"),
        };
    format!(
        r#"
        SELECT q.queue_id
        FROM unified_queue q
        LEFT JOIN watch_folders w
            ON q.tenant_id = w.tenant_id
            AND q.collection = '{coll_projects}'
            AND w.parent_watch_id IS NULL
        WHERE (
            (q.status = 'pending' AND (q.lease_until IS NULL OR q.lease_until < ?1))
            OR (q.status = 'in_progress' AND q.lease_until < ?1)
        )
        AND q.queue_id IN (SELECT value FROM json_each({candidates_param}))
        {tenant_filter}
        ORDER BY
            CASE WHEN q.op = 'delete' THEN 1
                 WHEN q.op = 'reset' THEN 1
                 ELSE 0
            END DESC,
            -- Project registrations jump the line so `wqm project register`
            -- materialises in `wqm project list` even when the queue is
            -- backlogged with file ingestion (issue #70).
            CASE WHEN q.item_type = 'tenant' AND q.op = 'add' THEN 1 ELSE 0 END DESC,
            -- Age-based promotion (Unit 3 of audit issue #8): prevent starvation
            -- of tenants whose pending items have low op-weight by promoting
            -- old items above the project-active ranking.
            -- +1 once age >= warning threshold, +2 once age >= critical threshold.
            -- Uses {priority_order} so anti-starvation flip still inverts ranking.
            CASE
                WHEN (strftime('%s','now') - strftime('%s', q.created_at))
                     >= {age_critical_param} THEN 2
                WHEN (strftime('%s','now') - strftime('%s', q.created_at))
                     >= {age_warning_param} THEN 1
                ELSE 0
            END {priority_order},
            CASE
                WHEN q.collection = '{coll_memory}' THEN 1
                WHEN q.collection = '{coll_libraries}' THEN 0
                WHEN w.is_active > 0 THEN 1
                ELSE 0
            END {priority_order},
            CASE WHEN q.op = 'delete' THEN 10
                 WHEN q.op = 'reset' THEN 8
                 WHEN q.op = 'add' THEN 5
                 WHEN q.op = 'update' THEN 4
                 WHEN q.op = 'rename' THEN 3
                 WHEN q.op = 'uplift' THEN 2
                 WHEN q.op = 'scan' THEN 1
                 ELSE 1
            END {op_order},
            q.created_at {created_at_order}
        LIMIT {limit_param}
        "#,
        coll_projects = COLLECTION_PROJECTS,
        coll_libraries = COLLECTION_LIBRARIES,
        coll_memory = COLLECTION_RULES,
        tenant_filter = tenant_filter,
        priority_order = priority_order,
        op_order = op_order,
        created_at_order = created_at_order,
        age_warning_param = age_warning_param,
        age_critical_param = age_critical_param,
        limit_param = limit_param,
        candidates_param = candidates_param,
    )
}

#[cfg(test)]
#[path = "dequeue_query_tests.rs"]
mod tests;
