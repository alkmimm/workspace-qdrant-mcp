//! Branch-membership reconcile for linked git **worktrees** (issue: worktree
//! coverage, "Option B1").
//!
//! ## The gap this closes
//!
//! A linked worktree checked out on branch `X` shares the main repo's canonical
//! tenant, but its content is only searchable under `X` if `X` appears in the
//! `tracked_files.branches` authority. Registration is session-triggered
//! (`try_worktree_auto_register` fires on an MCP `RegisterProject` whose cwd is
//! the worktree), so a worktree created by tooling that never opens a session
//! there — e.g. a parallel `/batch` worktree — leaves `X`'s baseline untagged:
//! a branch-scoped search from that worktree returns only `main`-widened hits.
//!
//! ## Why this is dedup-safe (reuses the main folder, never duplicates)
//!
//! Cross-branch dedup is keyed by `(watch_folder_id, relative_path, file_hash)`
//! (see `strategies/processing/file/branch_dedup.rs`). We therefore reconcile
//! against the **main** repo's `watch_folder_id`, not a per-worktree one: a file
//! whose content is identical to what the main tree already indexed resolves to
//! the same `base_point` and every `point_id`, so the ingest fast-path just
//! appends `X` to the shared points' `branch` array — no new vectors, no
//! re-embed, no duplicate points. Registering the worktree as its own
//! watch_folder would break that key and re-embed / clobber tags instead (the
//! #224/#250 drift class). This is exactly the "worktrees own no `tracked_files`
//! — content served by the main folder via branch tags" invariant (spec §4).
//!
//! ## Scope
//!
//! Two candidate sets, kept disjoint by membership in `tracked_files`:
//!
//! - **Shared baseline** (B1) — every path the main folder already tracks that
//!   also exists in the worktree tree, read from the MAIN tree.
//! - **New-on-branch** (B1.1) — files that exist ONLY on the worktree branch
//!   (no `tracked_files` row under the main folder), read from the WORKTREE
//!   tree via a `read_root` on the item. Their bytes have no main-tree copy, so
//!   the baseline path cannot reach them.
//!
//! Still deferred: a file that *diverges* on the worktree branch (tracked under
//! the main folder but edited on the branch) is tagged with the main tree's
//! bytes (baseline inheritance, matching the read-side #151 auto-widen);
//! capturing that delta needs the shared-baseline read to move to the worktree
//! tree too (B2). The live file-watcher still captures edits in an *active*
//! worktree session.
//!
//! Runs on every tenant scan (startup, periodic, reindex), right after the main
//! branch's own `reconcile_branch_membership`. Idempotent: once a worktree's
//! baseline and branch-only files are tagged, both candidate sets are empty.

use std::path::Path;

use sqlx::SqlitePool;
use tracing::{debug, info, warn};

use wqm_common::constants::COLLECTION_PROJECTS;
use wqm_common::paths::RelativePath;

use crate::queue_operations::QueueManager;
use crate::unified_queue_schema::{FilePayload, ItemType, QueueOperation};

use super::db::fetch_paths_missing_branch;

