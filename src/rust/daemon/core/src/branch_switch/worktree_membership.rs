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
//! Three candidate sets per worktree branch, kept mutually disjoint:
//!
//! - **Shared baseline** (B1) — paths the main folder tracks that also exist in
//!   the worktree tree AND are byte-identical to main (not in the divergent
//!   set), read from the MAIN tree (cross-branch dedup merges — no new vectors).
//! - **New-on-branch** (B1.1) — files that exist ONLY on the worktree branch (no
//!   `tracked_files` row under the main folder), read from the WORKTREE tree via
//!   a `read_root` on the item. Their bytes have no main-tree copy, so the
//!   baseline path cannot reach them.
//! - **Divergent** (B2) — shared files whose content DIFFERS on the branch (git
//!   reports them `Modified` between the main HEAD and the branch tip), read from
//!   the WORKTREE tree via `read_root`. The ingest writes a new content-row
//!   `(relative_path, file_hash)` tagged with the branch; the main content-row is
//!   a different hash, left untouched. The baseline SKIPS these (reading from
//!   main would tag the branch with main's bytes and collide on the shared
//!   `(path, branch)` idempotency key).
//!
//! Disjointness: baseline = shared ∧ identical; divergent = shared ∧ Modified;
//! new-on-branch = untracked (git `Added`). Only *committed* divergence is
//! captured (commit-to-commit diff); the live file-watcher covers an *active*
//! worktree session's uncommitted edits.
//!
//! This module builds the three candidate sets; WHEN a worktree is reconciled
//! is [`super::worktree_discovery`]'s job: on every tenant scan, right after the
//! main branch's own `reconcile_branch_membership`, and as soon as a worktree
//! appears or its branch tip moves. Idempotent: once a worktree's files are
//! tagged with their correct content, all three candidate sets settle.

use std::collections::HashSet;
use std::path::Path;

use sqlx::SqlitePool;
use tracing::warn;

use wqm_common::paths::RelativePath;

use crate::allowed_extensions::AllowedExtensions;
use crate::queue_operations::QueueManager;
use crate::unified_queue_schema::{FilePayload, ItemType, QueueOperation};

use super::db::fetch_paths_missing_branch;

/// Enqueue the worktree's working-tree files missing `branch` as `File/Add`
/// items keyed to the MAIN watch_folder, flagged `worktree_membership`.
///
/// Mirrors [`super::reconcile_branch_membership`]'s candidate loop. Reads happen
/// from the main tree (the item is anchored to the main watch_folder); the
/// `worktree_membership` flag stops the process-time restamp from rewriting the
/// branch. Files absent from the worktree working tree are skipped — never
/// enqueued — so this never resurrects a path deleted on the worktree branch.
/// Returns the number of files enqueued.
pub(super) async fn enqueue_worktree_membership(
    pool: &SqlitePool,
    queue_manager: &QueueManager,
    main_watch_folder_id: &str,
    tenant_id: &str,
    collection: &str,
    wt_root: &str,
    branch: &str,
    divergent: &HashSet<String>,
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
        // Divergent files are handled by `enqueue_worktree_divergent` (read from
        // the worktree); reading them from main here would tag the branch with
        // main's bytes and collide on the shared `(path, branch)` idempotency key.
        if divergent.contains(&rel_str) {
            continue;
        }
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
/// must come from the worktree. Discovery enumerates the worktree working tree
/// with the project `.gitignore`/`.wqmignore` cascade only — passing `None`
/// global to the walk, because `global.wqmignore`'s `.claude/worktrees/` rule
/// matches absolute paths and their parents and would self-exclude the whole
/// worktree even rooted inside it. It then subtracts every path already tracked
/// under the main folder and applies the MAIN folder's full eligibility to each
/// survivor: the ignore gate (project cascade + `global.wqmignore`) anchored at
/// the main root — which re-adds the global layer the walk had to drop, so a
/// worktree branch never indexes generated / globally-excluded files the main
/// scan omits, while the main anchor (`main_root/rel`, never `worktree_root/rel`)
/// keeps the `.claude/worktrees/` rule from self-excluding — plus the
/// extension/filename allowlist, so disallowed types (certs, keystores, lock
/// files) are dropped at discovery instead of enqueued only to skip at ingest. What remains is branch-only
/// content; each item carries `read_root` so the processor anchors its reads at
/// the worktree tree while storage stays keyed to the main watch_folder
/// (cross-branch dedup + branch-scoped idempotency unchanged).
///
/// Subtracting the full `tracked_files` set keeps this disjoint from the
/// baseline path: a shared file (baseline, read from main) and a branch-only
/// file (read from the worktree) never contend for the same `(path, branch)`
/// idempotency key. Once tagged, a branch-only file gains a `tracked_files` row
/// and drops out of the candidate set — idempotent across scans. Returns the
/// number of files enqueued.
pub(super) async fn enqueue_worktree_new_on_branch(
    pool: &SqlitePool,
    queue_manager: &QueueManager,
    main_watch_folder_id: &str,
    tenant_id: &str,
    collection: &str,
    wt_root: &str,
    main_project_root: &str,
    branch: &str,
    allowed_extensions: &AllowedExtensions,
) -> usize {
    // Enumerate the worktree working tree with the project-cascade ignore only
    // (`None` global — see doc above); the global layer is re-applied below via
    // the main-anchored gate.
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

    let gate = main_eligibility_gate(main_project_root);

    let mut enqueued = 0usize;
    for rel_str in walked {
        // Skip anything the main folder already tracks (shared baseline or a
        // path tagged on another branch) — not a branch-only candidate.
        if tracked.contains(&rel_str) {
            continue;
        }
        // Mirror the main scan's eligibility (ignore gate + extension allowlist)
        // so a worktree branch never indexes generated / globally-excluded /
        // disallowed-type files the main scan omits (see `worktree_path_eligible`).
        if !worktree_path_eligible(
            main_project_root,
            &gate,
            allowed_extensions,
            collection,
            &rel_str,
        ) {
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

/// Enqueue shared files whose content DIVERGES on the worktree branch, reading
/// their bytes from the worktree tree so the branch is indexed with its OWN
/// content instead of inheriting the main tree's (B2).
///
/// The baseline path reads shared files from the main tree — correct for files
/// byte-identical across branches (dedup merges), but for a file edited on the
/// branch it would tag the branch with the MAIN content (the branch would search
/// stale bytes; baseline inheritance / read-side #151 auto-widen). `divergent` is
/// the set git reports as `Modified` between the main HEAD and the branch tip
/// ([`crate::git::modified_paths_head_vs_branch`]); the baseline is told to SKIP
/// them and they are enqueued here with a `read_root`, so the processor reads the
/// worktree copy. The ingest computes the worktree bytes' hash and writes a NEW
/// `(relative_path, file_hash)` content-generation tagged with the branch; the
/// main generation is a different hash → a different row, left untouched (the
/// branch-scoped, reference-counted delete never GCs content another branch still
/// holds — see `update_preamble`). Each candidate is filtered through the main
/// folder's eligibility (gate + allowlist), so a modified generated / disallowed
/// file the diff surfaces is not indexed. Returns the number of files enqueued.
#[allow(clippy::too_many_arguments)]
pub(super) async fn enqueue_worktree_divergent(
    queue_manager: &QueueManager,
    tenant_id: &str,
    collection: &str,
    wt_root: &str,
    main_project_root: &str,
    branch: &str,
    divergent: &HashSet<String>,
    allowed_extensions: &AllowedExtensions,
) -> usize {
    if divergent.is_empty() {
        return 0;
    }
    let gate = main_eligibility_gate(main_project_root);
    let wt = Path::new(wt_root);
    let mut enqueued = 0usize;
    for rel_str in divergent {
        // A `Modified` delta is present in the worktree tree, but stay defensive.
        if !wt.join(rel_str).exists() {
            continue;
        }
        // Mirror the main scan's eligibility so a modified generated / disallowed
        // file the diff surfaces is never indexed under the branch.
        if !worktree_path_eligible(
            main_project_root,
            &gate,
            allowed_extensions,
            collection,
            rel_str,
        ) {
            continue;
        }
        let rel = match RelativePath::from_user_input(rel_str) {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    "worktree membership: skipping invalid relative path {:?}: {}",
                    rel_str, e
                );
                continue;
            }
        };
        // Divergent content reads from the WORKTREE tree → carry `read_root`.
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
                "worktree membership: enqueue divergent {} on '{}' failed: {}",
                rel_str, branch, e
            ),
        }
    }
    enqueued
}

