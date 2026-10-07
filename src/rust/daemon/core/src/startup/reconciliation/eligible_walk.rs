//! The startup reconciler's "eligible" set: the files a project walk keeps
//! that the ingest would also take.
//!
//! Both halves must agree with what the dequeue-time gate decides, file by
//! file: the reconciler diffs this set against the index, so a file the walk
//! drops and the ingest keeps is called stale at every start, and one the walk
//! keeps and the ingest drops is called missing at every start (#402).

use std::collections::HashSet;
use std::path::Path;

use tracing::debug;

use crate::allowed_extensions::{AllowedExtensions, FileRoute};
use crate::patterns::ignore_gate::IgnoreGate;
use crate::patterns::project_walk::project_walk_builder;

/// Walk project tree and collect all eligible file paths (not excluded
/// by .gitignore or .wqmignore). Returns paths relative to `project_root`,
/// normalized to forward-slash separators so comparison against the
/// `tracked_files.relative_path` column works identically on Windows.
///
/// `global_ignore_path` — if `Some` and the file exists on disk, its patterns
/// are applied as a base-level ignore layer across the entire walk (equivalent
/// to a project-root `.wqmignore` but sourced from outside the project tree).
/// Pass `None` to walk a subtree that itself lives under a globally-ignored
/// path (a linked worktree under `.claude/worktrees/`): the global layer matches
/// absolute paths and their parents, so it would otherwise self-exclude the
/// whole worktree even when the walk is rooted inside it — see
/// `branch_switch::worktree_membership`.
pub(crate) fn walk_eligible_files(
    project_root: &Path,
    global_ignore_path: Option<&Path>,
) -> Result<HashSet<String>, String> {
    // The shared project walker: no `.ignore` files and no parent directories,
    // so this pruning walk never drops a file the gate below keeps (#402).
    let mut builder = project_walk_builder(project_root);
    builder.add_custom_ignore_filename(".wqmignore");

    // Explicitly apply the project-root `.wqmignore` as a base layer. The
    // `add_custom_ignore_filename(".wqmignore")` above is meant to pick it up
    // during the walk, but in a git repo (`git_ignore(true)`) it did NOT
    // reliably exclude root-anchored deep paths (e.g.
    // `src/typescript/mcp-server/reports/`), so reconciliation eligibility
    // diverged from the scan path's `ProjectIgnoreMatcher` (which DOES honor
    // the root `.wqmignore`). `add_ignore` anchors the file's patterns to its
    // parent dir (= `project_root`), matching the scan path exactly — without
    // this, reconciliation would keep re-adding files the scan path excludes
    // (add/delete reconcile loop). The bind-mounted `global.wqmignore` is
    // applied separately below.
    let project_wqmignore = project_root.join(".wqmignore");
    if project_wqmignore.is_file() {
        builder.add_ignore(&project_wqmignore);
    }

    // Apply global ignore rules (daemon-wide, outside the project tree).
    // `add_ignore` applies the file's patterns as a base layer that every
    // project walk inherits; `add_custom_ignore_filename` only finds files
    // inside the walked tree, so it cannot reference the global file here.
    if let Some(global_path) = global_ignore_path {
        if global_path.is_file() {
            builder.add_ignore(global_path);
            debug!(
                "[ignore_sync] applying global ignore rules from {}",
                global_path.display()
            );
        }
    }

    // Authoritative post-filter via the shared IgnoreGate (project cascade +
    // global.wqmignore, root-anchored). The WalkBuilder above keeps git_ignore on
    // purely to prune huge dirs cheaply, but `add_ignore` only matches depth-1
    // reliably — nested matches leak (`state/qdrant/...`, `<proj>/generated/...`
    // survived reconciliation and were never marked stale). Re-checking every
    // candidate through the SAME gate the folder-scan uses guarantees the two
    // walk paths can never disagree. The gate only DROPS files (never adds), so
    // it cannot resurrect a walk-pruned path.
    let gate = IgnoreGate::for_dir(project_root, Some(project_root), global_ignore_path);

    let mut files = HashSet::new();
    for entry in builder.build().flatten() {
        if entry.file_type().is_some_and(|ft| ft.is_file()) {
            if gate.is_ignored(entry.path(), false) {
                continue;
            }
            if let Some(rel) = entry
                .path()
                .strip_prefix(project_root)
                .ok()
                .map(normalize_relative)
            {
                files.insert(rel);
            }
        }
    }

    Ok(files)
}

