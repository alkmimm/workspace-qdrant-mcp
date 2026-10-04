//! Branch membership of graph generations, read from the index authority.
//!
//! `tracked_files` holds one row per content generation of a path with the set
//! of branches that hold it (`branches`), and its `base_point` is the
//! generation key the graph rows carry. Reading membership from there at query
//! time — instead of mirroring it into `graph.db` — leaves nothing to drift:
//! every branch create, switch, dedup share and prune already maintains it.
//!
//! A feature branch is tagged only on the files it CHANGED; every other file
//! stays tagged under the trunk. So a branch's view is composed the way every
//! MCP read surface composes it (the trunk fill-in, #408/#409): its own
//! generations, plus the trunk's generation of each path the branch neither
//! holds nor changed between the two tips. A path the branch deleted or
//! rewrote is "changed", so the trunk's copy never stands in for it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use sqlx::{Row, SqlitePool};

use super::{GenerationBranches, GraphScope};

/// The branch label that asks for every branch at once (the pre-scoping view).
pub const ALL_BRANCHES: &str = "*";

/// A graph read's branch, resolved to the generations it may see.
#[derive(Debug, Clone)]
pub struct BranchGraphScope {
    /// The branch the answer describes: the requested one, or the branch the
    /// project's main folder has checked out when none was given. `*` when
    /// unscoped. Empty for a folder that is not a git repository (one tree,
    /// so every generation it holds is visible).
    pub branch: String,
    pub scope: GraphScope,
    /// Every generation the branch sees — its own plus the trunk's fill-in
    /// (every file, code or not): the denominator of the graph's coverage.
    pub visible: HashSet<String>,
}

/// One tracked generation of the tenant.
#[derive(Debug, Clone)]
pub(crate) struct TrackedRow {
    pub generation: String,
    pub path: String,
    /// Empty for a legacy row, which every branch holds.
    pub branches: Vec<String>,
}

impl TrackedRow {
    fn held_by(&self, branch: &str) -> bool {
        self.branches.is_empty() || self.branches.iter().any(|b| b == branch)
    }
}

/// Resolve `branch` (absent, empty or `*` included) for one tenant.
pub async fn resolve_branch_scope(
    pool: &SqlitePool,
    tenant_id: &str,
    branch: Option<&str>,
) -> Result<BranchGraphScope, sqlx::Error> {
    let rows = tracked_rows(pool, tenant_id).await?;
    let requested = branch.map(str::trim).filter(|b| !b.is_empty());
    if requested == Some(ALL_BRANCHES) {
        return Ok(BranchGraphScope {
            branch: ALL_BRANCHES.to_string(),
            scope: GraphScope::all(),
            visible: rows.into_iter().map(|r| r.generation).collect(),
        });
    }
    let root = main_root(pool, tenant_id).await?;
    let branch = match requested {
        Some(b) => Some(b.to_string()),
        None => root
            .as_deref()
            .and_then(|r| crate::git::head_branch(Path::new(r))),
    };
    let Some(branch) = branch else {
        // Not a git repository: one tree, whatever label its rows carry.
        let visible: HashSet<String> = rows.into_iter().map(|r| r.generation).collect();
        return Ok(BranchGraphScope {
            branch: String::new(),
            scope: GraphScope::generations(visible.clone()),
            visible,
        });
    };
    let root = root.map(PathBuf::from);
    let trunk = resolve_trunk(root.as_deref(), &rows, &branch);
    let changed = match (&root, &trunk) {
        (Some(root), Some(trunk)) if *trunk != branch => {
            let (root, trunk, head) = (root.clone(), trunk.clone(), branch.clone());
            tokio::task::spawn_blocking(move || {
                crate::git::paths_changed_between(&root, &trunk, &head)
            })
            .await
            .ok()
            .flatten()
        }
        _ => None,
    };
    // Fill in only for a branch that exists: git resolved its tip, or the
    // index tags it on at least one file. A misspelled name holds nothing and
    // must not quietly inherit the whole trunk.
    let exists = changed.is_some() || rows.iter().any(|r| r.branches.contains(&branch));
    let visible = compose_visible(
        &rows,
        &branch,
        trunk.as_deref().filter(|_| exists),
        changed.as_deref(),
    );
    Ok(BranchGraphScope {
        branch,
        scope: GraphScope::generations(visible.clone()),
        visible,
    })
}

