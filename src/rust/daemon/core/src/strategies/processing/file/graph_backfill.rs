//! Idle backfill of graph generations the index holds but the graph lacks.
//!
//! Graph rows are per content generation (`base_point`). An ordinary ingest
//! writes its generation's graph, but nothing rewrites an unchanged file — so
//! after the graph.db v7 rebuild, and for any generation whose extraction
//! failed, the graph would stay empty for every file nobody edits. While the
//! queue is idle this walks the tracked generations, finds the ones with no
//! extraction record, and rebuilds each from a checkout that has exactly that
//! version on disk: the main folder for its HEAD branch, a linked worktree for
//! a worktree branch. A generation whose bytes no checkout holds (a branch
//! switched away from) waits until one does; its graph is then built by the
//! dedup heal or by this pass on a later refresh.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sqlx::Row;
use tracing::{debug, info, warn};

use crate::context::ProcessingContext;
use crate::git::BranchCheckouts;
use crate::tracked_files_schema::compute_file_hash;

/// How often the pending list is rebuilt from `tracked_files` once drained.
const REFRESH_INTERVAL: Duration = Duration::from_secs(300);
/// Wall-clock budget of one idle step: the queue may fill meanwhile, so the
/// step yields well before a user-visible delay.
pub(crate) const STEP_BUDGET: Duration = Duration::from_millis(1500);

/// One generation to rebuild.
#[derive(Debug, Clone)]
struct Pending {
    tenant_id: String,
    collection: String,
    watch_root: PathBuf,
    relative_path: String,
    generation: String,
    file_hash: String,
    branches: Vec<String>,
}

/// State of the backfill across idle steps.
#[derive(Debug, Default)]
pub(crate) struct GraphBackfill {
    pending: VecDeque<Pending>,
    refreshed_at: Option<Instant>,
    rebuilt: u64,
    unreachable: u64,
}

impl GraphBackfill {
    /// Rebuild pending generations until `budget` runs out. Returns how many
    /// were rebuilt in this step.
    pub(crate) async fn step(&mut self, ctx: &ProcessingContext, budget: Duration) -> usize {
        let Some(ref graph) = ctx.graph_store else {
            return 0;
        };
        if self.pending.is_empty() && self.refresh_due() {
            self.refresh(ctx).await;
        }
        let started = Instant::now();
        let mut rebuilt = 0usize;
        while started.elapsed() < budget {
            let Some(p) = self.pending.pop_front() else {
                break;
            };
            match graph
                .generation_extracted(&p.tenant_id, &p.generation)
                .await
            {
                Ok(true) => continue, // an ingest got there first
                Ok(false) => {}
                Err(e) => {
                    warn!(error = %e, "graph backfill: generation probe failed — pausing until the next refresh");
                    self.pending.clear();
                    break;
                }
            }
            let Some(abs) = locate_version(&p) else {
                self.unreachable += 1;
                continue;
            };
            let abs_str = abs.to_string_lossy().to_string();
            super::graph_ingest::rebuild_generation(
                ctx,
                &p.tenant_id,
                &p.collection,
                &abs,
                &p.relative_path,
                &abs_str,
                &p.watch_root.to_string_lossy(),
                &p.generation,
            )
            .await;
            rebuilt += 1;
            self.rebuilt += 1;
        }
        if rebuilt > 0 && self.pending.is_empty() {
            info!(
                rebuilt = self.rebuilt,
                unreachable = self.unreachable,
                "Graph backfill drained: every reachable generation has a graph \
                 (unreachable = versions no checkout holds on disk right now)"
            );
        }
        rebuilt
    }

    fn refresh_due(&self) -> bool {
        self.refreshed_at
            .map_or(true, |at| at.elapsed() >= REFRESH_INTERVAL)
    }

