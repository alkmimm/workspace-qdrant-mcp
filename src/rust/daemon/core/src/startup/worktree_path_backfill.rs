//! One-shot repair for points whose stored path is anchored in a worktree.
//!
//! Before the store/read split (PR #347) a `read_root` ingest persisted the
//! WORKTREE absolute path — `…/.claude/worktrees/<wt>/src/x.rs` — while its
//! `relative_path` stayed main-anchored. The stored path is an identity, not a
//! label: `delete_points_by_filter` matches on it and
//! `generate_document_id(tenant_id, path)` derives from it, so those points
//! carry a delete key main can never produce and a document id that splits one
//! logical file in two. They also leak another session's checkout into read
//! results, which is what the read-side filters currently work around.
//!
//! #347 fixed the write path; nothing rewrites what is already stored. The
//! cross-branch dedup fast-path (`add_branch_to_base_point`) only ever sets the
//! `branch` array, so a first-ingest worktree anchor is frozen permanently —
//! these points would never self-heal, not even on a later re-ingest from main.
//! Hence this pass.
//!
//! What it does NOT touch: `base_point` and the point ids derive from
//! `relative_path`, which was always main-anchored — the physical points are
//! correct and are updated in place, never recreated.

use std::sync::Arc;

use qdrant_client::qdrant::{Condition, Filter};
use sqlx::SqlitePool;
use tracing::{info, warn};

use crate::storage::StorageClient;

/// Marker segment identifying a worktree-anchored path.
const WORKTREE_SEGMENT: &str = "/.claude/worktrees/";

/// Page size for the payload-only scroll.
const SCROLL_PAGE: u32 = 512;

/// Collections whose points carry file paths.
const PATH_COLLECTIONS: [&str; 2] = ["projects", "libraries"];

#[derive(Debug, Default)]
pub struct WorktreePathBackfillStats {
    pub points_scanned: u64,
    pub points_repaired: u64,
    pub search_rows_repaired: u64,
    pub failures: u64,
}

/// Strip the worktree segment from an absolute path, returning the
/// main-anchored form: `/repo/.claude/worktrees/wt/src/x.rs` → `/repo/src/x.rs`.
///
/// Returns `None` when the path is not worktree-anchored, or when the shape is
/// unexpected (no path component after the worktree name) — a malformed entry
/// is left alone rather than rewritten to something worse.
pub(crate) fn main_anchored_form(path: &str) -> Option<String> {
    let idx = path.find(WORKTREE_SEGMENT)?;
    let root = &path[..idx];
    let after = &path[idx + WORKTREE_SEGMENT.len()..];
    // after == "<wt-name>/<relative…>"; drop the worktree name.
    let (_wt_name, rest) = after.split_once('/')?;
    if rest.is_empty() {
        return None;
    }
    Some(format!("{}/{}", root, rest))
}

/// Repair every worktree-anchored path in Qdrant and search.db.
///
/// Idempotent: a second run finds nothing because the predicate is the presence
/// of the worktree segment in the stored path.
pub async fn backfill_worktree_paths(
    storage: &Arc<StorageClient>,
    search_pool: Option<&SqlitePool>,
) -> WorktreePathBackfillStats {
    let mut stats = WorktreePathBackfillStats::default();

    for collection in PATH_COLLECTIONS {
        repair_collection(storage, collection, &mut stats).await;
    }

    if let Some(pool) = search_pool {
        match repair_search_db(pool).await {
            Ok(n) => stats.search_rows_repaired = n,
            Err(e) => {
                stats.failures += 1;
                warn!("[worktree-path-backfill] search.db repair failed: {}", e);
            }
        }
    }

    if stats.points_repaired > 0 || stats.search_rows_repaired > 0 || stats.failures > 0 {
        info!(
            points_scanned = stats.points_scanned,
            points_repaired = stats.points_repaired,
            search_rows_repaired = stats.search_rows_repaired,
            failures = stats.failures,
            "[worktree-path-backfill] re-anchored stored paths to the main tree"
        );
    }
    stats
}

