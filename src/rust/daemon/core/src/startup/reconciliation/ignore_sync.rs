//! Ignore-file reconciliation engine.
//!
//! Given a project root, walks the tree applying current .gitignore +
//! .wqmignore rules, diffs against the set of already-indexed files in
//! the DB, and enqueues file/delete for stale entries and file/add for
//! missing ones. This keeps the index consistent when ignore rules change
//! while the daemon was offline (startup) or at runtime (watcher trigger).

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use sqlx::SqlitePool;
use tracing::{debug, info, warn};

use super::eligible_walk::{retain_indexable, walk_eligible_files};
use crate::queue_operations::QueueManager;
use crate::watching_queue::WatchManager;

/// Outcome of a single reconciliation run.
#[derive(Debug, Default)]
pub struct ReconcileStats {
    pub stale_deleted: u64,
    pub missing_added: u64,
}

/// Reconcile ignore rules for a single project.
///
/// 1. Walk the project tree with WalkBuilder (respects .gitignore + .wqmignore)
/// 2. Query tracked_files for all indexed file paths in that project
/// 3. Diff: stale = indexed but now excluded, missing = on disk but not indexed
/// 4. Enqueue file/delete for stale, file/add for missing
///
/// `global_ignore_path` — optional path to `global.wqmignore`, applied on top
/// of per-project ignore files. When `None` or when the file does not exist,
/// only the per-project `.gitignore` / `.wqmignore` files are used.
///
/// `watch_id` — the watch folder whose rows are diffed, supplied by the caller
/// who already knows which folder `project_root` belongs to (#280). It is NOT
/// re-derived here: a tenant owns one enabled `projects` row per clone AND per
/// registered git worktree, so a tenant-scoped `LIMIT 1` lookup can resolve to
/// a DIFFERENT folder than the tree being walked — diffing folder A's rows
/// against folder B's walk. Observed live 2026-07-16: the pre-#278 code did
/// exactly that 15 times in one evening, flagging a tenant's entire 1,868-path
/// index stale on every pass.
pub async fn reconcile_ignore_rules(
    project_root: &Path,
    watch_id: &str,
    tenant_id: &str,
    collection: &str,
    pool: &SqlitePool,
    queue_manager: &Arc<QueueManager>,
    global_ignore_path: Option<&Path>,
) -> Result<ReconcileStats, String> {
    // Resolve the branch from the SAME tree we are about to walk. The stale
    // delete this pass enqueues drops exactly this branch's tag, so the branch a
    // delete targets and the branch whose staleness we measure must be one and
    // the same value, read from one source. (Libraries and other non-`projects`
    // collections have no branch; `None` keeps their per-path semantics.)
    let branch = resolve_branch(project_root, collection);

    let mut eligible_files = walk_eligible_files(project_root, global_ignore_path)?;
    // "On disk and not ignored" is not the same as "indexable". Until
    // 2026-09-22 this pass stopped at the ignore cascade, so every file the
    // ingestion allowlist refuses — `.env`, `Runner.xcscheme`,
    // `contents.xcworkspacedata`, `terraform.tfvars.example`, extensionless
    // hook scripts — counted as MISSING at every single start: the reconciler
    // enqueued an add, the dequeue guard dropped it, nothing changed, and the
    // next start enqueued the same set again. Measured across six boots
    // (2026-09-19 → 22): "21 stale deleted, 183 missing added", byte for byte
    // identical every time, which is issue #402 and which also made the
    // `missing=` number useless as a drift signal.
    retain_indexable(&mut eligible_files, project_root, collection, tenant_id);
    let indexed_files = get_indexed_file_paths(pool, watch_id).await?;

    // Empty-walk safety net. A walk that yields ZERO eligible files while the
    // index holds many is a walk failure (unreadable/half-mounted tree, a
    // registered git worktree whose path resolves wrong, an ignore file that
    // transiently excludes everything), NOT a signal that the whole project was
    // deleted. Treating it as truth turns every indexed file into "stale" and
    // enqueues a mass delete. This is the same class of guard `branch_prune`
    // already carries ("repo unreadable / zero branches → skip the project").
    // Observed live 2026-07-16: a worktree walk returned eligible=0 and 10 tags
    // were dropped from real files before the per-branch scoping bounded it; a
    // per-path reconciler here would have proposed the entire index (#280).
    // Skipping is strictly safe: it only ever withholds deletes, and a genuinely
    // empty project has nothing to reconcile away.
    if eligible_files.is_empty() && !indexed_files.is_empty() {
        warn!(
            "[ignore_sync] {} — walk found 0 eligible files but {} are indexed; \
             treating as a walk failure and skipping (no deletes enqueued)",
            tenant_id,
            indexed_files.len()
        );
        return Ok(ReconcileStats::default());
    }

    // Staleness must be measured per (path, branch) because the remedy is
    // per-branch: a stale `file|delete` drops ONE tag from the Layer-2 content row
    // (`remove_branch_from_tracked_file`) and deliberately leaves the row alive
    // whenever another branch still holds that content. Diffing a per-PATH
    // "indexed" set against a per-BRANCH remedy therefore cannot converge — the
    // path survives in `tracked_files`, is re-diagnosed stale on the next pass, and
    // is re-enqueued forever. Measured live on example-monorepo 2026-07-16: 49 reconciles in
    // 50 minutes all reporting an identical `5 stale`, each one stripping `main`
    // off a live on-disk file and then filter-deleting its points across every
    // branch (#224). Scoping the diff to the branch we are about to delete from
    // makes the pass converge after one cycle: once the tag is gone the path is no
    // longer indexed *on this branch*, so it is no longer stale.
    let indexed_on_branch = match branch.as_deref() {
        Some(b) => get_indexed_file_paths_on_branch(pool, watch_id, b).await?,
        None => indexed_files.clone(),
    };

    let stale: Vec<&String> = indexed_on_branch
        .iter()
        .filter(|p| !eligible_files.contains(p.as_str()))
        .collect();
    // `missing` stays per-PATH, deliberately asymmetric with `stale`. There is no
    // per-branch remedy for it to mirror (the repair is an Uplift that re-ingests
    // the content and merges the branch into whichever row already holds it), and
    // a per-branch `missing` would report EVERY file in the repo for the window
    // between a branch checkout and branch-membership re-keying (#167) tagging the
    // rows — turning every branch switch into a full-repo Uplift storm.
    let missing: Vec<&String> = eligible_files
        .iter()
        .filter(|p| !indexed_files.contains(p.as_str()))
        .collect();

    if stale.is_empty() && missing.is_empty() {
        debug!(
            "[ignore_sync] {} — no changes (indexed={}, eligible={})",
            tenant_id,
            indexed_files.len(),
            eligible_files.len()
        );
        return Ok(ReconcileStats::default());
    }

    info!(
        "[ignore_sync] {} — {} stale, {} missing (indexed={}, on_branch={} [{}], eligible={})",
        tenant_id,
        stale.len(),
        missing.len(),
        indexed_files.len(),
        indexed_on_branch.len(),
        branch.as_deref().unwrap_or("-"),
        eligible_files.len()
    );
    // The paths, not just the counts: a set that comes back identical at every
    // start is a walk/ingest disagreement, and naming its members is what
    // located both halves of #402 (a tracked `.ignore`; Laravel placeholder
    // `.gitignore`s).
    debug!(
        "[ignore_sync] {} — stale: {:?}; missing: {:?}",
        tenant_id,
        stale.iter().take(50).collect::<Vec<_>>(),
        missing.iter().take(50).collect::<Vec<_>>()
    );

    super::ignore_enqueue::enqueue_reconcile_ops(
        queue_manager,
        tenant_id,
        collection,
        &stale,
        &missing,
        branch.as_deref(),
    )
    .await
}