/// Reconcile branch membership for every linked worktree of a main repository.
///
/// Enumerates the main repo's linked worktrees and, for each one on a concrete
/// branch, enqueues two candidate sets under that branch, both flagged
/// `worktree_membership` so the process-time branch-restamp (#224) does NOT
/// rewrite the authoritative worktree branch back to the main HEAD (without the
/// flag, every worktree file re-stamps to main and the tag never lands):
///
/// 1. **Shared baseline** — paths the main folder already tracks that also
///    exist in the worktree tree, read from the SHARED main tree (storage stays
///    keyed to the main watch_folder, so cross-branch dedup merges identical
///    content — no duplicate point). A file that DIVERGES on the branch is
///    tagged with the main tree's content (baseline inheritance, matching the
///    read-side #151 auto-widen — a follow-up moves this to the worktree tree).
/// 2. **New-on-branch** — files with no `tracked_files` row under the main
///    folder (content that exists only on the worktree branch), read from the
///    worktree tree via a `read_root` on the item; storage is still keyed to
///    the main watch_folder.
///
/// Both sets intersect the worktree's working tree, so a path deleted on the
/// worktree branch is never tagged.
///
/// Detached-HEAD worktrees and trees the daemon cannot read are skipped.
/// `projects`-only (the only branch-scoped collection). Returns the total files
/// enqueued.
pub async fn reconcile_worktree_branches(
    pool: &SqlitePool,
    queue_manager: &QueueManager,
    main_watch_folder_id: &str,
    tenant_id: &str,
    collection: &str,
    main_project_root: &str,
) -> usize {
    if collection != COLLECTION_PROJECTS {
        return 0;
    }

    let mut total = 0usize;
    for wt in crate::git::list_linked_worktrees(Path::new(main_project_root)) {
        let Some(branch) = wt.branch else {
            continue; // detached HEAD: no branch to tag
        };

        // Fold the git-recorded path (possibly a `\\wsl.localhost\...` UNC form
        // from a Windows-host `git worktree add`) to the daemon's native view.
        // CATEGORY-B: used only for the process-local `is_dir()`/existence check;
        // never persisted (reads happen from the main tree, not this path).
        let wt_root = canonicalize_host_path(&wt.root.to_string_lossy());

        // Defensive: the main tree is reconciled by its own caller; skip it.
        if wt_root == main_project_root {
            continue;
        }

        // The daemon must be able to read the worktree tree to check per-file
        // existence; if not (e.g. an unfolded UNC path, or a stale admin entry
        // for a removed worktree), there is nothing to reconcile.
        if !Path::new(&wt_root).is_dir() {
            debug!(
                "worktree membership: tree {} for branch '{}' not readable by the daemon; skipping",
                wt_root, branch
            );
            continue;
        }

        // (a) Shared baseline: paths the main folder already tracks that also
        // exist in the worktree tree, read from the MAIN tree (dedup merges).
        let n_baseline = enqueue_worktree_membership(
            pool,
            queue_manager,
            main_watch_folder_id,
            tenant_id,
            collection,
            &wt_root,
            &branch,
        )
        .await;
        if n_baseline > 0 {
            info!(
                "worktree membership: enqueued {} baseline file(s) under branch '{}' (worktree {})",
                n_baseline, branch, wt_root
            );
            crate::monitoring::metrics_core::METRICS
                .worktree_membership_enqueued_total
                .with_label_values(&[tenant_id, branch.as_str()])
                .inc_by(n_baseline as u64);
        }

        // (b) New-on-branch: files that exist ONLY on the worktree branch (no
        // tracked_files row under the main folder). Their bytes live solely in
        // the worktree tree, so these items carry a `read_root` and the
        // processor reads from the worktree (storage still keyed to the main
        // folder). This is what the shared-baseline path cannot do — reading
        // from the main tree would resolve to a missing file.
        let n_new = enqueue_worktree_new_on_branch(
            pool,
            queue_manager,
            main_watch_folder_id,
            tenant_id,
            collection,
            &wt_root,
            &branch,
        )
        .await;
        if n_new > 0 {
            info!(
                "worktree membership: enqueued {} new-on-branch file(s) under branch '{}' (worktree {})",
                n_new, branch, wt_root
            );
            crate::monitoring::metrics_core::METRICS
                .worktree_membership_new_on_branch_enqueued_total
                .with_label_values(&[tenant_id, branch.as_str()])
                .inc_by(n_new as u64);
        }

        total += n_baseline + n_new;
    }
    total
}

/// Enqueue the worktree's working-tree files missing `branch` as `File/Add`
/// items keyed to the MAIN watch_folder, flagged `worktree_membership`.
///
/// Mirrors [`super::reconcile_branch_membership`]'s candidate loop. Reads happen
/// from the main tree (the item is anchored to the main watch_folder); the
/// `worktree_membership` flag stops the process-time restamp from rewriting the
/// branch. Files absent from the worktree working tree are skipped — never
/// enqueued — so this never resurrects a path deleted on the worktree branch.
/// Returns the number of files enqueued.
async fn enqueue_worktree_membership(
    pool: &SqlitePool,
    queue_manager: &QueueManager,
    main_watch_folder_id: &str,
    tenant_id: &str,
    collection: &str,
    wt_root: &str,
    branch: &str,
) -> usize {
    let candidates = match fetch_paths_missing_branch(pool, main_watch_folder_id, branch).await {
        Ok(v) => v,
        Err(e) => {
            warn!(
                "worktree membership: fetch candidates failed for branch '{}': {}",
                branch, e
            );
            return 0;
        }
    };
    if candidates.is_empty() {
        return 0;
    }

    let wt = Path::new(wt_root);
    let mut enqueued = 0usize;
    for rel_str in candidates {
        if !wt.join(&rel_str).exists() {
            continue;
        }
        let rel = match RelativePath::from_user_input(&rel_str) {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    "worktree membership: skipping invalid relative path {:?}: {}",
                    rel_str, e
                );
                continue;
            }
        };
        // Shared baseline reads from the MAIN tree → no `read_root`.
        match enqueue_worktree_file(queue_manager, tenant_id, collection, &rel, branch, None).await
        {
            Ok(()) => enqueued += 1,
            Err(e) => warn!(
                "worktree membership: enqueue {} on '{}' failed: {}",
                rel_str, branch, e
            ),
        }
    }
    enqueued
}