/// The MAIN folder's eligibility gate (project `.gitignore`/`.wqmignore` cascade
/// + `global.wqmignore`), anchored at the main root.
///
/// Worktree-read items (new-on-branch, divergent) test each candidate as
/// `main_root/rel` so every global rule applies to the rel path exactly as the
/// main scan would — dropping generated / globally-excluded files — WITHOUT the
/// `.claude/worktrees/` self-exclusion a worktree-anchored test would trigger
/// (that rule matches absolute paths and their parents, so anchoring inside the
/// worktree does not save it). Build once per worktree; reuse across candidates.
fn main_eligibility_gate(main_project_root: &str) -> crate::patterns::ignore_gate::IgnoreGate {
    let main_root = Path::new(main_project_root);
    let global = crate::patterns::global_ignore::resolve_global_ignore_path();
    crate::patterns::ignore_gate::IgnoreGate::for_dir(main_root, Some(main_root), global.as_deref())
}

/// Whether `rel` passes the MAIN folder's full eligibility — the ignore `gate`
/// (from [`main_eligibility_gate`]) plus the extension/filename allowlist — the
/// exact filter the folder scan applies. Worktree-read items run every candidate
/// through this so a worktree branch never indexes files the main scan omits, and
/// disallowed types are dropped at discovery instead of enqueued only to be
/// skipped at the ingest guard and re-churned every scan.
fn worktree_path_eligible(
    main_project_root: &str,
    gate: &crate::patterns::ignore_gate::IgnoreGate,
    allowed_extensions: &AllowedExtensions,
    collection: &str,
    rel: &str,
) -> bool {
    let main_root = Path::new(main_project_root);
    !gate.is_ignored_with_ancestors(main_root, &main_root.join(rel))
        && allowed_extensions.is_allowed(rel, collection)
}

/// Enqueue a single `File/Add` for a worktree file, flagged
/// `worktree_membership` in the item metadata so the processor keeps the
/// authoritative worktree branch (no restamp) and storage stays keyed to the
/// resolved main watch_folder.
///
/// `read_root` selects where the processor reads the bytes: `None` for a shared
/// baseline file (read from the main tree — cross-branch dedup merges), or
/// `Some(worktree_root)` for a branch-only file whose content lives solely in
/// the worktree tree. When set, the processor anchors its reads there; the
/// dequeue ignore-gate still runs, but against the main-anchored path so
/// `global.wqmignore` filters the rel path without self-excluding the worktree.
/// The stored `read_root` is the worktree's on-disk root; the item's
/// `file_path` stays repo-relative so the
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