/// Resolve the git branch of the tree being reconciled.
///
/// Reads HEAD from `project_root` — the very tree `walk_eligible_files` walks —
/// rather than re-deriving a watch-folder path from the DB, so the walk, the
/// staleness diff, and the enqueued delete can never disagree about which branch
/// they are talking about.
fn resolve_branch(project_root: &Path, collection: &str) -> Option<String> {
    if collection != "projects" {
        return None;
    }
    Some(crate::watching_queue::get_current_branch(project_root))
}

/// Resolve a watch folder by the PATH being reconciled → `(watch_id, is_worktree)`.
///
/// The path is the folder's identity (#280): tenant-scoped lookups are ambiguous
/// once a tenant owns several enabled rows (clones + registered worktrees).
/// Exact string match first; on miss, compare each enabled row's path through
/// `resolve_local_watch_path` (Docker Desktop aliases can make the walked root
/// differ from the stored spelling).
pub(crate) async fn fetch_watch_folder_by_path(
    pool: &SqlitePool,
    project_root: &Path,
) -> Result<Option<(String, bool)>, String> {
    let root = project_root.to_string_lossy().to_string();
    let exact: Option<(String, i64)> = sqlx::query_as(
        "SELECT watch_id, COALESCE(is_worktree, 0) FROM watch_folders \
         WHERE path = ?1 AND enabled = 1 LIMIT 1",
    )
    .bind(&root)
    .fetch_optional(pool)
    .await
    .map_err(|e| format!("watch folder lookup by path failed: {e}"))?;
    if let Some((watch_id, is_wt)) = exact {
        return Ok(Some((watch_id, is_wt != 0)));
    }

    let rows: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT watch_id, path, COALESCE(is_worktree, 0) FROM watch_folders WHERE enabled = 1",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| format!("watch folder enumeration failed: {e}"))?;
    for (watch_id, path, is_wt) in rows {
        if WatchManager::resolve_local_watch_path(&path) == project_root {
            return Ok(Some((watch_id, is_wt != 0)));
        }
    }
    Ok(None)
}

