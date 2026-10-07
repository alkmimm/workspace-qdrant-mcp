//! The `WalkBuilder` every project walk starts from.
//!
//! A project walk only PRUNES. The authoritative "is this path ignored"
//! decision is the [`IgnoreGate`](super::ignore_gate::IgnoreGate): the project
//! `.gitignore` / `.wqmignore` cascade from the project root down, plus
//! `global.wqmignore`. The folder scan and the dequeue-time re-check ask that
//! gate and nothing else, so a walker that reads any OTHER ignore source drops
//! files they keep — and the startup reconciler, which diffs the walk against
//! the index, then calls those files stale at every start while the ingest
//! keeps them (#402).
//!
//! Two `WalkBuilder` defaults did exactly that and are switched off here:
//!
//! - **`.ignore` files.** Ripgrep's convention, not wqm's. This repository
//!   tracks one for local search tools listing `storage/`, `backup/` and
//!   `.env.*`; measured live 2026-10-07, the reconciler stripped `main` from
//!   the 22 files under `core/src/storage/`, `cli/src/commands/backup/` and
//!   `docker/.env.example` at every boot, and another path re-tagged them.
//! - **Parent directories.** Ignore files ABOVE the project root (a stray
//!   `.wqmignore` in the dev root, the main checkout around a linked worktree)
//!   are outside the gate's cascade, which starts at the project root.

use std::path::Path;

use ignore::WalkBuilder;

/// A walker over `project_root` honouring only what the ignore gate honours:
/// `.gitignore` files inside the project (as git does, and as a custom file
/// for trees that are not a git repository). Callers add `.wqmignore` and the
/// global file on top.
pub fn project_walk_builder(project_root: &Path) -> WalkBuilder {
    let mut builder = WalkBuilder::new(project_root);
    builder
        .hidden(false)
        .ignore(false)
        .parents(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(false)
        .add_custom_ignore_filename(".gitignore");
    builder
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::fs;

    use super::*;

    fn walked_files(root: &Path, builder: &WalkBuilder) -> HashSet<String> {
        builder
            .build()
            .flatten()
            .filter(|e| e.file_type().is_some_and(|ft| ft.is_file()))
            .filter_map(|e| {
                e.path()
                    .strip_prefix(root)
                    .ok()
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
            })
            .collect()
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    /// The live shape: a tracked `.ignore` for local search tools listing
    /// `storage/`, `backup/` and `.env.*` — real source the gate keeps.
    #[test]
    fn an_ignore_file_never_prunes_the_walk() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        write(r, ".ignore", "storage/\nbackup/\n.env.*\n");
        write(r, ".gitignore", "dist/\n");
        write(r, "core/src/storage/search.rs", "fn s() {}");
        write(r, "cli/src/commands/backup/mod.rs", "fn b() {}");
        write(r, "docker/.env.example", "A=1");
        write(r, "dist/bundle.js", "//");

        let files = walked_files(r, &project_walk_builder(r));
        for kept in [
            "core/src/storage/search.rs",
            "cli/src/commands/backup/mod.rs",
            "docker/.env.example",
        ] {
            assert!(files.contains(kept), "{kept} pruned by .ignore: {files:?}");
        }
        assert!(!files.contains("dist/bundle.js"), ".gitignore still prunes");
    }

    /// Ignore files above the project root are outside the gate's cascade.
    #[test]
    fn ignore_files_above_the_root_never_prune_the_walk() {
        let outer = tempfile::tempdir().unwrap();
        write(outer.path(), ".gitignore", "*.rs\n");
        write(outer.path(), ".wqmignore", "src/\n");
        let root = outer.path().join("project");
        write(&root, "src/main.rs", "fn main() {}");

        let mut builder = project_walk_builder(&root);
        builder.add_custom_ignore_filename(".wqmignore");
        let files = walked_files(&root, &builder);
        assert!(files.contains("src/main.rs"), "{files:?}");
    }
}
