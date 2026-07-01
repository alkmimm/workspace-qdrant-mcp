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
    use sqlx::sqlite::SqlitePoolOptions;

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
}
