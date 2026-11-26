//! Idle backfill of graph generations the index holds but the graph lacks.
//!
//! Graph rows are per content generation (`base_point`). An ordinary ingest
//! writes its generation's graph, but nothing rewrites an unchanged file — so
//! after the graph.db v7 rebuild, and for any generation whose extraction
//! failed, the graph would stay empty for every file nobody edits. While the
//! queue is idle this walks the tracked generations, finds the ones with no
//! extraction record, and rebuilds each from a checkout that has exactly that
//! version on disk: the main folder for its HEAD branch, a linked worktree for
//! a worktree branch. A generation whose bytes no checkout holds is read from
//! git instead — the tip of a live branch that holds it (see
//! `stage_from_git`); one no live tip holds either (a branch that moved on,
//! or only deleted branches) waits, and its graph is built by the dedup heal or
//! by this pass on a later refresh.

use std::collections::{HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use sqlx::{Row, SqlitePool};
use tracing::{debug, info, warn};
use wqm_common::timestamps;

use crate::context::ProcessingContext;
use crate::git::BranchCheckouts;
use crate::graph::GRAPH_EXTRACTOR_VERSION;
use crate::tracked_files_schema::compute_file_hash;

/// Delay between completed sweeps.
const REFRESH_INTERVAL: Duration = Duration::from_secs(300);
const PAGE_SIZE: i64 = 512;
/// Wall-clock budget of one idle step: the queue may fill meanwhile, so the
/// step yields well before a user-visible delay.
pub(crate) const STEP_BUDGET: Duration = Duration::from_millis(1500);

/// One generation to rebuild.
#[derive(Debug, Clone)]
struct Pending {
    tenant_id: String,
    collection: String,
    watch_root: PathBuf,
    relative_path: String,
    generation: String,
    file_hash: String,
    branches: Vec<String>,
}

/// State of the backfill across idle steps.
#[derive(Debug, Default)]
pub(crate) struct GraphBackfill {
    pending: VecDeque<Pending>,
    sweep: Option<Sweep>,
    finished_at: Option<Instant>,
    scanned: u64,
    rebuilt: u64,
    from_git: u64,
    unreachable: u64,
}

#[derive(Debug)]
struct Sweep {
    folders: VecDeque<String>,
    last_id: i64,
    upper_id: i64,
    queued: HashSet<(String, String)>,
}

impl Sweep {
    async fn start(pool: &SqlitePool) -> Result<Self, sqlx::Error> {
        let upper_id = sqlx::query_scalar("SELECT COALESCE(MAX(file_id), 0) FROM tracked_files")
            .fetch_one(pool)
            .await?;
        let folders = sqlx::query_scalar(
            "SELECT watch_id FROM watch_folders WHERE enabled = 1
             ORDER BY is_active DESC, tenant_id, watch_id",
        )
        .fetch_all(pool)
        .await?
        .into();
        Ok(Self {
            folders,
            last_id: 0,
            upper_id,
            queued: HashSet::new(),
        })
    }

    async fn page(
        &self,
        pool: &SqlitePool,
        watch_id: &str,
    ) -> Result<Vec<sqlx::sqlite::SqliteRow>, sqlx::Error> {
        // Bound the number of tracked rows visited, including rows without a
        // base point. The watch-folder index also orders ties by rowid/file_id.
        sqlx::query(
            "SELECT tf.file_id, w.tenant_id, w.path, tf.relative_path, tf.base_point,
                    tf.file_hash, tf.branches, COALESCE(tf.collection, w.collection) AS collection
             FROM watch_folders w
             JOIN tracked_files tf ON tf.watch_folder_id = w.watch_id
             WHERE w.watch_id = ?1 AND w.enabled = 1
               AND tf.file_id > ?2 AND tf.file_id <= ?3
             ORDER BY tf.file_id LIMIT ?4",
        )
        .bind(watch_id)
        .bind(self.last_id)
        .bind(self.upper_id)
        .bind(PAGE_SIZE)
        .fetch_all(pool)
        .await
    }

    fn advance(&mut self, rows: &[sqlx::sqlite::SqliteRow]) {
        if rows.len() < PAGE_SIZE as usize {
            self.folders.pop_front();
            self.last_id = 0;
        } else if let Some(last) = rows.last() {
            self.last_id = last.get("file_id");
        }
    }
}

impl GraphBackfill {
    /// Discover at most one page and rebuild within the idle work budget.
    /// Returns true when the queue should be polled before more maintenance.
    pub(crate) async fn step(&mut self, ctx: &ProcessingContext, budget: Duration) -> bool {
        let Some(ref graph) = ctx.graph_store else {
            return false;
        };
        let started = Instant::now();
        if self.pending.is_empty() {
            if self.sweep.is_none() {
                if self
                    .finished_at
                    .is_some_and(|at| at.elapsed() < REFRESH_INTERVAL)
                {
                    return false;
                }
                match Sweep::start(&ctx.pool).await {
                    Ok(sweep) => self.sweep = Some(sweep),
                    Err(e) => {
                        warn!(error = %e, "graph backfill: could not start sweep");
                        self.finish();
                        return false;
                    }
                }
            }
            match self.discover_page(ctx).await {
                Ok(true) => {}
                Ok(false) => {
                    self.finish();
                    return false;
                }
                Err(e) => {
                    warn!(error = %e, "graph backfill: page discovery failed");
                    self.finish();
                    return false;
                }
            }
        }
        while !self.pending.is_empty() && started.elapsed() < budget {
            // Work can arrive while discovering a page or rebuilding a file.
            // A delayed retry is not claimable and must not stall backfill.
            match has_ready_work(&ctx.pool).await {
                Ok(true) => return true,
                Ok(false) => {}
                Err(e) => {
                    debug!(error = %e, "graph backfill: queue probe failed");
                    return true;
                }
            }
            let Some(p) = self.pending.pop_front() else {
                break;
            };
            match graph
                .generation_extracted(&p.tenant_id, &p.generation)
                .await
            {
                Ok(true) => continue, // an ingest got there first
                Ok(false) => {}
                Err(e) => {
                    warn!(error = %e, "graph backfill: generation probe failed");
                    self.finish();
                    return false;
                }
            }
            let (abs, staged) = match locate_version(&p) {
                Some(abs) => (abs, None),
                None => match stage_from_git(&p) {
                    Some(staged) => (staged.path.clone(), Some(staged)),
                    None => {
                        self.unreachable += 1;
                        continue;
                    }
                },
            };
            let abs_str = abs.to_string_lossy().to_string();
            super::graph_ingest::rebuild_generation(
                ctx,
                &p.tenant_id,
                &p.collection,
                &abs,
                &p.relative_path,
                &abs_str,
                &p.watch_root.to_string_lossy(),
                &p.generation,
                // Bytes read from git are in no checkout the project's language
                // server sees; resolving their calls there would answer for
                // another branch's code.
                staged.is_none(),
            )
            .await;
            if staged.is_some() {
                self.from_git += 1;
            }
            self.rebuilt += 1;
        }
        false
    }

    /// Discover missing generations only for the page's watch folder.
    async fn discover_page(&mut self, ctx: &ProcessingContext) -> Result<bool, sqlx::Error> {
        let (Some(sweep), Some(graph)) = (self.sweep.as_mut(), ctx.graph_store.as_ref()) else {
            return Ok(false);
        };
        let Some(watch_id) = sweep.folders.front() else {
            return Ok(false);
        };
        let rows = sweep.page(&ctx.pool, watch_id).await?;
        if let Some(first) = rows.first() {
            let tenant_id: &str = first.get("tenant_id");
            let generations: Vec<&str> = rows
                .iter()
                .filter_map(|row| row.get::<Option<&str>, _>("base_point"))
                .collect();
            let current = {
                let store = graph.read().await;
                current_generations(store.pool(), tenant_id, &generations).await?
            };
            for row in &rows {
                let (Some(generation), Some(relative_path)) = (
                    row.get::<Option<&str>, _>("base_point"),
                    row.get::<Option<&str>, _>("relative_path"),
                ) else {
                    continue;
                };
                if current.contains(generation)
                    || !sweep
                        .queued
                        .insert((tenant_id.to_owned(), generation.to_owned()))
                {
                    continue;
                }
                let raw: Option<&str> = row.get("branches");
                self.pending.push_back(Pending {
                    tenant_id: tenant_id.to_owned(),
                    collection: row.get("collection"),
                    watch_root: PathBuf::from(row.get::<&str, _>("path")),
                    relative_path: relative_path.to_owned(),
                    generation: generation.to_owned(),
                    file_hash: row.get("file_hash"),
                    branches: raw
                        .and_then(|json| serde_json::from_str(json).ok())
                        .unwrap_or_default(),
                });
            }
        }
        self.scanned += rows.len() as u64;
        sweep.advance(&rows);
        Ok(true)
    }

    fn finish(&mut self) {
        if self.sweep.is_some() {
            info!(
                scanned = self.scanned,
                page_size = PAGE_SIZE,
                rebuilt = self.rebuilt,
                from_git = self.from_git,
                unreachable = self.unreachable,
                "Graph backfill sweep ended"
            );
        }
        *self = Self {
            finished_at: Some(Instant::now()),
            ..Self::default()
        };
    }
}

async fn current_generations(
    pool: &SqlitePool,
    tenant_id: &str,
    generations: &[&str],
) -> Result<HashSet<String>, sqlx::Error> {
    let current = sqlx::query_scalar(
        "SELECT generation FROM graph_generations
         WHERE tenant_id = ?1 AND extractor_version >= ?2
           AND generation IN (SELECT value FROM json_each(?3))",
    )
    .bind(tenant_id)
    .bind(GRAPH_EXTRACTOR_VERSION)
    .bind(sqlx::types::Json(generations))
    .fetch_all(pool)
    .await?;
    Ok(current.into_iter().collect())
}

async fn has_ready_work(pool: &SqlitePool) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM unified_queue
             WHERE status = 'pending' AND (lease_until IS NULL OR lease_until < ?1)
             UNION ALL
             SELECT 1 FROM unified_queue WHERE status = 'in_progress' AND lease_until < ?1
         )",
    )
    .bind(timestamps::now_utc())
    .fetch_one(pool)
    .await
}