async fn repair_collection(
    storage: &Arc<StorageClient>,
    collection: &str,
    stats: &mut WorktreePathBackfillStats,
) {
    let mut offset: Option<qdrant_client::qdrant::PointId> = None;
    loop {
        // `file_path` carries a full-text index, so a `text` match on the
        // segment narrows the scan server-side; the exact substring test below
        // is what decides, so a looser index match costs nothing but a read.
        let filter = Filter::must([Condition::matches_text(
            "file_path",
            ".claude/worktrees".to_string(),
        )]);
        let page = match storage
            .scroll_with_filter(collection, filter, SCROLL_PAGE, offset.clone())
            .await
        {
            Ok(p) => p,
            Err(e) => {
                stats.failures += 1;
                warn!(
                    "[worktree-path-backfill] scroll failed for {}: {} — leaving the rest for the next start",
                    collection, e
                );
                return;
            }
        };
        if page.is_empty() {
            return;
        }
        let last_id = page.last().and_then(|p| p.id.clone());

        for point in &page {
            stats.points_scanned += 1;
            let Some(stored) = point
                .payload
                .get("file_path")
                .and_then(|v| v.as_str().map(|s| s.to_string()))
            else {
                continue;
            };
            let Some(main_path) = main_anchored_form(&stored) else {
                continue;
            };
            let Some(id) = point.id.as_ref().and_then(point_id_string) else {
                continue;
            };
            let tenant = point
                .payload
                .get("tenant_id")
                .and_then(|v| v.as_str().map(|s| s.to_string()));

            let mut payload = std::collections::HashMap::new();
            payload.insert("file_path".to_string(), serde_json::json!(main_path));
            payload.insert("absolute_path".to_string(), serde_json::json!(main_path));
            // The document id derives from the path, so it split with it.
            if let Some(tenant_id) = tenant {
                payload.insert(
                    "document_id".to_string(),
                    serde_json::json!(crate::generate_document_id(&tenant_id, &main_path)),
                );
            }

            match storage
                .set_payload_on_point(collection, &id, payload)
                .await
            {
                Ok(()) => stats.points_repaired += 1,
                Err(e) => {
                    stats.failures += 1;
                    warn!(
                        "[worktree-path-backfill] set_payload failed for {} in {}: {}",
                        id, collection, e
                    );
                }
            }
        }

        if page.len() < SCROLL_PAGE as usize {
            return;
        }
        offset = last_id;
    }
}

fn point_id_string(id: &qdrant_client::qdrant::PointId) -> Option<String> {
    use qdrant_client::qdrant::point_id::PointIdOptions;
    match id.point_id_options.as_ref()? {
        PointIdOptions::Uuid(u) => Some(u.clone()),
        PointIdOptions::Num(n) => Some(n.to_string()),
    }
}

/// search.db `file_metadata.file_path` is the path grep reports and the
/// `pathGlob` matcher anchors against, so it carries the same defect.
async fn repair_search_db(pool: &SqlitePool) -> Result<u64, sqlx::Error> {
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT file_id, file_path FROM file_metadata WHERE file_path LIKE ?1",
    )
    .bind(format!("%{}%", WORKTREE_SEGMENT))
    .fetch_all(pool)
    .await?;

    let mut repaired = 0u64;
    for (file_id, stored) in rows {
        let Some(main_path) = main_anchored_form(&stored) else {
            continue;
        };
        sqlx::query("UPDATE file_metadata SET file_path = ?1 WHERE file_id = ?2")
            .bind(&main_path)
            .bind(file_id)
            .execute(pool)
            .await?;
        repaired += 1;
    }
    Ok(repaired)
}

#[cfg(test)]
mod tests {
    use super::main_anchored_form;

    #[test]
    fn strips_the_worktree_prefix() {
        assert_eq!(
            main_anchored_form("/home/u/repo/.claude/worktrees/pr-x/src/a.rs").as_deref(),
            Some("/home/u/repo/src/a.rs")
        );
    }

    #[test]
    fn strips_a_nested_relative_path() {
        assert_eq!(
            main_anchored_form("/repo/.claude/worktrees/wt/a/b/c.dart").as_deref(),
            Some("/repo/a/b/c.dart")
        );
    }

    #[test]
    fn leaves_a_main_tree_path_alone() {
        assert_eq!(main_anchored_form("/home/u/repo/src/a.rs"), None);
    }

    #[test]
    fn refuses_a_malformed_entry_instead_of_mangling_it() {
        // No path component after the worktree name: rewriting this would
        // invent a path. Leave it for a human.
        assert_eq!(main_anchored_form("/repo/.claude/worktrees/wt"), None);
        assert_eq!(main_anchored_form("/repo/.claude/worktrees/wt/"), None);
    }

    #[test]
    fn is_idempotent_on_an_already_repaired_path() {
        let once = main_anchored_form("/repo/.claude/worktrees/wt/src/a.rs").unwrap();
        assert_eq!(main_anchored_form(&once), None, "second pass is a no-op");
    }
}
