//! Where tip versions live while the queue reads them: one directory per
//! staged tip on the daemon's data volume, swept once no queue item names it.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tracing::debug;

/// A staged tip no queue item reads any more is removed once it is this old —
/// long enough that a tip staged this pass is never swept before its items land.
const STAGE_GRACE: Duration = Duration::from_secs(600);

/// `<stage_root>/<tenant>/<branch key>/<tip>`: one directory per staged tip. The
/// branch is keyed by hash — a branch name holds `/` and arbitrary characters.
pub(super) fn stage_dir(stage_root: &Path, tenant_id: &str, branch: &str, tip: &str) -> PathBuf {
    let key = format!("{:x}", Sha256::digest(branch.as_bytes()));
    stage_root.join(tenant_id).join(&key[..16]).join(tip)
}

/// Write `bytes` at `dir/rel`. `rel` comes from the index or a git tree and
/// becomes a path under the stage: plain components only.
pub(super) fn stage_file(dir: &Path, rel: &str, bytes: &[u8]) -> bool {
    let plain = !rel.is_empty()
        && Path::new(rel)
            .components()
            .all(|c| matches!(c, Component::Normal(_)));
    if !plain {
        return false;
    }
    let path = dir.join(rel);
    path.parent()
        .is_some_and(|parent| std::fs::create_dir_all(parent).is_ok())
        && std::fs::write(&path, bytes).is_ok()
}

/// Remove staged tips no queue item reads any more: older than
/// [`STAGE_GRACE`], not staged this pass, and named in no item's metadata.
pub(super) async fn sweep_stage(
    pool: &SqlitePool,
    stage_root: &Path,
    staged_now: &HashSet<PathBuf>,
) {
    for tip_dir in staged_tip_dirs(stage_root) {
        if staged_now.contains(&tip_dir) {
            continue;
        }
        let old_enough = std::fs::metadata(&tip_dir)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age >= STAGE_GRACE);
        if !old_enough {
            continue;
        }
        let needle = tip_dir.to_string_lossy().to_string();
        let referenced: Result<Option<i64>, sqlx::Error> =
            sqlx::query_scalar("SELECT 1 FROM unified_queue WHERE instr(metadata, ?1) > 0 LIMIT 1")
                .bind(&needle)
                .fetch_optional(pool)
                .await;
        if let Ok(None) = referenced {
            if let Err(e) = std::fs::remove_dir_all(&tip_dir) {
                debug!("branch tips: removing {} failed: {}", needle, e);
            }
        }
    }
}

/// Every `<tenant>/<branch key>/<tip>` directory under the stage root.
fn staged_tip_dirs(stage_root: &Path) -> Vec<PathBuf> {
    let subdirs = |dir: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default()
    };
    subdirs(stage_root)
        .iter()
        .flat_map(|tenant| subdirs(tenant))
        .flat_map(|branch| subdirs(&branch))
        .collect()
}
