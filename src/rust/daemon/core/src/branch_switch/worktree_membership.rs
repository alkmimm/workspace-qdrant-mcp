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
//! Runs on every tenant scan (startup, periodic, reindex), right after the main
//! branch's own `reconcile_branch_membership`. Idempotent: once a worktree's
//! files are tagged with their correct content, all three candidate sets settle.

use std::collections::HashSet;
use std::path::Path;

use sqlx::SqlitePool;
use tracing::{debug, info, warn};

use wqm_common::constants::COLLECTION_PROJECTS;
use wqm_common::paths::RelativePath;

use crate::allowed_extensions::AllowedExtensions;
use crate::queue_operations::QueueManager;
use crate::unified_queue_schema::{FilePayload, ItemType, QueueOperation};

use super::db::fetch_paths_missing_branch;

/// Reconcile branch membership for every linked worktree of a main repository.
///
/// Enumerates the main repo's linked worktrees and, for each one on a concrete
/// branch, enqueues three disjoint candidate sets under that branch, all flagged
/// `worktree_membership` so the process-time branch-restamp (#224) does NOT
/// rewrite the authoritative worktree branch back to the main HEAD (without the
/// flag, every worktree file re-stamps to main and the tag never lands):
///
/// 1. **Shared baseline** — paths the main folder tracks that also exist in the
///    worktree tree AND are byte-identical to main (not in the divergent set),
///    read from the SHARED main tree (storage stays keyed to the main
///    watch_folder, so cross-branch dedup merges identical content — no
///    duplicate point).
/// 2. **New-on-branch** — files with no `tracked_files` row under the main
///    folder (content that exists only on the worktree branch), read from the
///    worktree tree via a `read_root` on the item; storage is still keyed to
///    the main watch_folder.
/// 3. **Divergent** — shared files whose content DIFFERS on the branch (git
///    `Modified` between the main HEAD and the branch tip), read from the
///    worktree tree via `read_root` so the branch is indexed with its OWN bytes
///    (a new content-generation), not main's. The baseline skips these.
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
    allowed_extensions: &AllowedExtensions,
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

        // Guard: the root must be a genuine linked-worktree *checkout*, not a
        // stale/malformed admin entry whose `gitdir` resolved to a non-worktree
        // directory. Observed 2026-08-06: an admin entry whose gitdir parent was
        // the `.claude/worktrees` *container* (an ancestor of every sub-worktree)
        // passed both guards above — `is_dir()` is true and it is not the main
        // root — so the walk below enumerated EVERY file of EVERY sub-worktree,
        // enqueuing ~89k phantom "new-on-branch" items (relative paths prefixed
        // with the worktree name, which can never match the main tracked set)
        // under one wrong branch. A real linked worktree always has a `.git`
        // gitlink FILE (`gitdir: <main>/.git/worktrees/<name>`); the container
        // has no `.git`, and a main repo has `.git` as a directory — so
        // `is_leaf_worktree_root` rejects both.
        if !is_leaf_worktree_root(Path::new(&wt_root)) {
            warn!(
                "worktree membership: {} for branch '{}' is not a leaf worktree checkout \
                 (no `.git` gitlink file); skipping to avoid a container-wide phantom walk",
                wt_root, branch
            );
            continue;
        }

        // Committed divergence: paths git reports as Modified between the main
        // HEAD and this branch's tip. The baseline SKIPS these (reading them from
        // the main tree would tag the branch with main's bytes); (c) below reads
        // them from the worktree so the branch is indexed with its OWN content.
        let divergent =
            crate::git::modified_paths_head_vs_branch(Path::new(main_project_root), &branch);

        // (a) Shared baseline: paths the main folder already tracks that also
        // exist in the worktree tree AND are byte-identical to main (not in
        // `divergent`), read from the MAIN tree (dedup merges — no new vectors).
        let n_baseline = enqueue_worktree_membership(
            pool,
            queue_manager,
            main_watch_folder_id,
            tenant_id,
            collection,
            &wt_root,
            &branch,
            &divergent,
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
            main_project_root,
            &branch,
            allowed_extensions,
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

        // (c) Divergent: shared files whose content DIFFERS on the worktree
        // branch (the `divergent` set), read from the worktree tree so the branch
        // is indexed with its own bytes instead of inheriting main's (baseline
        // inheritance / #151 auto-widen). Creates a new content-generation tagged
        // with the branch; the main generation (a different hash) is untouched.
        let n_div = enqueue_worktree_divergent(
            queue_manager,
            tenant_id,
            collection,
            &wt_root,
            main_project_root,
            &branch,
            &divergent,
            allowed_extensions,
        )
        .await;
        if n_div > 0 {
            info!(
                "worktree membership: enqueued {} divergent file(s) under branch '{}' (worktree {})",
                n_div, branch, wt_root
            );
            crate::monitoring::metrics_core::METRICS
                .worktree_membership_divergent_enqueued_total
                .with_label_values(&[tenant_id, branch.as_str()])
                .inc_by(n_div as u64);
        }

        total += n_baseline + n_new + n_div;
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
async fn enqueue_worktree_new_on_branch(
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
        if !worktree_path_eligible(main_project_root, &gate, allowed_extensions, collection, &rel_str)
        {
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
async fn enqueue_worktree_divergent(
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
        if !worktree_path_eligible(main_project_root, &gate, allowed_extensions, collection, rel_str)
        {
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

/// Whether `wt_root` is a genuine linked-worktree *checkout* rather than a
/// stale/malformed admin entry that resolved to a non-worktree directory.
///
/// A linked worktree checkout always carries a `.git` gitlink **file** whose
/// content is `gitdir: <main>/.git/worktrees/<name>`. Two non-worktree cases
/// this rejects:
/// - the `.claude/worktrees` *container* (an ancestor of every sub-worktree):
///   no `.git` entry at all — the 2026-08-06 phantom-walk root cause;
/// - a main repository root: `.git` is a **directory**, not a gitlink file.
///
/// Uses `is_file()` (not `exists()`) so a `.git` directory (a main repo) is
/// rejected too. This is the source-level guard against a bogus worktree root
/// being walked whole; see the call site in [`reconcile_worktree_branches`].
fn is_leaf_worktree_root(wt_root: &Path) -> bool {
    wt_root.join(".git").is_file()
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
    use super::{canonicalize_host_path, collapse_slashes, is_leaf_worktree_root};
    use std::path::Path;

    /// Regression (2026-08-06 phantom-walk): a real linked worktree has a `.git`
    /// gitlink FILE and is accepted; the `.claude/worktrees` container (no
    /// `.git`) and a main repo (`.git` DIRECTORY) are both rejected, so neither
    /// is ever walked whole.
    #[test]
    fn leaf_worktree_root_accepts_gitlink_rejects_container_and_main() {
        let temp = tempfile::TempDir::new().unwrap();

        // A linked worktree checkout: `.git` is a gitlink file.
        let wt = temp.path().join(".claude/worktrees/wt-feat");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: /main/.git/worktrees/wt-feat\n").unwrap();
        assert!(
            is_leaf_worktree_root(&wt),
            "a `.git` gitlink file marks a real leaf worktree"
        );

        // The container that holds the worktrees: no `.git` at all → rejected
        // (this is the exact root that produced the phantom walk).
        let container = temp.path().join(".claude/worktrees");
        assert!(
            !is_leaf_worktree_root(&container),
            "the worktrees container must never be treated as a worktree root"
        );

        // A main repository root: `.git` is a directory, not a gitlink → rejected.
        let main_repo = temp.path().join("main");
        std::fs::create_dir_all(main_repo.join(".git")).unwrap();
        assert!(
            !is_leaf_worktree_root(&main_repo),
            "a `.git` directory (main repo) is not a linked-worktree gitlink"
        );

        // A bare directory with nothing: rejected.
        let bare = temp.path().join("bare");
        std::fs::create_dir_all(&bare).unwrap();
        assert!(!is_leaf_worktree_root(Path::new(&bare)));
    }

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