/// Enqueue files that exist ONLY on the worktree branch — no `tracked_files`
/// row under the main folder — reading their bytes from the worktree tree.
///
/// The shared-baseline reconcile ([`enqueue_worktree_membership`]) can only tag
/// paths the main folder already tracks, because it reads from the main tree;
/// a file added on the worktree branch has no main-tree copy, so its content
/// must come from the worktree. Discovery walks the worktree working tree with
/// the project `.gitignore`/`.wqmignore` cascade only (no `global.wqmignore`:
/// its `.claude/worktrees/` rule matches absolute paths and their parents, so it
/// would self-exclude the whole worktree even rooted inside it) and subtracts
/// every path already tracked under the main folder. What remains is
/// branch-only content; each item carries `read_root` so the processor anchors
/// its reads at the worktree tree while storage stays keyed to the main
/// watch_folder (cross-branch dedup + branch-scoped idempotency unchanged).
///
/// Subtracting the full `tracked_files` set keeps this disjoint from the
/// baseline path: a shared file (baseline, read from main) and a branch-only
/// file (read from the worktree) never contend for the same `(path, branch)`
/// idempotency key. Once tagged, a branch-only file gains a `tracked_files` row
/// and drops out of the candidate set — idempotent across scans. Returns the
/// number of files enqueued.
async fn enqueue_worktree_new_on_branch(
    pool: &SqlitePool,
    queue_manager: &QueueManager,
    main_watch_folder_id: &str,
    tenant_id: &str,
    collection: &str,
    wt_root: &str,
    branch: &str,
) -> usize {
    // Walk with project-cascade ignore only (`None` global) — see doc above.
    let walked = match crate::startup::reconciliation::ignore_sync::walk_eligible_files(
        Path::new(wt_root),
        None,
    ) {
        Ok(set) => set,
        Err(e) => {
            warn!(
                "worktree membership: walk of worktree tree {} for branch '{}' failed: {}",
                wt_root, branch, e
            );
            return 0;
        }
    };
    if walked.is_empty() {
        return 0;
    }

    let tracked = match super::db::fetch_all_tracked_paths(pool, main_watch_folder_id).await {
        Ok(set) => set,
        Err(e) => {
            warn!(
                "worktree membership: fetch tracked paths failed for branch '{}': {}",
                branch, e
            );
            return 0;
        }
    };

    let mut enqueued = 0usize;
    for rel_str in walked {
        // Skip anything the main folder already tracks (shared baseline or a
        // path tagged on another branch) — not a branch-only candidate.
        if tracked.contains(&rel_str) {
            continue;
        }
        let rel = match RelativePath::from_user_input(&rel_str) {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    "worktree membership: skipping invalid relative path {:?}: {}",
                    rel_str, e
                );
                continue;
            }
        };
        // Branch-only content reads from the WORKTREE tree → carry `read_root`.
        match enqueue_worktree_file(
            queue_manager,
            tenant_id,
            collection,
            &rel,
            branch,
            Some(wt_root),
        )
        .await
        {
            Ok(()) => enqueued += 1,
            Err(e) => warn!(
                "worktree membership: enqueue new-on-branch {} on '{}' failed: {}",
                rel_str, branch, e
            ),
        }
    }
    enqueued
}