/// Drop from `eligible` every path the ingestion gate would refuse, so the
/// reconciler's "missing" means "indexable and absent" rather than merely
/// "on disk and not ignored".
///
/// The predicate is `route_file(..) != Excluded`, NOT
/// [`AllowedExtensions::is_allowed`]: a library-routed document inside a
/// project folder (`.pdf`, `.docx`, `.odt` — see `LIBRARY_ROUTED_EXTENSIONS`)
/// is legitimately indexed under `libraries` while `is_allowed(path,
/// "projects")` returns false for it. Measured before shipping: exactly 9
/// such files are indexed today, and `is_allowed` would have flipped every
/// one of them from fine to STALE — this pass deletes what it considers
/// stale, so the wrong predicate here is a data-loss bug, not a cosmetic one.
/// `route_file` is also what the file watcher filters on
/// (`should_filter_debounced_event`), so the three enqueue paths now agree.
///
/// Shrinking the eligible set is otherwise safe in the stale direction: a
/// path that is indexed AND refused by the gate is debris the ingest can
/// never refresh, and the delete this pass enqueues for it is the cleanup.
pub(super) fn retain_indexable(
    eligible: &mut HashSet<String>,
    project_root: &Path,
    collection: &str,
    tenant_id: &str,
) {
    static ALLOWED: std::sync::LazyLock<AllowedExtensions> =
        std::sync::LazyLock::new(AllowedExtensions::default);
    eligible.retain(|rel| {
        let abs = project_root.join(rel);
        !matches!(
            ALLOWED.route_file(&abs.to_string_lossy(), collection, tenant_id),
            FileRoute::Excluded
        )
    });
}