/// Get all tracked relative paths for a watch folder from the DB.
///
/// Reads the canonical `relative_path` column. Pre-v37 the schema had a
/// denormalized absolute `file_path`; that column was dropped by the v37
/// `tracked_files` rebuild (see `tracked_files_schema::schema`).
async fn get_indexed_file_paths(
    pool: &SqlitePool,
    watch_folder_id: &str,
) -> Result<HashSet<String>, String> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT relative_path FROM tracked_files WHERE watch_folder_id = ?1")
            .bind(watch_folder_id)
            .fetch_all(pool)
            .await
            .map_err(|e| format!("query tracked_files failed: {e}"))?;

    Ok(rows.into_iter().map(|(p,)| p).collect())
}

/// Get the tracked relative paths a watch folder holds **on `branch`**.
///
/// Layer 2 (#124) keeps one row per `(watch_folder, relative_path, file_hash)`
/// with a JSON `branches` array, so a path routinely has several generations and
/// only some of them carry any given branch. This is the set a per-branch stale
/// delete can actually act on; see the convergence note in
/// `reconcile_ignore_rules`.
async fn get_indexed_file_paths_on_branch(
    pool: &SqlitePool,
    watch_folder_id: &str,
    branch: &str,
) -> Result<HashSet<String>, String> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT relative_path FROM tracked_files \
         WHERE watch_folder_id = ?1 \
           AND EXISTS (SELECT 1 FROM json_each(branches) WHERE value = ?2)",
    )
    .bind(watch_folder_id)
    .bind(branch)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("query tracked_files on branch failed: {e}"))?;

    Ok(rows.into_iter().map(|(p,)| p).collect())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// Unit test the diff logic without DB (just the set operations)
    #[test]
    fn diff_stale_and_missing() {
        let indexed: HashSet<String> = ["/a/foo.rs", "/a/bar.rs", "/a/old.rs"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let eligible: HashSet<String> = ["/a/foo.rs", "/a/bar.rs", "/a/new.rs"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let stale: Vec<&String> = indexed
            .iter()
            .filter(|p| !eligible.contains(p.as_str()))
            .collect();
        let missing: Vec<&String> = eligible
            .iter()
            .filter(|p| !indexed.contains(p.as_str()))
            .collect();

        assert_eq!(stale.len(), 1);
        assert!(stale.iter().any(|p| p.as_str() == "/a/old.rs"));
        assert_eq!(missing.len(), 1);
        assert!(missing.iter().any(|p| p.as_str() == "/a/new.rs"));
    }

    #[test]
    fn diff_no_changes() {
        let files: HashSet<String> = ["/a/foo.rs"].iter().map(|s| s.to_string()).collect();
        let stale: Vec<&String> = files
            .iter()
            .filter(|p| !files.contains(p.as_str()))
            .collect();
        let missing: Vec<&String> = files
            .iter()
            .filter(|p| !files.contains(p.as_str()))
            .collect();
        assert!(stale.is_empty());
        assert!(missing.is_empty());
    }

    #[tokio::test]
    async fn get_indexed_file_paths_reads_relative_path_column() {
        // Bootstrap the full SchemaManager pipeline so tracked_files has the
        // post-v37 shape (no `file_path` column, `relative_path` canonical).
        let pool = super::super::tests::create_test_pool().await;
        super::super::tests::setup_schema(&pool).await;

        sqlx::query(
            "INSERT INTO watch_folders (watch_id, path, collection, tenant_id, enabled, is_archived, created_at, updated_at) \
             VALUES ('wf1', '/some/root', 'projects', 'tenant1', 1, 0, '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        for rel in ["src/main.rs", "docs/readme.md"] {
            sqlx::query(
                "INSERT INTO tracked_files \
                 (watch_folder_id, relative_path, branches, file_mtime, file_hash, collection, base_point, created_at, updated_at) \
                 VALUES ('wf1', ?1, '[\"main\"]', '2025-01-01T00:00:00Z', 'h', 'projects', 'bp', '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z')"
            )
            .bind(rel)
            .execute(&pool)
            .await
            .unwrap();
        }

        let got = get_indexed_file_paths(&pool, "wf1").await.unwrap();
        assert_eq!(got.len(), 2);
        assert!(got.contains("src/main.rs"));
        assert!(got.contains("docs/readme.md"));
    }

    /// Seed one Layer-2 generation: `(wf1, rel, hash)` tagged `branches`.
    async fn seed_generation(pool: &SqlitePool, rel: &str, hash: &str, branches: &str) {
        sqlx::query(
            "INSERT INTO tracked_files \
             (watch_folder_id, relative_path, branches, file_mtime, file_hash, collection, base_point, created_at, updated_at) \
             VALUES ('wf1', ?1, ?2, '2025-01-01T00:00:00Z', ?3, 'projects', 'bp', '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z')",
        )
        .bind(rel)
        .bind(branches)
        .bind(hash)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_watch_folder(pool: &SqlitePool) {
        sqlx::query(
            "INSERT INTO watch_folders (watch_id, path, collection, tenant_id, enabled, is_archived, created_at, updated_at) \
             VALUES ('wf1', '/some/root', 'projects', 'tenant1', 1, 0, '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z')",
        )
        .execute(pool)
        .await
        .unwrap();
    }

    /// The per-branch indexed set must only report paths that actually carry the
    /// branch — that is the set a per-branch stale delete can act on (#224).
    #[tokio::test]
    async fn indexed_on_branch_filters_by_branch_tag() {
        let pool = super::super::tests::create_test_pool().await;
        super::super::tests::setup_schema(&pool).await;
        seed_watch_folder(&pool).await;

        seed_generation(&pool, "proto/live.proto", "h1", r#"["main","feat/x"]"#).await;
        seed_generation(&pool, "proto/other.proto", "h2", r#"["feat/x"]"#).await;

        let on_main = get_indexed_file_paths_on_branch(&pool, "wf1", "main")
            .await
            .unwrap();
        assert_eq!(on_main.len(), 1, "only the main-tagged path: {on_main:?}");
        assert!(on_main.contains("proto/live.proto"));

        let all = get_indexed_file_paths(&pool, "wf1").await.unwrap();
        assert_eq!(all.len(), 2, "the per-path set still sees both");
    }

    /// The #224 non-convergence, as a regression test.
    ///
    /// A path whose content is shared across branches keeps its row when the stale
    /// delete drops one tag (`remove_branch_from_tracked_file` only deletes the row
    /// once the set empties). Under the old per-PATH diff the path stayed
    /// "indexed", so it was re-diagnosed stale on every pass and re-enqueued
    /// forever — 49 identical reconciles in 50 minutes on the live stack. Scoped to
    /// the branch being deleted from, the second pass sees nothing.
    #[tokio::test]
    async fn stale_diff_converges_after_the_tag_is_dropped() {
        let pool = super::super::tests::create_test_pool().await;
        super::super::tests::setup_schema(&pool).await;
        seed_watch_folder(&pool).await;

        // One generation, held by the checked-out branch AND another branch.
        seed_generation(&pool, "proto/excluded.proto", "h1", r#"["main","feat/x"]"#).await;
        let eligible: HashSet<String> = HashSet::new(); // ignore rules now exclude it

        // Pass 1: stale on `main`.
        let on_main = get_indexed_file_paths_on_branch(&pool, "wf1", "main")
            .await
            .unwrap();
        let stale: Vec<&String> = on_main
            .iter()
            .filter(|p| !eligible.contains(p.as_str()))
            .collect();
        assert_eq!(stale.len(), 1, "pass 1 diagnoses it stale on main");

        // The delete drops ONLY `main`; the row survives for `feat/x`.
        sqlx::query("UPDATE tracked_files SET branches = ?1 WHERE relative_path = ?2")
            .bind(r#"["feat/x"]"#)
            .bind("proto/excluded.proto")
            .execute(&pool)
            .await
            .unwrap();

        // Pass 2: converged — nothing left to do on `main`.
        let on_main = get_indexed_file_paths_on_branch(&pool, "wf1", "main")
            .await
            .unwrap();
        let stale: Vec<&String> = on_main
            .iter()
            .filter(|p| !eligible.contains(p.as_str()))
            .collect();
        assert!(stale.is_empty(), "pass 2 must not re-enqueue: {stale:?}");

        // ...while the per-PATH set — what the old code diffed — still reports it,
        // which is exactly why it looped.
        let all = get_indexed_file_paths(&pool, "wf1").await.unwrap();
        assert!(all.contains("proto/excluded.proto"));
    }

    /// A walk that returns zero eligible files against a non-empty index is a
    /// walk failure, not a mass delete signal — reconcile must skip (#280).
    #[tokio::test]
    async fn empty_walk_against_nonempty_index_enqueues_nothing() {
        use crate::queue_operations::QueueManager;
        use std::sync::Arc;
        use tempfile::TempDir;

        let pool = super::super::tests::create_test_pool().await;
        super::super::tests::setup_schema(&pool).await;

        // An empty directory → walk_eligible_files yields 0.
        let empty_tree = TempDir::new().unwrap();
        sqlx::query(
            "INSERT INTO watch_folders (watch_id, path, collection, tenant_id, enabled, is_archived, created_at, updated_at) \
             VALUES ('wf1', ?1, 'projects', 'tenant1', 1, 0, '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z')",
        )
        .bind(empty_tree.path().to_string_lossy().as_ref())
        .execute(&pool)
        .await
        .unwrap();
        // ...but the index holds files (the failure case: walk empty, index full).
        for rel in ["src/a.rs", "src/b.rs", "Makefile"] {
            seed_generation(&pool, rel, &format!("h_{rel}"), r#"["main"]"#).await;
        }

        let qm = Arc::new(QueueManager::new(pool.clone()));
        let stats = reconcile_ignore_rules(
            empty_tree.path(),
            "wf1",
            "tenant1",
            "projects",
            &pool,
            &qm,
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            stats.stale_deleted, 0,
            "must not delete against an empty walk"
        );
        assert_eq!(stats.missing_added, 0);
        let queued: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM unified_queue")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(queued.0, 0, "nothing may be enqueued when the walk fails");
    }

    /// #280: the rows diffed are the ones of the watch_id the CALLER supplies —
    /// two folders of the same tenant must reconcile independently.
    #[tokio::test]
    async fn reconcile_diffs_only_the_given_watch_folder() {
        use crate::queue_operations::QueueManager;
        use std::sync::Arc;
        use tempfile::TempDir;

        let pool = super::super::tests::create_test_pool().await;
        super::super::tests::setup_schema(&pool).await;

        // Two enabled folders, SAME tenant (clone + second clone). Distinct
        // paths — the production DDL enforces UNIQUE(watch_folders.path).
        let tree_a = TempDir::new().unwrap();
        let tree_b = TempDir::new().unwrap();
        std::fs::write(tree_a.path().join("kept.rs"), "fn a() {}\n").unwrap();
        for (wid, tree) in [("wfA", &tree_a), ("wfB", &tree_b)] {
            sqlx::query(
                "INSERT INTO watch_folders (watch_id, path, collection, tenant_id, enabled, is_archived, created_at, updated_at) \
                 VALUES (?1, ?2, 'projects', 'tenant-multi', 1, 0, '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z')",
            )
            .bind(wid)
            .bind(tree.path().to_string_lossy().as_ref())
            .execute(&pool)
            .await
            .unwrap();
        }
        // Folder B owns a row for a path that does NOT exist on disk (stale on B).
        sqlx::query(
            "INSERT INTO tracked_files \
             (watch_folder_id, relative_path, branches, file_mtime, file_hash, collection, base_point, created_at, updated_at) \
             VALUES ('wfB', 'gone.rs', '[]', '2025-01-01T00:00:00Z', 'hB', 'projects', 'bp', '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Reconciling folder A must NOT see folder B's stale row.
        let qm = Arc::new(QueueManager::new(pool.clone()));
        let stats = reconcile_ignore_rules(
            tree_a.path(),
            "wfA",
            "tenant-multi",
            "projects",
            &pool,
            &qm,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            stats.stale_deleted, 0,
            "folder B's rows must be invisible to folder A's diff"
        );
    }

    /// #280: the by-path resolver returns the folder whose path matches, with
    /// its worktree flag — never a tenant-scoped LIMIT 1 guess.
    #[tokio::test]
    async fn fetch_watch_folder_by_path_is_exact_and_flags_worktrees() {
        let pool = super::super::tests::create_test_pool().await;
        super::super::tests::setup_schema(&pool).await;

        sqlx::query(
            "INSERT INTO watch_folders (watch_id, path, collection, tenant_id, enabled, is_worktree, is_archived, created_at, updated_at) VALUES \
             ('wf-main', '/repo/main', 'projects', 't1', 1, 0, 0, '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z'), \
             ('wf-wt',   '/repo/main/.claude/worktrees/x', 'projects', 't1', 1, 1, 0, '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let main = fetch_watch_folder_by_path(&pool, Path::new("/repo/main"))
            .await
            .unwrap();
        assert_eq!(main, Some(("wf-main".to_string(), false)));

        let wt = fetch_watch_folder_by_path(&pool, Path::new("/repo/main/.claude/worktrees/x"))
            .await
            .unwrap();
        assert_eq!(wt, Some(("wf-wt".to_string(), true)));

        let miss = fetch_watch_folder_by_path(&pool, Path::new("/repo/other"))
            .await
            .unwrap();
        assert_eq!(miss, None);
    }

    /// The branch must come from the tree that was walked, not a DB lookup that
    /// can land on another clone/worktree of the same tenant.
    #[test]
    fn resolve_branch_reads_head_of_the_walked_tree() {
        use std::process::Command;
        use tempfile::TempDir;

        let repo = TempDir::new().unwrap();
        let run_git = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(repo.path())
                .status()
                .unwrap_or_else(|e| panic!("failed to run git {args:?}: {e}"));
            assert!(status.success(), "git {args:?} failed with {status}");
        };
        run_git(&["init", "-b", "dev-clean"]);
        run_git(&["config", "user.email", "wqm-test@example.invalid"]);
        run_git(&["config", "user.name", "WQM Test"]);
        fs::write(repo.path().join("README.md"), "test\n").unwrap();
        run_git(&["add", "README.md"]);
        run_git(&["commit", "-m", "init"]);

        assert_eq!(
            resolve_branch(repo.path(), "projects").as_deref(),
            Some("dev-clean")
        );
        assert_eq!(
            resolve_branch(repo.path(), "libraries"),
            None,
            "non-projects collections have no branch"
        );
    }
}
