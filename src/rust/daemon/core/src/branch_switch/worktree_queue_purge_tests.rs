//! Dropping queued worktree work once the worktree is gone
//! (`worktree_queue_purge`).

use std::collections::HashSet;
use std::path::Path;

use sqlx::SqlitePool;

use crate::allowed_extensions::AllowedExtensions;
use crate::queue_operations::QueueManager;

use super::tests::{create_test_pool, insert_watch_folder, setup_tables};
use super::worktree_queue_purge::{
    purge_checkoutless_worktree_work, CheckoutlessPurge, PurgeTarget,
};
use super::WorktreeDiscovery;

const OLD: &str = "2020-01-01T00:00:00.000Z";
const CUTOFF: &str = "2026-01-01T00:00:00.000Z";
const BASELINE: &str = r#"{"worktree_membership":true}"#;
const WT_READ: &str = r#"{"worktree_membership":true,"read_root":"/repo/.claude/worktrees/x"}"#;
const STAGED: &str =
    r#"{"worktree_membership":true,"read_root":"/stage/t/b/tip","git_stage":true}"#;

/// One queue row with the fields the purge filters on.
struct Row<'a> {
    id: &'a str,
    tenant: &'a str,
    branch: &'a str,
    op: &'a str,
    status: &'a str,
    created_at: &'a str,
    metadata: Option<&'a str>,
}

impl<'a> Row<'a> {
    /// A pending, old worktree-baseline add of tenant `t1`.
    fn add(id: &'a str, branch: &'a str) -> Self {
        Self {
            id,
            tenant: "t1",
            branch,
            op: "add",
            status: "pending",
            created_at: OLD,
            metadata: Some(BASELINE),
        }
    }

    async fn insert(&self, pool: &SqlitePool) {
        sqlx::query(
            "INSERT INTO unified_queue (queue_id, item_type, op, tenant_id, collection, status, \
             created_at, branch, file_path, idempotency_key, metadata) \
             VALUES (?1, 'file', ?2, ?3, 'projects', ?4, ?5, ?6, ?7, ?1, ?8)",
        )
        .bind(self.id)
        .bind(self.op)
        .bind(self.tenant)
        .bind(self.status)
        .bind(self.created_at)
        .bind(self.branch)
        .bind(format!("src/{}.rs", self.id))
        .bind(self.metadata)
        .execute(pool)
        .await
        .unwrap();
    }
}

async fn queue_ids(pool: &SqlitePool) -> Vec<String> {
    sqlx::query_scalar("SELECT queue_id FROM unified_queue ORDER BY queue_id")
        .fetch_all(pool)
        .await
        .unwrap()
}

fn set(names: &[&str]) -> HashSet<String> {
    names.iter().map(|s| s.to_string()).collect()
}

/// Only the worktree reconcile's pending adds of a branch no checkout has are
/// dropped. The tip follower's staged items, other producers' adds, deletes,
/// held items, work queued after the cutoff, checked-out branches, the trunk
/// names and other tenants all keep their rows.
#[tokio::test]
async fn drops_only_worktree_adds_of_branches_without_checkout() {
    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    let not_git = tempfile::tempdir().unwrap();

    Row::add("d-baseline", "agent/gone").insert(&pool).await;
    Row {
        metadata: Some(WT_READ),
        ..Row::add("d-wt-read", "agent/gone")
    }
    .insert(&pool)
    .await;
    Row {
        metadata: Some(STAGED),
        ..Row::add("k-staged", "agent/gone")
    }
    .insert(&pool)
    .await;
    Row {
        metadata: None,
        ..Row::add("k-plain", "agent/gone")
    }
    .insert(&pool)
    .await;
    Row {
        metadata: Some("not json"),
        ..Row::add("k-badjson", "agent/gone")
    }
    .insert(&pool)
    .await;
    Row {
        op: "delete",
        ..Row::add("k-delete", "agent/gone")
    }
    .insert(&pool)
    .await;
    Row {
        status: "in_progress",
        ..Row::add("k-held", "agent/gone")
    }
    .insert(&pool)
    .await;
    Row {
        created_at: "2999-01-01T00:00:00.000Z",
        ..Row::add("k-new", "agent/gone")
    }
    .insert(&pool)
    .await;
    Row::add("k-live", "agent/live").insert(&pool).await;
    Row::add("k-main", "main").insert(&pool).await;
    Row::add("k-master", "master").insert(&pool).await;
    Row {
        tenant: "t2",
        ..Row::add("k-other", "agent/gone")
    }
    .insert(&pool)
    .await;

    let removed = purge_checkoutless_worktree_work(
        &pool,
        "t1",
        not_git.path(),
        &set(&["agent/live"]),
        CUTOFF,
    )
    .await
    .expect("purge");

    assert_eq!(removed, 2);
    assert_eq!(
        queue_ids(&pool).await,
        vec![
            "k-badjson",
            "k-delete",
            "k-held",
            "k-live",
            "k-main",
            "k-master",
            "k-new",
            "k-other",
            "k-plain",
            "k-staged",
        ]
    );
}

/// A project is purged on first sight and again only when its checkout set
/// changes — not on every discovery pass.
#[tokio::test]
async fn purges_once_per_checkout_set() {
    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    let not_git = tempfile::tempdir().unwrap();
    let root = not_git.path().to_string_lossy().to_string();
    let target = PurgeTarget {
        watch_id: "w1",
        tenant_id: "t1",
        root: &root,
    };
    let mut purge = CheckoutlessPurge::default();

    Row::add("a", "agent/gone").insert(&pool).await;
    assert_eq!(
        purge.run(&pool, &target, set(&["agent/a"]), CUTOFF).await,
        1
    );

    Row::add("b", "agent/gone").insert(&pool).await;
    assert_eq!(
        purge.run(&pool, &target, set(&["agent/a"]), CUTOFF).await,
        0,
        "same checkout set: not purged again"
    );
    assert_eq!(
        purge.run(&pool, &target, set(&[]), CUTOFF).await,
        1,
        "a worktree went away: purged"
    );
}

/// End to end through discovery: a project with one linked worktree keeps that
/// worktree's queued work and loses the work of a worktree already removed.
#[tokio::test]
async fn discovery_drops_the_work_of_a_removed_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let main_repo = tmp.path().join("main");
    let main_git = main_repo.join(".git");
    std::fs::create_dir_all(&main_git).unwrap();
    std::fs::write(main_git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    add_linked_worktree(&main_repo, "wt-live", "agent/live");

    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    insert_watch_folder(&pool, "w1", "t1", &main_repo.to_string_lossy()).await;
    Row::add("k-live", "agent/live").insert(&pool).await;
    Row::add("d-removed", "agent/removed").insert(&pool).await;

    let qm = QueueManager::new(pool.clone());
    let mut discovery = WorktreeDiscovery::default();
    discovery
        .tick(&pool, &qm, &AllowedExtensions::default())
        .await;

    assert_eq!(queue_ids(&pool).await, vec!["k-live"]);
}

/// Register a linked worktree on `branch` under `<main>/.claude/worktrees/<name>`.
fn add_linked_worktree(main_repo: &Path, name: &str, branch: &str) {
    let wt = main_repo.join(".claude").join("worktrees").join(name);
    std::fs::create_dir_all(&wt).unwrap();
    let admin = main_repo.join(".git").join("worktrees").join(name);
    std::fs::create_dir_all(&admin).unwrap();
    std::fs::write(admin.join("gitdir"), format!("{}/.git\n", wt.display())).unwrap();
    std::fs::write(admin.join("HEAD"), format!("ref: refs/heads/{branch}\n")).unwrap();
    std::fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
}
