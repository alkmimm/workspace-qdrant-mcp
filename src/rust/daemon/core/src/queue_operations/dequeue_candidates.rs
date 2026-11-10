//! Candidate collection for dequeue: the short list the ranking query sorts.
//!
//! Ranking the WHOLE queue on every dequeue cost ~0.9 s of CPU at 663k pending
//! rows (measured 2026-11-10), and it ran inside `BEGIN IMMEDIATE`, so every
//! other writer (tracked-files updates, heartbeats, enqueues) waited behind it:
//! p50 3 s for a primary-key UPDATE, the queue drained ~4k items/h and grew
//! faster than that. Bigger queue → slower dequeue → slower drain.
//!
//! Why a per-group head is enough: within one group of
//! `(tenant_id, collection, op, item_type)` every term of the dequeue ORDER BY
//! is constant except the age bucket and `created_at`, and the age bucket is
//! monotone in `created_at`. So on the normal pass the best rows of a group are
//! its OLDEST, and on the anti-starvation pass (age ASC, created_at DESC) its
//! NEWEST. The global top `batch_size` is therefore contained in the union of
//! each group's top `batch_size` — collecting those and ranking only them
//! returns exactly what ranking the whole queue would.
//!
//! Every query here walks `idx_unified_queue_status_group`
//! `(status, tenant_id, collection, op, item_type, created_at)`: the groups are
//! enumerated with a loose index scan (one seek per distinct value per column)
//! and each head is an index range read. ~1 ms total on the same 667k rows.

use sqlx::SqliteConnection;

use super::dequeue_query::DequeueParams;
use super::QueueResult;

/// The grouping columns, in index order after `status`.
const GROUP_COLUMNS: [&str; 4] = ["tenant_id", "collection", "op", "item_type"];

/// Upper bound on expired-lease rows considered per dequeue. Expired leases
/// are rare (one per crashed or overrunning worker), so this only guards a
/// pathological pile-up; they are ranked with the rest.
const MAX_EXPIRED_CANDIDATES: i64 = 256;

/// Collect the queue ids the ranking query has to choose from.
pub(super) async fn collect_candidates(
    conn: &mut SqliteConnection,
    params: &DequeueParams<'_>,
) -> QueueResult<Vec<String>> {
    let item_type = params.item_type.map(|it| it.to_string());
    let mut ids = Vec::new();
    for group in pending_groups(conn, params.tenant_id).await? {
        if item_type.as_deref().is_some_and(|it| it != group[3]) {
            continue;
        }
        ids.extend(group_head(conn, &group, params).await?);
    }
    ids.extend(expired_leases(conn, params.now_str).await?);
    Ok(ids)
}

/// Distinct `(tenant_id, collection, op, item_type)` among pending rows, by a
/// loose index scan. With `tenant_id` set, only that tenant's groups.
pub(super) async fn pending_groups(
    conn: &mut SqliteConnection,
    tenant_id: Option<&str>,
) -> QueueResult<Vec<Vec<String>>> {
    let mut prefix: Vec<String> = tenant_id.map(|t| vec![t.to_string()]).unwrap_or_default();
    let floor = prefix.len();
    let mut after: Option<String> = None;
    let mut groups = Vec::new();
    loop {
        if prefix.len() == GROUP_COLUMNS.len() {
            groups.push(prefix.clone());
            after = prefix.pop();
            continue;
        }
        match next_value(conn, &prefix, after.as_deref()).await? {
            Some(value) => {
                prefix.push(value);
                after = None;
            }
            None if prefix.len() == floor => break,
            None => after = prefix.pop(),
        }
    }
    Ok(groups)
}

/// The smallest value of the next grouping column under `prefix` that is
/// greater than `after` — one index seek.
async fn next_value(
    conn: &mut SqliteConnection,
    prefix: &[String],
    after: Option<&str>,
) -> QueueResult<Option<String>> {
    let column = GROUP_COLUMNS[prefix.len()];
    let mut sql = String::from("SELECT ");
    sql.push_str(column);
    sql.push_str(" FROM unified_queue WHERE status = 'pending'");
    for (i, prefix_column) in GROUP_COLUMNS[..prefix.len()].iter().enumerate() {
        sql.push_str(&format!(" AND {} = ?{}", prefix_column, i + 1));
    }
    if after.is_some() {
        sql.push_str(&format!(" AND {} > ?{}", column, prefix.len() + 1));
    }
    sql.push_str(&format!(" ORDER BY {} LIMIT 1", column));

    let mut query = sqlx::query_scalar::<_, String>(&sql);
    for value in prefix {
        query = query.bind(value);
    }
    if let Some(after) = after {
        query = query.bind(after);
    }
    Ok(query.fetch_optional(&mut *conn).await?)
}

/// A group's best `batch_size` claimable rows for this pass.
async fn group_head(
    conn: &mut SqliteConnection,
    group: &[String],
    params: &DequeueParams<'_>,
) -> QueueResult<Vec<String>> {
    // Oldest first on the normal pass, newest first on the anti-starvation pass
    // — the same direction the ranking query sorts `created_at` in.
    let order = if params.descending { "ASC" } else { "DESC" };
    let sql = format!(
        "SELECT queue_id FROM unified_queue \
         WHERE status = 'pending' AND tenant_id = ?1 AND collection = ?2 \
           AND op = ?3 AND item_type = ?4 \
           AND (lease_until IS NULL OR lease_until < ?5) \
         ORDER BY created_at {} LIMIT ?6",
        order
    );
    let ids = sqlx::query_scalar::<_, String>(&sql)
        .bind(&group[0])
        .bind(&group[1])
        .bind(&group[2])
        .bind(&group[3])
        .bind(params.now_str)
        .bind(params.batch_size)
        .fetch_all(&mut *conn)
        .await?;
    Ok(ids)
}

/// `in_progress` rows whose lease expired — re-lease candidates.
async fn expired_leases(conn: &mut SqliteConnection, now_str: &str) -> QueueResult<Vec<String>> {
    let ids = sqlx::query_scalar::<_, String>(
        "SELECT queue_id FROM unified_queue \
         WHERE status = 'in_progress' AND lease_until < ?1 \
         ORDER BY lease_until LIMIT ?2",
    )
    .bind(now_str)
    .bind(MAX_EXPIRED_CANDIDATES)
    .fetch_all(&mut *conn)
    .await?;
    Ok(ids)
}