    /// Rebuild the pending list: every tracked generation of an enabled watch
    /// folder with no extraction record. Active projects first.
    async fn refresh(&mut self, ctx: &ProcessingContext) {
        self.refreshed_at = Some(Instant::now());
        self.unreachable = 0;
        let Some(ref graph) = ctx.graph_store else {
            return;
        };
        let rows = match sqlx::query(
            "SELECT w.tenant_id, w.path, tf.relative_path, tf.base_point, tf.file_hash,
                    tf.branches, COALESCE(tf.collection, w.collection) AS collection
             FROM tracked_files tf JOIN watch_folders w ON w.watch_id = tf.watch_folder_id
             WHERE tf.base_point IS NOT NULL AND w.enabled = 1
             ORDER BY w.is_active DESC, w.tenant_id, tf.relative_path",
        )
        .fetch_all(&ctx.pool)
        .await
        {
            Ok(rows) => rows,
            Err(e) => {
                warn!(error = %e, "graph backfill: tracked_files query failed");
                return;
            }
        };
        let mut extracted_by_tenant: std::collections::HashMap<String, HashSet<String>> =
            std::collections::HashMap::new();
        let mut queued: HashSet<(String, String)> = HashSet::new();
        for r in &rows {
            let tenant_id: String = r.get("tenant_id");
            if !extracted_by_tenant.contains_key(&tenant_id) {
                let extracted = match graph.extracted_generations(&tenant_id).await {
                    Ok(list) => list.into_iter().map(|g| g.generation).collect(),
                    Err(e) => {
                        warn!(tenant = %tenant_id, error = %e, "graph backfill: listing generations failed");
                        return;
                    }
                };
                extracted_by_tenant.insert(tenant_id.clone(), extracted);
            }
            let generation: String = r.get("base_point");
            if extracted_by_tenant[&tenant_id].contains(&generation)
                || !queued.insert((tenant_id.clone(), generation.clone()))
            {
                continue;
            }
            let raw: Option<String> = r.get("branches");
            self.pending.push_back(Pending {
                tenant_id,
                collection: r.get("collection"),
                watch_root: PathBuf::from(r.get::<String, _>("path")),
                relative_path: r.get("relative_path"),
                generation,
                file_hash: r.get("file_hash"),
                branches: raw
                    .as_deref()
                    .and_then(|j| serde_json::from_str(j).ok())
                    .unwrap_or_default(),
            });
        }
        if self.pending.is_empty() {
            debug!("graph backfill: nothing pending");
        } else {
            info!(
                pending = self.pending.len(),
                "Graph backfill: generations in the index without a graph"
            );
        }
    }
}

/// The on-disk copy of exactly this version: the file at its path in a
/// checkout whose bytes hash to the generation's. The checkouts of the
/// branches that hold the version are tried first; then every other checkout,
/// because a version is often byte-identical elsewhere (a trunk nobody has
/// checked out shares most files with the main folder's branch). A row with
/// no branch set is a legacy row; its one tree is the watch root.
fn locate_version(p: &Pending) -> Option<PathBuf> {
    let checkouts = BranchCheckouts::discover_cached(&p.watch_root);
    let mut roots: Vec<&Path> = if p.branches.is_empty() {
        vec![p.watch_root.as_path()]
    } else {
        p.branches
            .iter()
            .filter_map(|b| checkouts.root_for(b))
            .collect()
    };
    for root in checkouts.all_roots() {
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    roots.into_iter().find_map(|root| {
        let abs = root.join(&p.relative_path);
        let matches =
            abs.is_file() && compute_file_hash(&abs).is_ok_and(|hash| hash == p.file_hash);
        matches.then_some(abs)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(root: &Path, rel: &str, hash: &str, branches: &[&str]) -> Pending {
        Pending {
            tenant_id: "t".into(),
            collection: "projects".into(),
            watch_root: root.to_path_buf(),
            relative_path: rel.into(),
            generation: "g".into(),
            file_hash: hash.into(),
            branches: branches.iter().map(|b| b.to_string()).collect(),
        }
    }

    #[test]
    fn a_version_is_found_only_where_its_bytes_are() {
        // Not a git repository: one tree, whatever label the rows carry.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "fn a() {}\n").unwrap();
        let hash = compute_file_hash(&file).unwrap();

        assert_eq!(
            locate_version(&pending(dir.path(), "a.rs", &hash, &["main"])),
            Some(file.clone())
        );
        // Another version of the same path: the disk holds different bytes.
        assert_eq!(
            locate_version(&pending(dir.path(), "a.rs", "not-this-hash", &["main"])),
            None
        );
        // A path no checkout has.
        assert_eq!(
            locate_version(&pending(dir.path(), "gone.rs", &hash, &["main"])),
            None
        );
        // A branch with no checkout of its own: found wherever the bytes are.
        assert_eq!(
            locate_version(&pending(dir.path(), "a.rs", &hash, &["develop"])),
            Some(file.clone())
        );
        // A legacy row (no branch set) is looked up in the watch root.
        assert_eq!(
            locate_version(&pending(dir.path(), "a.rs", &hash, &[])),
            Some(file)
        );
    }
}