/// A branch's view: its own generations, plus the trunk's generation of every
/// path the branch holds none of and did not change. With `changed` unknown
/// (git could not say) the trunk fills every path the branch does not hold —
/// the same answer the MCP read surfaces give when git is unavailable.
pub(crate) fn compose_visible(
    rows: &[TrackedRow],
    branch: &str,
    trunk: Option<&str>,
    changed: Option<&HashSet<String>>,
) -> HashSet<String> {
    let mut visible = HashSet::new();
    let mut own_paths: HashSet<&str> = HashSet::new();
    for r in rows.iter().filter(|r| r.held_by(branch)) {
        visible.insert(r.generation.clone());
        own_paths.insert(r.path.as_str());
    }
    if let Some(trunk) = trunk.filter(|t| *t != branch) {
        for r in rows.iter().filter(|r| r.held_by(trunk)) {
            if own_paths.contains(r.path.as_str()) || changed.is_some_and(|c| c.contains(&r.path)) {
                continue;
            }
            visible.insert(r.generation.clone());
        }
    }
    visible
}

/// The project's trunk, resolved like the MCP server's `resolveTrunkBranch`:
/// git's default branch when the index actually holds files under it, else the
/// branch tagging the most rows (an exact tie prefers `effective` — no
/// fill-in, the conservative answer).
pub(crate) fn resolve_trunk(
    root: Option<&Path>,
    rows: &[TrackedRow],
    effective: &str,
) -> Option<String> {
    let indexed = |b: &str| rows.iter().any(|r| r.branches.iter().any(|x| x == b));
    if let Some(git) = root.and_then(crate::git::default_branch) {
        if indexed(&git) {
            return Some(git);
        }
    }
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for r in rows {
        for b in &r.branches {
            *counts.entry(b.as_str()).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by(|(a, ca), (b, cb)| {
            ca.cmp(cb)
                .then_with(|| (*a == effective).cmp(&(*b == effective)))
                .then_with(|| b.cmp(a))
        })
        .map(|(b, _)| b.to_string())
}

/// The tenant's main watch folder (the one that is not a submodule child).
async fn main_root(pool: &SqlitePool, tenant_id: &str) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT path FROM watch_folders WHERE tenant_id = ?1
         ORDER BY (parent_watch_id IS NULL) DESC, enabled DESC LIMIT 1",
    )
    .bind(tenant_id)
    .fetch_optional(pool)
    .await
}

async fn tracked_rows(pool: &SqlitePool, tenant_id: &str) -> Result<Vec<TrackedRow>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT tf.base_point, tf.relative_path, tf.branches
         FROM tracked_files tf JOIN watch_folders w ON w.watch_id = tf.watch_folder_id
         WHERE w.tenant_id = ?1 AND tf.base_point IS NOT NULL",
    )
    .bind(tenant_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let raw: Option<String> = r.get("branches");
            TrackedRow {
                generation: r.get("base_point"),
                path: r.get("relative_path"),
                branches: raw
                    .as_deref()
                    .and_then(|j| serde_json::from_str(j).ok())
                    .unwrap_or_default(),
            }
        })
        .collect())
}

/// Every generation the tenant's index holds, on any branch.
pub async fn tracked_generations(
    pool: &SqlitePool,
    tenant_id: &str,
) -> Result<HashSet<String>, sqlx::Error> {
    Ok(tracked_rows(pool, tenant_id)
        .await?
        .into_iter()
        .map(|r| r.generation)
        .collect())
}

