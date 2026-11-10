//! The MAIN folder's eligibility, applied to files read from a linked
//! worktree or a staged branch tip: every path a worktree branch or a
//! checkout-less branch indexes must pass the same ignore gate and extension
//! allowlist the main folder's scan applies.

use std::path::Path;

use crate::allowed_extensions::AllowedExtensions;

/// The MAIN folder's eligibility gate (project `.gitignore`/`.wqmignore` cascade
/// + `global.wqmignore`), anchored at the main root.
///
/// Worktree-read items (new-on-branch, divergent) test each candidate as
/// `main_root/rel` so every global rule applies to the rel path exactly as the
/// main scan would — dropping generated / globally-excluded files — WITHOUT the
/// `.claude/worktrees/` self-exclusion a worktree-anchored test would trigger
/// (that rule matches absolute paths and their parents, so anchoring inside the
/// worktree does not save it). Build once per worktree; reuse across candidates.
pub(super) fn main_eligibility_gate(
    main_project_root: &str,
) -> crate::patterns::ignore_gate::IgnoreGate {
    let main_root = Path::new(main_project_root);
    let global = crate::patterns::global_ignore::resolve_global_ignore_path();
    crate::patterns::ignore_gate::IgnoreGate::for_dir(main_root, Some(main_root), global.as_deref())
}

/// Whether `rel` passes the MAIN folder's full eligibility — the ignore `gate`
/// (from [`main_eligibility_gate`]) plus [`AllowedExtensions::is_indexable`] —
/// the exact filter the folder scan applies. Worktree-read items run every
/// candidate through this so a worktree branch never indexes files the main scan
/// omits, and disallowed types are dropped at discovery instead of enqueued only
/// to be skipped at the ingest guard and re-churned every scan.
///
/// `is_indexable`, not `is_allowed`: a library-format document (`.docx`, `.pdf`)
/// is indexed into the project's library, and the queue routes the item there.
/// With `is_allowed` such a file was never eligible here, so worktree branches
/// and branches with no checkout never followed it (bws-engineer `master`, a
/// `.docx` left on a version its tip no longer had, 2026-10-05).
pub(super) fn worktree_path_eligible(
    main_project_root: &str,
    gate: &crate::patterns::ignore_gate::IgnoreGate,
    allowed_extensions: &AllowedExtensions,
    collection: &str,
    rel: &str,
) -> bool {
    let main_root = Path::new(main_project_root);
    !gate.is_ignored_with_ancestors(main_root, &main_root.join(rel))
        && allowed_extensions.is_indexable(rel, collection)
}