/// Enqueue a single `File/Add` for a worktree file, flagged
/// `worktree_membership` in the item metadata so the processor keeps the
/// authoritative worktree branch (no restamp) and storage stays keyed to the
/// resolved main watch_folder.
///
/// `read_root` selects where the processor reads the bytes: `None` for a shared
/// baseline file (read from the main tree — cross-branch dedup merges), or
/// `Some(worktree_root)` for a branch-only file whose content lives solely in
/// the worktree tree. When set, the processor anchors its reads there and skips
/// the dequeue ignore-gate (the daemon-curated path already applied the
/// worktree's own ignore cascade at discovery). The stored `read_root` is the
/// worktree's on-disk root; the item's `file_path` stays repo-relative so the
/// storage key (`watch_folder_id`, `relative_path`, `file_hash`) is unaffected.
async fn enqueue_worktree_file(
    queue_manager: &QueueManager,
    tenant_id: &str,
    collection: &str,
    rel: &RelativePath,
    branch: &str,
    read_root: Option<&str>,
) -> Result<(), String> {
    let payload = FilePayload {
        file_path: rel.clone(),
        file_type: None,
        file_hash: None,
        size_bytes: None,
        old_path: None,
    };
    let payload_json =
        serde_json::to_string(&payload).map_err(|e| format!("serialize FilePayload: {e}"))?;
    let metadata = match read_root {
        Some(root) => {
            serde_json::json!({ "worktree_membership": true, "read_root": root }).to_string()
        }
        None => serde_json::json!({ "worktree_membership": true }).to_string(),
    };
    queue_manager
        .enqueue_unified(
            ItemType::File,
            QueueOperation::Add,
            tenant_id,
            collection,
            &payload_json,
            Some(branch),
            Some(metadata.as_str()),
        )
        .await
        .map(|_| ())
        .map_err(|e| format!("enqueue: {e}"))
}

/// Fold a host-reported path to the daemon's native POSIX view.
///
/// Mirrors the WSL-UNC arm of the TypeScript `canonicalizeHostPath`
/// (`src/typescript/mcp-server/src/clients/project-queries.ts`): a worktree
/// created from a Windows host records a `\\wsl.localhost\<distro>\home\…`
/// (or the legacy `\\wsl$\<distro>\…`) gitdir, but the daemon runs inside the
/// distro and reads the native `/home/…` path. Backslashes fold to `/`, the
/// `wsl.localhost`/`wsl$` share + distro segments are dropped, and duplicate /
/// trailing slashes are collapsed. Native POSIX paths pass through unchanged.
fn canonicalize_host_path(raw: &str) -> String {
    let slashed = raw.replace('\\', "/");
    let trimmed = slashed.trim_start_matches('/');
    let lower = trimmed.to_ascii_lowercase();

    let after_share = if lower.starts_with("wsl.localhost/") {
        Some(&trimmed["wsl.localhost/".len()..])
    } else if lower.starts_with("wsl$/") {
        Some(&trimmed["wsl$/".len()..])
    } else {
        None
    };

    let folded = match after_share {
        // Drop the `<distro>` segment right after the share host.
        Some(after) => match after.split_once('/') {
            Some((_distro, tail)) => format!("/{tail}"),
            None => "/".to_string(),
        },
        None => slashed.clone(),
    };

    collapse_slashes(&folded)
}

/// Collapse runs of `/` to a single separator and drop a trailing slash,
/// preserving the root `/`.
fn collapse_slashes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_slash = false;
    for c in s.chars() {
        if c == '/' {
            if !prev_slash {
                out.push('/');
            }
            prev_slash = true;
        } else {
            out.push(c);
            prev_slash = false;
        }
    }
    if out.len() > 1 && out.ends_with('/') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{canonicalize_host_path, collapse_slashes};

    #[test]
    fn wsl_unc_folds_to_native_posix() {
        assert_eq!(
            canonicalize_host_path(
                "\\\\wsl.localhost\\ubuntu-24.04\\home\\me\\repo\\.claude\\worktrees\\wt"
            ),
            "/home/me/repo/.claude/worktrees/wt"
        );
        // Forward-slash form (git may already store it normalized).
        assert_eq!(
            canonicalize_host_path("//wsl.localhost/Ubuntu-24.04/home/me/repo"),
            "/home/me/repo"
        );
        // Legacy `wsl$` share, case-insensitive host.
        assert_eq!(
            canonicalize_host_path("\\\\WSL$\\ubuntu\\home\\x"),
            "/home/x"
        );
    }

    #[test]
    fn native_posix_passes_through() {
        assert_eq!(
            canonicalize_host_path("/home/me/repo/.claude/worktrees/wt"),
            "/home/me/repo/.claude/worktrees/wt"
        );
    }

    #[test]
    fn collapses_and_trims_slashes() {
        assert_eq!(collapse_slashes("/home//me///x/"), "/home/me/x");
        assert_eq!(collapse_slashes("/"), "/");
        assert_eq!(collapse_slashes("/home/x"), "/home/x");
    }
}
