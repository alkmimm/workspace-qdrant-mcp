//! Branch membership of graph generations, read from the index authority.
//!
//! `tracked_files` holds one row per content generation of a path with the set
//! of branches that hold it (`branches`), and its `base_point` is the
//! generation key the graph rows carry. Reading membership from there at query
//! time — instead of mirroring it into `graph.db` — leaves nothing to drift:
//! every branch create, switch, dedup share and prune already maintains it.

use std::collections::HashSet;
use std::path::Path;

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
    /// The generations the branch holds in the index (every file, code or
    /// not) — the denominator of the graph's coverage of this branch.
    pub held: HashSet<String>,
}

/// Rows of a tenant's tracked generations that `branch` holds. A row with no
/// branch set is a legacy row visible on every branch, as in every other read.
const BRANCH_GENERATIONS_SQL: &str = "SELECT DISTINCT tf.base_point
     FROM tracked_files tf JOIN watch_folders w ON w.watch_id = tf.watch_folder_id
     WHERE w.tenant_id = ?1 AND tf.base_point IS NOT NULL
       AND (COALESCE(json_array_length(tf.branches), 0) = 0
            OR EXISTS (SELECT 1 FROM json_each(tf.branches) WHERE value = ?2))";

/// Resolve `branch` (absent, empty or `*` included) for one tenant.
pub async fn resolve_branch_scope(
    pool: &SqlitePool,
    tenant_id: &str,
    branch: Option<&str>,
) -> Result<BranchGraphScope, sqlx::Error> {
    let requested = branch.map(str::trim).filter(|b| !b.is_empty());
    if requested == Some(ALL_BRANCHES) {
        return Ok(BranchGraphScope {
            branch: ALL_BRANCHES.to_string(),
            scope: GraphScope::all(),
            held: tracked_generations(pool, tenant_id).await?,
        });
    }
    let branch = match requested {
        Some(b) => Some(b.to_string()),
        None => default_branch(pool, tenant_id).await?,
    };
    let Some(branch) = branch else {
        // Not a git repository: one tree, whatever label its rows carry.
        let held = tracked_generations(pool, tenant_id).await?;
        return Ok(BranchGraphScope {
            branch: String::new(),
            scope: GraphScope::generations(held.clone()),
            held,
        });
    };
    let held: HashSet<String> = sqlx::query_scalar(BRANCH_GENERATIONS_SQL)
        .bind(tenant_id)
        .bind(&branch)
        .fetch_all(pool)
        .await?
        .into_iter()
        .collect();
    Ok(BranchGraphScope {
        branch,
        scope: GraphScope::generations(held.clone()),
        held,
    })
}

/// The branch the tenant's main folder has checked out (`None` when it is not
/// a git repository, or when the tenant has no watch folder at all).
async fn default_branch(pool: &SqlitePool, tenant_id: &str) -> Result<Option<String>, sqlx::Error> {
    let root: Option<String> = sqlx::query_scalar(
        "SELECT path FROM watch_folders WHERE tenant_id = ?1
         ORDER BY (parent_watch_id IS NULL) DESC, enabled DESC LIMIT 1",
    )
    .bind(tenant_id)
    .fetch_optional(pool)
    .await?;
    Ok(root.and_then(|r| crate::git::head_branch(Path::new(&r))))
}