/// The on-disk copy of exactly this version: the file at its path in a
/// checkout whose bytes hash to the generation's. The checkouts of the
/// branches that hold the version are tried first; then every other checkout,
/// because a version is often byte-identical elsewhere (a trunk nobody has
/// checked out shares most files with the main folder's branch). A row with
/// no branch set is a legacy row; its one tree is the watch root.
fn locate_version(p: &Pending) -> Option<PathBuf> {
    let checkouts = BranchCheckouts::discover_cached(&p.watch_root);
    let mut roots: Vec<&Path> = if p.branches.is_empty() {
        vec![p.watch_root.as_path()]
    } else {
        p.branches
            .iter()
            .filter_map(|b| checkouts.root_for(b))
            .collect()
    };
    for root in checkouts.all_roots() {
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    roots.into_iter().find_map(|root| {
        let abs = root.join(&p.relative_path);
        let matches =
            abs.is_file() && compute_file_hash(&abs).is_ok_and(|hash| hash == p.file_hash);
        matches.then_some(abs)
    })
}

/// A version staged from git; its temp dir lives as long as this does.
struct StagedVersion {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

/// A version no checkout holds, read from git: the tip of a branch that holds
/// it and that nobody has checked out — a trunk beside a main folder on another
/// branch, a merged branch whose local ref survives (1,284 such versions on the
/// live stack, 2026-10-05). Staged in a temp dir at its relative path, because
/// language detection reads the name, and accepted only when it hashes to the
/// generation's own hash: a tip that moved on no longer holds this version.
fn stage_from_git(p: &Pending) -> Option<StagedVersion> {
    // The relative path becomes a path under the temp dir: plain names only.
    let plain = Path::new(&p.relative_path)
        .components()
        .all(|c| matches!(c, Component::Normal(_)));
    if !plain {
        return None;
    }
    p.branches.iter().find_map(|branch| {
        let bytes = crate::git::blob_at_branch_tip(&p.watch_root, branch, &p.relative_path)?;
        let dir = tempfile::Builder::new()
            .prefix("wqm-graph-version-")
            .tempdir()
            .ok()?;
        let path = dir.path().join(&p.relative_path);
        std::fs::create_dir_all(path.parent()?).ok()?;
        std::fs::write(&path, bytes).ok()?;
        let matches = compute_file_hash(&path).is_ok_and(|hash| hash == p.file_hash);
        matches.then_some(StagedVersion { _dir: dir, path })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sweep_pages_in_active_order_and_defers_rows_added_mid_sweep() {
        use crate::tracked_files_schema::schema::{
            CREATE_TRACKED_FILES_V41_INDEXES_SQL, CREATE_TRACKED_FILES_V41_SQL,
        };
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(crate::watch_folders_schema::CREATE_WATCH_FOLDERS_SQL)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(CREATE_TRACKED_FILES_V41_SQL)
            .execute(&pool)
            .await
            .unwrap();
        for index in CREATE_TRACKED_FILES_V41_INDEXES_SQL {
            sqlx::query(index).execute(&pool).await.unwrap();
        }
        sqlx::query(
            "INSERT INTO watch_folders (watch_id, path, collection, tenant_id,
                                         is_active, enabled, created_at, updated_at)
             VALUES ('idle', '/idle', 'projects', 'a', 0, 1, '', ''),
                    ('active', '/active', 'projects', 'b', 1, 1, '', ''),
                    ('disabled', '/disabled', 'projects', 'c', 1, 0, '', '')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let insert = "INSERT INTO tracked_files (watch_folder_id, relative_path, file_hash,
                                                 file_mtime, created_at, updated_at)
                      SELECT 'active', CAST(value AS TEXT), 'hash', '', '', ''
                      FROM json_each(?1)";
        sqlx::query(insert)
            .bind(sqlx::types::Json((1..=PAGE_SIZE + 1).collect::<Vec<_>>()))
            .execute(&pool)
            .await
            .unwrap();

        let mut sweep = Sweep::start(&pool).await.unwrap();
        assert_eq!(
            sweep.folders,
            VecDeque::from([String::from("active"), String::from("idle")])
        );
        sqlx::query(insert)
            .bind("[9999]")
            .execute(&pool)
            .await
            .unwrap();

        let first = sweep.page(&pool, "active").await.unwrap();
        assert_eq!(first.len(), PAGE_SIZE as usize);
        assert_eq!(first[0].get::<i64, _>("file_id"), 1);
        sweep.advance(&first);

        let last = sweep.page(&pool, "active").await.unwrap();
        assert_eq!(last.len(), 1);
        assert_eq!(last[0].get::<i64, _>("file_id"), PAGE_SIZE + 1);
        sweep.advance(&last);
        assert_eq!(sweep.folders, VecDeque::from([String::from("idle")]));
        assert!(sweep.page(&pool, "idle").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn page_probe_filters_by_generation_tenant_and_extractor_version() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE graph_generations (
                 tenant_id TEXT, generation TEXT, extractor_version INTEGER,
                 PRIMARY KEY (tenant_id, generation)
             )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO graph_generations VALUES
             ('project', 'current', ?1), ('project', 'stale', ?1 - 1),
             ('project', 'outside-page', ?1), ('other', 'elsewhere', ?1)",
        )
        .bind(GRAPH_EXTRACTOR_VERSION)
        .execute(&pool)
        .await
        .unwrap();

        let page = ["current", "stale", "elsewhere", "missing"];
        assert_eq!(
            current_generations(&pool, "project", &page).await.unwrap(),
            HashSet::from([String::from("current")]),
        );
    }

    fn pending(root: &Path, rel: &str, hash: &str, branches: &[&str]) -> Pending {
        Pending {
            tenant_id: "t".into(),
            collection: "projects".into(),
            watch_root: root.to_path_buf(),
            relative_path: rel.into(),
            generation: "g".into(),
            file_hash: hash.into(),
            branches: branches.iter().map(|b| b.to_string()).collect(),
        }
    }

    #[test]
    fn a_version_is_found_only_where_its_bytes_are() {
        // Not a git repository: one tree, whatever label the rows carry.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "fn a() {}\n").unwrap();
        let hash = compute_file_hash(&file).unwrap();

        assert_eq!(
            locate_version(&pending(dir.path(), "a.rs", &hash, &["main"])),
            Some(file.clone())
        );
        // Another version of the same path: the disk holds different bytes.
        assert_eq!(
            locate_version(&pending(dir.path(), "a.rs", "not-this-hash", &["main"])),
            None
        );
        // A path no checkout has.
        assert_eq!(
            locate_version(&pending(dir.path(), "gone.rs", &hash, &["main"])),
            None
        );
        // A branch with no checkout of its own: found wherever the bytes are.
        assert_eq!(
            locate_version(&pending(dir.path(), "a.rs", &hash, &["develop"])),
            Some(file.clone())
        );
        // A legacy row (no branch set) is looked up in the watch root.
        assert_eq!(
            locate_version(&pending(dir.path(), "a.rs", &hash, &[])),
            Some(file)
        );
    }

    /// A repository whose branch `feat` holds `src/a.rs` in git only: no
    /// checkout has that version on disk.
    fn repo_with_branch_only_version(content: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        let blob = repo.blob(content.as_bytes()).unwrap();
        let mut src = repo.treebuilder(None).unwrap();
        src.insert("a.rs", blob, 0o100644).unwrap();
        let mut root = repo.treebuilder(None).unwrap();
        root.insert("src", src.write().unwrap(), 0o040000).unwrap();
        let tree = repo.find_tree(root.write().unwrap()).unwrap();
        let sig = git2::Signature::now("t", "t@t").unwrap();
        let commit = repo.commit(None, &sig, &sig, "c", &tree, &[]).unwrap();
        repo.reference("refs/heads/feat", commit, true, "").unwrap();
        dir
    }

    #[test]
    fn a_version_only_git_holds_is_staged_from_its_branch_tip() {
        let content = "fn feat() {}\n";
        let dir = repo_with_branch_only_version(content);
        let hash = wqm_common::hashing::compute_content_hash(content);
        // Held by a deleted branch and by `feat`, which no checkout has.
        let p = pending(dir.path(), "src/a.rs", &hash, &["gone", "feat"]);
        assert_eq!(locate_version(&p), None, "no checkout holds it");

        let staged = stage_from_git(&p).expect("staged from feat's tip");
        assert_eq!(
            compute_file_hash(&staged.path).unwrap(),
            hash,
            "feat's bytes"
        );
        assert!(
            staged.path.ends_with("src/a.rs"),
            "staged under its own name, for language detection"
        );

        // Another version of the path: the tip does not hold it.
        let other = pending(dir.path(), "src/a.rs", "another-hash", &["feat"]);
        assert!(stage_from_git(&other).is_none());
        // Only deleted branches hold it: nothing to read.
        let dead = pending(dir.path(), "src/a.rs", &hash, &["gone"]);
        assert!(stage_from_git(&dead).is_none());
        // A path that would leave the staging dir is never written.
        let escape = pending(dir.path(), "../a.rs", &hash, &["feat"]);
        assert!(stage_from_git(&escape).is_none());
    }
}
