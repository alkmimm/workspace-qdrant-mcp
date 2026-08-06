//! SQLite write-contention helpers: IMMEDIATE-lock transactions with BUSY retry.
//!
//! Under WAL the daemon has a single writer; the queue processor's concurrent
//! item tasks all serialize on that writer. Two failure modes surfaced as
//! `database is locked` during heavy reembed load:
//!   - `SQLITE_BUSY` (5): a writer could not take the write lock within
//!     `busy_timeout`.
//!   - `SQLITE_BUSY_SNAPSHOT` (517): a *deferred* transaction that read first and
//!     then tried to upgrade to a write after another writer committed — its read
//!     snapshot went stale. `busy_timeout` does NOT cover this; it fails at once.
//!
//! [`begin_immediate`] fixes both by starting the transaction with `BEGIN
//! IMMEDIATE`: it takes the write lock up front, so
//!   - 517 becomes impossible — there is no deferred read→write upgrade; and
//!   - the only point contention can occur is the `BEGIN` itself, which
//!     `busy_timeout` already waits on and which we additionally retry here with
//!     jittered backoff. Once `BEGIN IMMEDIATE` returns, the transaction owns the
//!     write lock, so the body writes and the `COMMIT` cannot hit `SQLITE_BUSY`.
//!
//! Callers just swap `pool.begin()` for `begin_immediate(pool)`.

use std::time::Duration;

use sqlx::{Sqlite, SqlitePool, Transaction};
use tracing::warn;

/// Max `BEGIN IMMEDIATE` retries on a busy/locked error before surfacing it.
/// Layered under the queue's own item-level `retry_count`.
pub const MAX_BUSY_RETRIES: u32 = 6;

/// True if `err` is a transient SQLite busy/locked condition worth retrying:
/// `SQLITE_BUSY` (5), `SQLITE_LOCKED` (6), `SQLITE_LOCKED_SHAREDCACHE` (261),
/// `SQLITE_BUSY_SNAPSHOT` (517). Checks the driver code first, with a message
/// fallback so it stays correct across sqlx code representations.
pub fn is_sqlite_busy(err: &sqlx::Error) -> bool {
    if let Some(db) = err.as_database_error() {
        if let Some(code) = db.code() {
            if matches!(code.as_ref(), "5" | "6" | "261" | "517") {
                return true;
            }
        }
        let m = db.message();
        return m.contains("database is locked") || m.contains("database table is locked");
    }
    false
}

/// Jittered exponential backoff for a busy retry: ~25ms doubling per attempt,
/// capped at 2s, each with +[0, half) ms of jitter so concurrent tasks don't all
/// re-collide on the write lock. Uses the wall clock for cheap jitter to avoid
/// pulling in `rand`.
pub fn busy_backoff(attempt: u32) -> Duration {
    let base_ms = 25u64.saturating_mul(1u64 << attempt.min(6)).min(2000);
    let half = base_ms / 2;
    let jitter = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()))
        .unwrap_or(0)
        % (half + 1);
    Duration::from_millis(half + jitter)
}

/// Begin a transaction that acquires the write lock immediately (`BEGIN
/// IMMEDIATE`) instead of sqlx's default deferred `BEGIN`, retrying the `BEGIN`
/// on a transient busy/locked error with jittered backoff. See the module docs.
pub async fn begin_immediate(
    pool: &SqlitePool,
) -> Result<Transaction<'static, Sqlite>, sqlx::Error> {
    let mut attempt: u32 = 0;
    loop {
        match pool.begin_with("BEGIN IMMEDIATE").await {
            Ok(tx) => return Ok(tx),
            Err(e) if attempt < MAX_BUSY_RETRIES && is_sqlite_busy(&e) => {
                attempt += 1;
                let delay = busy_backoff(attempt);
                warn!(
                    "BEGIN IMMEDIATE: SQLite busy/locked, retry {}/{} after {:?}",
                    attempt, MAX_BUSY_RETRIES, delay
                );
                tokio::time::sleep(delay).await;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
    use std::str::FromStr;

    #[test]
    fn backoff_grows_and_is_bounded() {
        for a in 1..=8 {
            let d = busy_backoff(a).as_millis();
            assert!(d > 0 && d <= 2000, "attempt {a} -> {d}ms out of range");
        }
    }

    #[tokio::test]
    async fn begin_immediate_yields_a_usable_write_tx() {
        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .execute(&pool)
            .await
            .unwrap();

        let mut tx = begin_immediate(&pool).await.unwrap();
        sqlx::query("INSERT INTO t (v) VALUES ('x')")
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM t")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 1);
    }

    /// Concurrency regression for the read-then-write hazard `begin_immediate`
    /// exists to fix (queue dequeue path; PRs #329/#330 did the same for the
    /// search.db). A DEFERRED `pool.begin()` that SELECTs first and then UPDATEs
    /// takes a read snapshot and must upgrade the lock at the write; under
    /// contention SQLite fails that upgrade with `SQLITE_BUSY` (5) /
    /// `SQLITE_BUSY_SNAPSHOT` (517) *immediately*, without invoking the busy
    /// handler — so `busy_timeout` does NOT cover it and the transaction is lost.
    ///
    /// This must use a FILE-backed WAL pool: `sqlite::memory:` gives each pooled
    /// connection its own private database, so it cannot reproduce cross-
    /// connection write contention. Each task runs a genuine read-then-write
    /// (`SELECT v` → `UPDATE v = v+1`) with a data dependency, so a dropped or
    /// lost transaction shows up as BOTH a surfaced error AND a final count below
    /// the expected total. With `begin_immediate` every task takes the write lock
    /// up front, serializes cleanly, and all increments land.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_read_then_write_txs_all_commit_without_lock_errors() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db_retry_concurrency.db");

        // Mirror the state.db pool's contention-relevant settings: WAL + a
        // per-connection busy_timeout (see queue_config.rs). `begin_immediate`
        // layers its own jittered retry on top of this.
        let opts = SqliteConnectOptions::from_str(&format!("sqlite:{}", db_path.display()))
            .unwrap()
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(30))
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await
            .unwrap();

        sqlx::query("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO t (id, v) VALUES (1, 0)")
            .execute(&pool)
            .await
            .unwrap();

        const N_TASKS: i64 = 8;
        const N_ITERS: i64 = 10;

        let mut handles = Vec::new();
        for _ in 0..N_TASKS {
            let pool = pool.clone();
            handles.push(tokio::spawn(async move {
                for _ in 0..N_ITERS {
                    let mut tx = begin_immediate(&pool).await?;
                    // READ first — this is what makes a deferred BEGIN vulnerable.
                    let v: i64 = sqlx::query_scalar("SELECT v FROM t WHERE id = 1")
                        .fetch_one(&mut *tx)
                        .await?;
                    // ...then WRITE a value derived from the read.
                    sqlx::query("UPDATE t SET v = ?1 WHERE id = 1")
                        .bind(v + 1)
                        .execute(&mut *tx)
                        .await?;
                    tx.commit().await?;
                }
                Ok::<(), sqlx::Error>(())
            }));
        }

        for handle in handles {
            handle
                .await
                .expect("task panicked")
                .expect("every read-then-write tx must commit (no database is locked / 517)");
        }

        let final_v: i64 = sqlx::query_scalar("SELECT v FROM t WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        // Exact total proves serialized isolation — no lost updates, no dropped tx.
        assert_eq!(final_v, N_TASKS * N_ITERS);
    }
}