/// Every generation the tenant's index holds, on any branch.
pub async fn tracked_generations(
    pool: &SqlitePool,
    tenant_id: &str,
) -> Result<HashSet<String>, sqlx::Error> {
    Ok(sqlx::query_scalar(
        "SELECT DISTINCT tf.base_point
         FROM tracked_files tf JOIN watch_folders w ON w.watch_id = tf.watch_folder_id
         WHERE w.tenant_id = ?1 AND tf.base_point IS NOT NULL",
    )
    .bind(tenant_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect())
}

/// Which branches hold each of the tenant's generations.
pub async fn generation_branches(
    pool: &SqlitePool,
    tenant_id: &str,
) -> Result<GenerationBranches, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT tf.base_point, tf.branches
         FROM tracked_files tf JOIN watch_folders w ON w.watch_id = tf.watch_folder_id
         WHERE w.tenant_id = ?1 AND tf.base_point IS NOT NULL",
    )
    .bind(tenant_id)
    .fetch_all(pool)
    .await?;
    Ok(GenerationBranches::from_rows(rows.iter().map(|r| {
        let generation: String = r.get("base_point");
        let raw: Option<String> = r.get("branches");
        let branches: Vec<String> = raw
            .as_deref()
            .and_then(|j| serde_json::from_str(j).ok())
            .unwrap_or_default();
        (generation, branches)
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn state_pool() -> SqlitePool {
        // One connection: every `:memory:` connection is its own database.
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE watch_folders (watch_id TEXT PRIMARY KEY, path TEXT NOT NULL,
                tenant_id TEXT NOT NULL, enabled INTEGER NOT NULL DEFAULT 1,
                parent_watch_id TEXT)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE tracked_files (file_id INTEGER PRIMARY KEY, watch_folder_id TEXT,
                relative_path TEXT, base_point TEXT, branches TEXT)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // A path that is not a git repository: the default branch is "none".
        sqlx::query("INSERT INTO watch_folders VALUES ('w1', '/nonexistent/repo', 't1', 1, NULL)")
            .execute(&pool)
            .await
            .unwrap();
        for (path, bp, branches) in [
            ("a.ts", "a-dev", r#"["develop"]"#),
            ("a.ts", "a-f5", r#"["fase-5"]"#),
            ("b.ts", "b-both", r#"["develop","fase-5"]"#),
            ("old.ts", "old-dev", r#"["develop"]"#),
            ("legacy.ts", "legacy", "[]"),
        ] {
            sqlx::query(
                "INSERT INTO tracked_files (watch_folder_id, relative_path, base_point, branches)
                 VALUES ('w1', ?1, ?2, ?3)",
            )
            .bind(path)
            .bind(bp)
            .bind(branches)
            .execute(&pool)
            .await
            .unwrap();
        }
        pool
    }

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[tokio::test]
    async fn a_branch_sees_its_own_versions_and_nothing_a_sibling_holds() {
        let pool = state_pool().await;
        let f5 = resolve_branch_scope(&pool, "t1", Some("fase-5"))
            .await
            .unwrap();
        assert_eq!(f5.branch, "fase-5");
        assert_eq!(f5.held, set(&["a-f5", "b-both", "legacy"]));
        assert!(f5.scope.admits("a-f5"));
        assert!(!f5.scope.admits("a-dev"), "develop's version of a.ts");
        assert!(!f5.scope.admits("old-dev"), "a file fase-5 deleted");

        let dev = resolve_branch_scope(&pool, "t1", Some("develop"))
            .await
            .unwrap();
        assert_eq!(dev.held, set(&["a-dev", "b-both", "old-dev", "legacy"]));
    }

    #[tokio::test]
    async fn star_is_unscoped_and_a_non_git_folder_sees_its_one_tree() {
        let pool = state_pool().await;
        let star = resolve_branch_scope(&pool, "t1", Some("*")).await.unwrap();
        assert_eq!(star.branch, "*");
        assert!(!star.scope.is_scoped());
        assert_eq!(star.held.len(), 5);

        // No branch asked and the folder is not a git repository.
        let none = resolve_branch_scope(&pool, "t1", None).await.unwrap();
        assert_eq!(none.branch, "");
        assert_eq!(none.held.len(), 5);
        assert!(none.scope.admits("a-dev") && none.scope.admits("a-f5"));
    }

    #[tokio::test]
    async fn an_unknown_branch_holds_only_legacy_rows() {
        let pool = state_pool().await;
        let typo = resolve_branch_scope(&pool, "t1", Some("fase5"))
            .await
            .unwrap();
        assert_eq!(typo.held, set(&["legacy"]));
    }

    #[tokio::test]
    async fn membership_maps_each_generation_to_its_branches() {
        let pool = state_pool().await;
        let m = generation_branches(&pool, "t1").await.unwrap();
        assert!(!m.co_visible("a-dev", "a-f5"));
        assert!(m.co_visible("a-f5", "b-both"));
        assert!(m.co_visible("a-dev", "legacy"), "a row with no branch set");
    }
}