/// Normalize a relative path to the storage format used by
/// `tracked_files.relative_path` — forward-slash separators, lossy UTF-8.
fn normalize_relative(rel: &Path) -> String {
    let s = rel.to_string_lossy().to_string();
    if std::path::MAIN_SEPARATOR == '/' {
        s
    } else {
        s.replace(std::path::MAIN_SEPARATOR, "/")
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn set(paths: &[&str]) -> HashSet<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    /// The eligible set is what the ingest can actually take. Paths the
    /// allowlist refuses must be dropped, or they are reported "missing"
    /// forever: the reconciler enqueues an add, the dequeue guard drops it,
    /// and the next start repeats it (issue #402 — the same "183 missing
    /// added" at six consecutive boots, 2026-09-19 → 22).
    #[test]
    fn retain_indexable_drops_what_the_ingest_would_refuse() {
        let root = Path::new("/repo");
        let mut files = set(&[
            // indexable
            "src/main.rs",
            "README.md",
            "Dockerfile",
            ".gitignore",
            "src/main/resources/application.properties",
            ".env.example",
            // refused by the allowlist — every one of these was in the live
            // 183 that the reconciler re-enqueued at each boot
            "doc-frontend/packages/app/.env",
            "doc-frontend/packages/app/.env.local",
            "ios/Runner.xcodeproj/xcshareddata/xcschemes/Runner.xcscheme",
            "macos/Runner.xcworkspace/contents.xcworkspacedata",
            "infra/terraform/bootstrap-github/terraform.tfvars.example",
            "scripts/git-hooks/pre-push",
            "windows/runner/runner.exe.manifest",
        ]);
        retain_indexable(&mut files, root, "projects", "tenant123");
        assert_eq!(
            files,
            set(&[
                "src/main.rs",
                "README.md",
                "Dockerfile",
                ".gitignore",
                "src/main/resources/application.properties",
                ".env.example",
            ])
        );
    }

    /// A library-routed document inside a PROJECT folder (`.pdf`, `.docx`,
    /// `.odt`) is indexed under `libraries`, so it must stay eligible. This is
    /// why the predicate is `route_file` and not `is_allowed`: the latter
    /// returns false for these, which would have flipped the 9 such files
    /// measured in the live index from fine to STALE — and this pass deletes
    /// what it calls stale.
    #[test]
    fn retain_indexable_keeps_library_routed_documents_in_a_project_folder() {
        let root = Path::new("/repo");
        let mut files = set(&[
            "docs/workspace-qdrant-tagging.pdf",
            "integrator-api/Guia_Integracao.docx",
            "integrator-api/Guia_Integracao.odt",
            "e2e/fixtures/sample.pdf",
            "src/lib.rs",
            "app/.env",
        ]);
        retain_indexable(&mut files, root, "projects", "tenant123");
        assert!(!files.contains("app/.env"), "still refused");
        assert_eq!(files.len(), 5, "the four documents and the source survive");
        for kept in [
            "docs/workspace-qdrant-tagging.pdf",
            "integrator-api/Guia_Integracao.docx",
            "integrator-api/Guia_Integracao.odt",
            "e2e/fixtures/sample.pdf",
        ] {
            assert!(files.contains(kept), "{kept} routes to libraries, not out");
        }
    }

    /// A libraries watch folder keeps its own formats and still refuses what
    /// the library allowlist does not name.
    #[test]
    fn retain_indexable_honours_the_library_collection() {
        let root = Path::new("/lib");
        let mut files = set(&["manual.pdf", "notes.md", "cover.png", "secrets/.env"]);
        retain_indexable(&mut files, root, "libraries", "mylib");
        assert!(files.contains("manual.pdf") && files.contains("notes.md"));
        assert!(!files.contains("secrets/.env"));
        assert!(!files.contains("cover.png"), "images are not text");
    }

    #[test]
    fn walk_eligible_files_respects_gitignore() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(".gitignore"), "dist/\n").unwrap();
        let dist = root.path().join("dist");
        fs::create_dir(&dist).unwrap();
        fs::write(dist.join("bundle.js"), "//").unwrap();
        let src = root.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("main.rs"), "fn main() {}").unwrap();

        let files = walk_eligible_files(root.path(), None).unwrap();
        // src/main.rs should be eligible
        assert!(files.iter().any(|f| f.ends_with("main.rs")));
        // dist/bundle.js should NOT be eligible
        assert!(!files.iter().any(|f| f.ends_with("bundle.js")));
    }

    #[test]
    fn walk_eligible_files_respects_wqmignore_exclusion() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(".wqmignore"), "data/\n").unwrap();
        let data = root.path().join("data");
        fs::create_dir(&data).unwrap();
        fs::write(data.join("big.csv"), "a,b,c").unwrap();
        fs::write(root.path().join("readme.md"), "# hi").unwrap();

        let files = walk_eligible_files(root.path(), None).unwrap();
        assert!(files.iter().any(|f| f.ends_with("readme.md")));
        assert!(!files.iter().any(|f| f.ends_with("big.csv")));
    }

    #[test]
    fn walk_eligible_files_respects_global_ignore() {
        let global_dir = tempfile::tempdir().unwrap();
        let global_ignore = global_dir.path().join("global.wqmignore");
        fs::write(&global_ignore, "vendors/\n*.zip\n").unwrap();

        let root = tempfile::tempdir().unwrap();
        let vendors = root.path().join("vendors");
        fs::create_dir(&vendors).unwrap();
        fs::write(vendors.join("library.js"), "// lib").unwrap();
        fs::write(root.path().join("archive.zip"), "PK..").unwrap();
        fs::write(root.path().join("main.rs"), "fn main() {}").unwrap();

        let files = walk_eligible_files(root.path(), Some(&global_ignore)).unwrap();
        // main.rs is eligible
        assert!(files.contains("main.rs"), "expected main.rs, got {files:?}");
        // vendors/ and *.zip are globally excluded
        assert!(
            !files.iter().any(|f| f.contains("library.js")),
            "vendors/ should be excluded"
        );
        assert!(!files.contains("archive.zip"), "*.zip should be excluded");
    }

    #[test]
    fn walk_eligible_files_excludes_generated_with_realistic_global() {
        // Reproduction of the live finding: with the FULL real global.wqmignore
        // pattern set (re-inclusions + many rules), deep generated/ files under a
        // example-monorepo-shaped tree must still be excluded by the post-filter.
        let global_dir = tempfile::tempdir().unwrap();
        let global_ignore = global_dir.path().join("global.wqmignore");
        fs::write(
            &global_ignore,
            "**/example-platform/legacy_app/**\n\
             !**/example-platform/legacy_app/cfg/\n\
             !**/example-platform/legacy_app/cfg/**\n\
             **/vendor_monitoring/vendor_monitoring/**\n\
             state/\n\
             **/state/qdrant/\n\
             node_modules/\n\
             **/proto/src/generated/\n\
             **/*_proto/\n\
             **/generated/proto/\n\
             **/*OuterClass.java\n\
             **/proto/**/*.java\n\
             **/*.pb.dart\n\
             **/generated/\n\
             **/lib/src/generated/\n\
             **/packages/generated/\n",
        )
        .unwrap();

        let root = tempfile::tempdir().unwrap();
        let be = root.path().join("doc-backend/proto/src/generated/doc");
        fs::create_dir_all(&be).unwrap();
        fs::write(be.join("ScheduleOuterClass.java"), "// gen").unwrap();
        let fe = root
            .path()
            .join("doc-frontend/packages/generated/lib/protos");
        fs::create_dir_all(&fe).unwrap();
        fs::write(fe.join("shifts.pb.dart"), "// gen").unwrap();
        let src = root.path().join("doc-backend/src/main/java/com/x");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("Service.java"), "class Service {}").unwrap();

        let files = walk_eligible_files(root.path(), Some(&global_ignore)).unwrap();
        assert!(
            !files.iter().any(|f| f.contains("ScheduleOuterClass")),
            "generated OuterClass.java must be excluded, got {files:?}"
        );
        assert!(
            !files.iter().any(|f| f.contains("shifts.pb.dart")),
            "generated .pb.dart must be excluded, got {files:?}"
        );
        assert!(
            files.iter().any(|f| f.contains("Service.java")),
            "hand-authored Service.java must be kept, got {files:?}"
        );
    }

    #[test]
    fn walk_eligible_files_excludes_deep_global_match() {
        // Regression: `WalkBuilder::add_ignore` anchors global patterns to the
        // ignore file's parent dir, so a `**/`-pattern leaks for DEEP (depth-2+)
        // project paths — `state/qdrant/...` survived reconciliation and was
        // never marked stale. The IgnoreGate post-filter must drop it
        // regardless of depth.
        let global_dir = tempfile::tempdir().unwrap();
        let global_ignore = global_dir.path().join("global.wqmignore");
        fs::write(&global_ignore, "**/state/qdrant/\n**/generated/\n").unwrap();

        let root = tempfile::tempdir().unwrap();
        let deep = root.path().join("sub").join("state").join("qdrant");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("segment.json"), "{}").unwrap();
        let gen = root.path().join("pkg").join("generated");
        fs::create_dir_all(&gen).unwrap();
        fs::write(gen.join("api.pb.dart"), "// gen").unwrap();
        fs::write(root.path().join("keep.rs"), "fn main() {}").unwrap();

        let files = walk_eligible_files(root.path(), Some(&global_ignore)).unwrap();
        assert!(
            files.contains("keep.rs"),
            "hand-authored file kept, got {files:?}"
        );
        assert!(
            !files.iter().any(|f| f.contains("state/qdrant")),
            "deep state/qdrant must be excluded, got {files:?}"
        );
        assert!(
            !files.iter().any(|f| f.contains("generated")),
            "deep generated/ must be excluded, got {files:?}"
        );
    }

    #[test]
    fn walk_eligible_files_emits_relative_paths_with_forward_slashes() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("src").join("api");
        std::fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("server.rs"), "fn main() {}").unwrap();

        let files = walk_eligible_files(root.path(), None).unwrap();

        // Output must be a relative path joined by '/', matching the format
        // used in tracked_files.relative_path. On Windows this validates the
        // separator normalization. The absolute path must NOT leak through.
        assert!(
            files.contains("src/api/server.rs"),
            "expected 'src/api/server.rs', got {:?}",
            files
        );
        assert!(
            !files.iter().any(|f| f.contains(':') || f.starts_with('/')),
            "no entry should look absolute, got {:?}",
            files
        );
    }

    /// The pruning walk and the dequeue-time gate must agree on EVERY file:
    /// the reconciler diffs the walk against what the ingest keeps, so a file
    /// one keeps and the other drops is re-enqueued at every start (#402).
    /// The fixture holds each shape that has split them: a tracked `.ignore`
    /// for local search tools (22 live files lost `main` at every boot), a
    /// Laravel placeholder `.gitignore` (12 re-enqueued adds), an ignore file
    /// above the root, a nested negation, and a `.wqmignore` re-inclusion.
    #[test]
    fn the_walk_and_the_dequeue_gate_agree_on_every_file() {
        let outer = tempfile::tempdir().unwrap();
        fs::write(outer.path().join(".wqmignore"), "*.rs\n").unwrap();
        let root = outer.path().join("project");
        let tree = [
            (".ignore", "storage/\nbackup/\n"),
            (".gitignore", "/vendor\ndist/\n*.log\n"),
            (".wqmignore", "!keep.log\n"),
            ("core/src/storage/search.rs", "fn s() {}"),
            ("cli/src/commands/backup/mod.rs", "fn b() {}"),
            ("storage/logs/.gitignore", "*\n!.gitignore\n"),
            ("storage/logs/laravel.log", "x"),
            ("storage/app/.gitignore", "*\n!public/\n!.gitignore\n"),
            ("storage/app/public/.gitignore", "*\n!.gitignore\n"),
            ("storage/app/public/avatar.txt", "x"),
            (
                "proto/.gitignore",
                "*.proto\n!common/\n!common/**/*.proto\n",
            ),
            ("proto/common/policy.proto", "syntax = \"proto3\";"),
            ("proto/scratch.proto", "syntax = \"proto3\";"),
            ("vendor/pkg/lib.php", "<?php"),
            ("dist/app.js", "//"),
            ("app/keep.log", "x"),
            ("app/drop.log", "x"),
            ("src/main.rs", "fn main() {}"),
        ];
        for (rel, body) in tree {
            let path = root.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        }

        let walked = walk_eligible_files(&root, None).unwrap();
        for (rel, _) in tree {
            let abs = root.join(rel);
            let dequeue_keeps = !IgnoreGate::for_dir(abs.parent().unwrap(), Some(&root), None)
                .is_ignored_with_ancestors(&root, &abs);
            assert_eq!(
                walked.contains(rel),
                dequeue_keeps,
                "{rel}: the walk and the dequeue gate disagree (walked: {walked:?})"
            );
        }
        for kept in [
            "core/src/storage/search.rs",
            "cli/src/commands/backup/mod.rs",
            "storage/logs/.gitignore",
            "storage/app/public/.gitignore",
            "proto/common/policy.proto",
            "app/keep.log",
            "src/main.rs",
        ] {
            assert!(walked.contains(kept), "{kept} must be eligible");
        }
    }
}
