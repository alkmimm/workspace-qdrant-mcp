//! Batch the Qdrant side of tag-only branch pruning for an already leased page.
//! Ambiguous holders, legacy IDs and physical deletes go through the file strategy.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use qdrant_client::qdrant::{Condition, Filter};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use uuid::Uuid;
use wqm_common::constants::COLLECTION_PROJECTS;
use wqm_common::{paths::CanonicalPath, timestamps};

use crate::context::ProcessingContext;
use crate::tracked_files_schema::{self, TrackedFile};
use crate::unified_queue_processor::{UnifiedProcessorError, UnifiedProcessorResult};
use crate::unified_queue_schema::{FilePayload, ItemType, QueueOperation, UnifiedQueueItem};

use super::{delete::is_branch_prune_delete, parse_payload, resolve_watch_folder};

// Bound the exclusive portion independently of a larger configured queue page.
const MAX_FILES: usize = 32;
const MAX_POINTS: i32 = 1024;
const GET_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Default)]
pub(crate) struct PruneBatch {
    pub outcomes: HashMap<usize, UnifiedProcessorResult<()>>,
    pub defer_remaining: bool,
    get_calls: usize,
    set_payload_calls: usize,
}

struct Candidate {
    index: usize,
    file: TrackedFile,
    base_point: String,
    point_id: Uuid,
}

pub(crate) fn is_project_prune(item: &UnifiedQueueItem) -> bool {
    item.item_type == ItemType::File
        && item.op == QueueOperation::Delete
        && item.collection == COLLECTION_PROJECTS
        && is_branch_prune_delete(item)
}

pub(crate) fn prune_prefix_len(items: &[UnifiedQueueItem]) -> usize {
    let Some(first) = items.first() else {
        return 0;
    };
    if items.iter().any(|item| item.item_type == ItemType::Text) {
        return 0;
    }
    items
        .iter()
        .take(MAX_FILES)
        .take_while(|item| {
            is_project_prune(item)
                && item.tenant_id == first.tenant_id
                && item.branch == first.branch
        })
        .count()
}

pub(crate) async fn process_prune_batch(
    ctx: &ProcessingContext,
    items: &[UnifiedQueueItem],
    cancel: &CancellationToken,
) -> PruneBatch {
    let mut result = PruneBatch::default();
    perform_prune_batch(ctx, items, cancel, &mut result).await;
    if result.get_calls > 0 || result.defer_remaining {
        let succeeded = result
            .outcomes
            .values()
            .filter(|value| value.is_ok())
            .count();
        info!(
            leased = items.len(),
            get_calls = result.get_calls,
            set_payload_calls = result.set_payload_calls,
            confirmed = succeeded,
            retry = result.outcomes.len() - succeeded,
            remaining = items.len() - result.outcomes.len(),
            deferred = result.defer_remaining,
            "Branch prune batch"
        );
    }
    result
}

