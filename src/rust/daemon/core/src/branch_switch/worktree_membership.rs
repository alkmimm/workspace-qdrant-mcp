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
//! ## Scope (B1)
//!
//! This tags the worktree branch's **shared baseline** — every path the main
//! folder already tracks that also exists in the worktree tree. Files that
//! genuinely *diverge* on the worktree branch read the main tree's bytes here
//! (the enqueue path anchors to the shared watch_folder root), so their delta is
//! not captured by B1; that is B2's job (an explicit worktree read-root). The
//! live file-watcher still captures edits in an *active* worktree session.
//!
//! Runs on every tenant scan (startup, periodic, reindex), right after the main
//! branch's own `reconcile_branch_membership`. Idempotent and cheap: once a
//! worktree's baseline is tagged, its candidate set is empty.

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
/// branch, enqueues its baseline files under that branch. The bytes are read
/// from the SHARED main tree (storage stays keyed to the main watch_folder, so
/// cross-branch dedup merges identical content — no duplicate point), and each
/// item is flagged `worktree_membership` so the process-time branch-restamp
/// (#224) does NOT rewrite the authoritative worktree branch back to the main
/// HEAD (without the flag, every worktree file re-stamps to main and the tag
/// never lands).
///
/// Only files that ALSO exist in the worktree's working tree are enqueued, so a
/// path deleted on the worktree branch is never tagged. Reading from the main
/// tree — rather than the worktree subtree, which lives under the globally
/// ignored `.claude/worktrees/` and would be dropped by the dequeue ignore-gate
/// — is deliberate: a file that DIVERGES on the worktree branch is tagged with
/// the main tree's content (baseline inheritance, matching the read-side #151
/// auto-widen). Capturing a worktree's own delta, and files that exist only on
/// the worktree branch, are follow-ups.
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

        let n = enqueue_worktree_membership(
            pool,
            queue_manager,
            main_watch_folder_id,
            tenant_id,
            collection,
            &wt_root,
            &branch,
        )
        .await;
        if n > 0 {
            info!(
                "worktree membership: enqueued {} baseline file(s) under branch '{}' (worktree {})",
                n, branch, wt_root
            );
        }
        total += n;
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
        match enqueue_worktree_file(queue_manager, tenant_id, collection, &rel, branch).await {
            Ok(()) => enqueued += 1,
            Err(e) => warn!(
                "worktree membership: enqueue {} on '{}' failed: {}",
                rel_str, branch, e
            ),
        }
    }
    enqueued
}

/// Enqueue a single `File/Add` for a worktree baseline file, flagged
/// `worktree_membership` in the item metadata so the processor keeps the
/// authoritative worktree branch (no restamp) while reading the bytes from, and
/// storing under, the resolved main watch_folder.
async fn enqueue_worktree_file(
    queue_manager: &QueueManager,
    tenant_id: &str,
    collection: &str,
    rel: &RelativePath,
    branch: &str,
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
    let metadata = serde_json::json!({ "worktree_membership": true }).to_string();
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