/// Which branches hold each of the tenant's generations, for the stub
/// resolver. The trunk's generations are visible on every branch (that is the
/// fill-in), so they count as co-visible with anything.
pub async fn generation_branches(
    pool: &SqlitePool,
    tenant_id: &str,
) -> Result<GenerationBranches, sqlx::Error> {
    let rows = tracked_rows(pool, tenant_id).await?;
    let root = main_root(pool, tenant_id).await?;
    let trunk = resolve_trunk(root.as_deref().map(Path::new), &rows, "");
    let universal: HashSet<String> = match &trunk {
        Some(t) => rows
            .iter()
            .filter(|r| r.branches.iter().any(|b| b == t))
            .map(|r| r.generation.clone())
            .collect(),
        None => HashSet::new(),
    };
    Ok(
        GenerationBranches::from_rows(rows.into_iter().map(|r| (r.generation, r.branches)))
            .with_universal(universal),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(path: &str, generation: &str, branches: &[&str]) -> TrackedRow {
        TrackedRow {
            generation: generation.to_string(),
            path: path.to_string(),
            branches: branches.iter().map(|b| b.to_string()).collect(),
        }
    }

    /// develop is the trunk. fase-5 rewrote a.ts, deleted old.ts, left b.ts and
    /// c.ts alone (only b.ts happens to be tagged with both branches — the
    /// daemon does not tag unchanged files under a feature branch).
    fn rows() -> Vec<TrackedRow> {
        vec![
            row("a.ts", "a-dev", &["develop"]),
            row("a.ts", "a-f5", &["fase-5"]),
            row("b.ts", "b-both", &["develop", "fase-5"]),
            row("c.ts", "c-dev", &["develop"]),
            row("old.ts", "old-dev", &["develop"]),
            row("legacy.ts", "legacy", &[]),
        ]
    }

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_feature_branch_sees_its_own_versions_plus_the_unchanged_trunk() {
        let changed = set(&["a.ts", "old.ts"]);
        let f5 = compose_visible(&rows(), "fase-5", Some("develop"), Some(&changed));
        assert_eq!(f5, set(&["a-f5", "b-both", "c-dev", "legacy"]));
        // c.ts is unchanged and untagged on fase-5: the trunk's copy IS its copy.
        // old.ts was deleted on fase-5 and a.ts rewritten: never filled.
    }

    #[test]
    fn the_trunk_sees_only_its_own_versions() {
        let dev = compose_visible(&rows(), "develop", Some("develop"), None);
        assert_eq!(dev, set(&["a-dev", "b-both", "c-dev", "old-dev", "legacy"]));
    }

    #[test]
    fn without_git_the_trunk_fills_every_path_the_branch_does_not_hold() {
        // Parity with the MCP read surfaces when git cannot answer.
        let f5 = compose_visible(&rows(), "fase-5", Some("develop"), None);
        assert!(f5.contains("old-dev") && f5.contains("c-dev"));
        assert!(
            !f5.contains("a-dev"),
            "the branch's own a.ts wins regardless"
        );
    }

    #[test]
    fn the_trunk_is_the_most_tagged_branch_and_a_tie_prefers_the_caller() {
        assert_eq!(
            resolve_trunk(None, &rows(), "fase-5").as_deref(),
            Some("develop")
        );
        let tie = vec![row("a", "1", &["x"]), row("b", "2", &["y"])];
        assert_eq!(resolve_trunk(None, &tie, "y").as_deref(), Some("y"));
        assert_eq!(resolve_trunk(None, &[], "y"), None);
    }

    fn commit(
        repo: &git2::Repository,
        files: &[(&str, &str)],
        parent: Option<git2::Oid>,
    ) -> git2::Oid {
        let mut builder = repo.treebuilder(None).unwrap();
        for (path, content) in files {
            let blob = repo.blob(content.as_bytes()).unwrap();
            builder.insert(path, blob, 0o100644).unwrap();
        }
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let sig = git2::Signature::now("t", "t@t").unwrap();
        let parents: Vec<git2::Commit> = parent
            .map(|p| repo.find_commit(p).unwrap())
            .into_iter()
            .collect();
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
        repo.commit(None, &sig, &sig, "c", &tree, &parent_refs)
            .unwrap()
    }

    async fn state_pool(root: &str) -> SqlitePool {
        // One connection: every `:memory:` connection is its own database.
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        for ddl in [
            "CREATE TABLE watch_folders (watch_id TEXT PRIMARY KEY, path TEXT NOT NULL,
                tenant_id TEXT NOT NULL, enabled INTEGER NOT NULL DEFAULT 1, parent_watch_id TEXT)",
            "CREATE TABLE tracked_files (file_id INTEGER PRIMARY KEY, watch_folder_id TEXT,
                relative_path TEXT, base_point TEXT, branches TEXT)",
        ] {
            sqlx::query(ddl).execute(&pool).await.unwrap();
        }
        sqlx::query("INSERT INTO watch_folders VALUES ('w1', ?1, 't1', 1, NULL)")
            .bind(root)
            .execute(&pool)
            .await
            .unwrap();
        for r in rows() {
            sqlx::query(
                "INSERT INTO tracked_files (watch_folder_id, relative_path, base_point, branches)
                 VALUES ('w1', ?1, ?2, ?3)",
            )
            .bind(&r.path)
            .bind(&r.generation)
            .bind(serde_json::to_string(&r.branches).unwrap())
            .execute(&pool)
            .await
            .unwrap();
        }
        pool
    }

    #[tokio::test]
    async fn the_view_is_composed_from_the_index_and_the_branch_tips() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        let dev = commit(
            &repo,
            &[("a.ts", "1"), ("b.ts", "1"), ("c.ts", "1"), ("old.ts", "1")],
            None,
        );
        let f5 = commit(
            &repo,
            &[("a.ts", "2"), ("b.ts", "1"), ("c.ts", "1")],
            Some(dev),
        );
        repo.reference("refs/heads/develop", dev, true, "").unwrap();
        repo.reference("refs/heads/fase-5", f5, true, "").unwrap();
        repo.set_head("refs/heads/fase-5").unwrap();
        let pool = state_pool(&dir.path().to_string_lossy()).await;

        let asked = resolve_branch_scope(&pool, "t1", Some("fase-5"))
            .await
            .unwrap();
        assert_eq!(asked.branch, "fase-5");
        assert_eq!(asked.visible, set(&["a-f5", "b-both", "c-dev", "legacy"]));
        assert!(!asked.scope.admits("old-dev"), "deleted on fase-5");
        assert!(!asked.scope.admits("a-dev"), "rewritten on fase-5");

        // No branch asked: the main folder's checkout (fase-5 here).
        let default = resolve_branch_scope(&pool, "t1", None).await.unwrap();
        assert_eq!(default.branch, "fase-5");
        assert_eq!(default.visible, asked.visible);

        // A misspelled branch holds nothing and inherits nothing.
        let typo = resolve_branch_scope(&pool, "t1", Some("fase5"))
            .await
            .unwrap();
        assert_eq!(typo.visible, set(&["legacy"]));

        let star = resolve_branch_scope(&pool, "t1", Some("*")).await.unwrap();
        assert!(!star.scope.is_scoped());
        assert_eq!(star.visible.len(), 6);
    }

    #[tokio::test]
    async fn a_non_git_folder_is_one_tree() {
        let pool = state_pool("/nonexistent/repo").await;
        let none = resolve_branch_scope(&pool, "t1", None).await.unwrap();
        assert_eq!(none.branch, "");
        assert_eq!(none.visible.len(), 6);
    }

    #[tokio::test]
    async fn membership_treats_the_trunk_as_visible_everywhere() {
        let pool = state_pool("/nonexistent/repo").await;
        let m = generation_branches(&pool, "t1").await.unwrap();
        assert!(
            m.co_visible("a-f5", "c-dev"),
            "develop's c.ts is fase-5's too"
        );
        assert!(m.co_visible("a-f5", "b-both"));
        assert!(m.co_visible("a-dev", "legacy"), "a row with no branch set");
    }
}