async fn perform_prune_batch(
    ctx: &ProcessingContext,
    items: &[UnifiedQueueItem],
    cancel: &CancellationToken,
    result: &mut PruneBatch,
) {
    if should_yield(ctx, cancel).await {
        result.defer_remaining = true;
        return;
    }
    let first = &items[0];
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    let mut points = 0;
    for (index, item) in items.iter().enumerate() {
        if let Some(candidate) = prepare_candidate(ctx, item, index).await {
            let chunks = candidate.file.chunk_count.max(1);
            if chunks <= MAX_POINTS - points && seen.insert(candidate.base_point.clone()) {
                points += chunks;
                candidates.push(candidate);
            }
        }
    }
    if candidates.len() < 2 {
        return;
    }
    if should_yield(ctx, cancel).await {
        result.defer_remaining = true;
        return;
    }

    let ids: Vec<_> = candidates.iter().map(|c| c.point_id).collect();
    result.get_calls += 1;
    let read = ctx.storage_client.read_tracked_branch_sets(
        &first.collection,
        &ids,
        Some(&first.tenant_id),
    );
    let observed = match tokio::time::timeout(GET_TIMEOUT, read).await {
        Ok(Ok(observed)) => observed,
        Ok(Err(error)) => {
            warn!(%error, "Branch prune batch Get failed, using per-file reads");
            result.defer_remaining = should_yield(ctx, cancel).await;
            return;
        }
        Err(_) => {
            warn!("Branch prune batch Get timed out, using per-file reads");
            result.defer_remaining = should_yield(ctx, cancel).await;
            return;
        }
    };

    let mut writes = BTreeMap::<Vec<String>, Vec<&Candidate>>::new();
    for candidate in &candidates {
        let Some((base_point, branches)) = observed.get(&candidate.point_id) else {
            continue;
        };
        if base_point != &candidate.base_point
            || candidate
                .file
                .branches
                .iter()
                .any(|branch| branch != &first.branch && !branches.contains(branch))
        {
            continue;
        }
        match surviving_branches(branches, &first.branch) {
            Some((_, false)) => {
                // A previous attempt may already have applied the Qdrant write.
                result.outcomes.insert(
                    candidate.index,
                    finish_tag_removal(ctx, candidate, &first.branch).await,
                );
            }
            Some((remaining, true)) => writes.entry(remaining).or_default().push(candidate),
            None => {}
        }
    }

    for (remaining, group) in writes {
        if should_yield(ctx, cancel).await {
            result.defer_remaining = true;
            return;
        }
        let base_points: Vec<_> = group.iter().map(|c| c.base_point.clone()).collect();
        let filter = Filter::must([
            Condition::matches("base_point", base_points),
            Condition::matches("tenant_id", first.tenant_id.clone()),
            Condition::matches("branch", first.branch.clone()),
        ]);
        let payload = HashMap::from([("branch".to_string(), serde_json::json!(remaining))]);
        result.set_payload_calls += 1;
        match ctx
            .storage_client
            .set_payload_by_filter(&first.collection, filter, payload)
            .await
        {
            Ok(()) => {
                for candidate in group {
                    result.outcomes.insert(
                        candidate.index,
                        finish_tag_removal(ctx, candidate, &first.branch).await,
                    );
                }
            }
            Err(error) => {
                warn!(%error, items = group.len(), "Branch prune batch SetPayload failed");
                for candidate in group {
                    result.outcomes.insert(
                        candidate.index,
                        Err(UnifiedProcessorError::Storage(error.to_string())),
                    );
                }
            }
        }
    }
    result.defer_remaining = should_yield(ctx, cancel).await;
}

async fn should_yield(ctx: &ProcessingContext, cancel: &CancellationToken) -> bool {
    cancel.is_cancelled() || waiting_inline(&ctx.pool).await.unwrap_or(true)
}

async fn waiting_inline(pool: &sqlx::SqlitePool) -> Result<bool, sqlx::Error> {
    // The fairness scheduler gives inline writes their own turn. Return
    // untouched leases to it when new text work arrives between writes.
    sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM unified_queue WHERE item_type = 'text'
             AND ((status = 'pending' AND (lease_until IS NULL OR lease_until < ?1))
                  OR (status = 'in_progress' AND lease_until < ?1))
         )",
    )
    .bind(timestamps::now_utc())
    .fetch_one(pool)
    .await
}

async fn prepare_candidate(
    ctx: &ProcessingContext,
    item: &UnifiedQueueItem,
    index: usize,
) -> Option<Candidate> {
    let payload: FilePayload = parse_payload(item).ok()?;
    let relative = payload.file_path.as_str();
    let (watch, root) = resolve_watch_folder(&ctx.pool, item, relative).await.ok()?;
    let root = CanonicalPath::from_user_input(&root).ok()?;
    let path = payload.file_path.to_absolute(&root);
    if tracked_files_schema::is_incremental(&ctx.pool, path.as_str())
        .await
        .ok()?
    {
        return None;
    }

    let mut holders = tracked_files_schema::lookup_tracked_files_holding_branch(
        &ctx.pool,
        &watch,
        relative,
        &item.branch,
    )
    .await
    .ok()?;
    if holders.len() != 1 {
        return None;
    }
    let file = holders.pop()?;
    if file.incremental
        || file.collection != item.collection
        || !file.branches.iter().any(|branch| branch != &item.branch)
    {
        return None;
    }
    let base_point = file.base_point.clone()?;
    if ctx
        .queue_manager
        .branch_held_by_other(&base_point, file.file_id, &item.branch)
        .await
        .ok()?
    {
        return None;
    }
    let point: String = sqlx::query_scalar(
        "SELECT point_id FROM qdrant_chunks WHERE file_id = ?1
         ORDER BY lower(replace(point_id, '-', '')) LIMIT 1",
    )
    .bind(file.file_id)
    .fetch_one(&ctx.pool)
    .await
    .ok()?;

    Some(Candidate {
        index,
        file,
        base_point,
        point_id: Uuid::parse_str(&point).ok()?,
    })
}

fn surviving_branches(branches: &[String], branch: &str) -> Option<(Vec<String>, bool)> {
    let found = branches.iter().any(|current| current == branch);
    let mut remaining: Vec<_> = branches
        .iter()
        .filter(|current| *current != branch)
        .cloned()
        .collect();
    remaining.sort_unstable();
    remaining.dedup();
    (!remaining.is_empty()).then_some((remaining, found))
}

/// Finalize only confirmed tag removals. Update the search mirror first so an
/// error leaves the authority row and its branch available for a retry.
async fn finish_tag_removal(
    ctx: &ProcessingContext,
    candidate: &Candidate,
    branch: &str,
) -> UnifiedProcessorResult<()> {
    let file = &candidate.file;
    if let Some(db) = &ctx.search_db {
        db.remove_branch_from_file_metadata(file.file_id, branch)
            .await
            .map_err(|e| UnifiedProcessorError::Storage(e.to_string()))?;
    }
    let updated = sqlx::query(
        "UPDATE tracked_files
         SET branches = (SELECT json_group_array(value) FROM json_each(branches) WHERE value != ?1),
             updated_at = ?2
         WHERE file_id = ?3 AND base_point = ?4 AND updated_at = ?5
           AND EXISTS (SELECT 1 FROM json_each(branches) WHERE value != ?1)",
    )
    .bind(branch)
    .bind(timestamps::now_utc())
    .bind(file.file_id)
    .bind(&candidate.base_point)
    .bind(&file.updated_at)
    .execute(&ctx.pool)
    .await
    .map_err(|e| UnifiedProcessorError::Storage(e.to_string()))?;
    if updated.rows_affected() == 0 {
        return Err(UnifiedProcessorError::Storage(format!(
            "Tracked file {} changed during branch prune batch",
            file.file_id
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_prune_batch_groups_only_nonempty_surviving_sets() {
        let a = ["dev", "old", "main"].map(str::to_string);
        let b = ["old", "main", "dev", "dev"].map(str::to_string);
        assert_eq!(surviving_branches(&a, "old"), surviving_branches(&b, "old"));
        assert_eq!(
            surviving_branches(&a, "old"),
            Some((vec!["dev".into(), "main".into()], true))
        );
        assert_eq!(
            surviving_branches(&["main".into()], "old"),
            Some((vec!["main".into()], false))
        );
        assert_eq!(surviving_branches(&["old".into()], "old"), None);
        assert_eq!(surviving_branches(&[], "old"), None);
    }

    #[tokio::test]
    async fn branch_prune_batch_yields_for_claimable_inline_writes() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE unified_queue (item_type TEXT, status TEXT, lease_until TEXT)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO unified_queue VALUES ('file','pending',NULL),
                ('text','pending','9999-01-01T00:00:00Z'),
                ('text','in_progress','9999-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(!waiting_inline(&pool).await.unwrap());
        sqlx::query("INSERT INTO unified_queue VALUES ('text','pending',NULL)")
            .execute(&pool)
            .await
            .unwrap();
        assert!(waiting_inline(&pool).await.unwrap());
    }
}
