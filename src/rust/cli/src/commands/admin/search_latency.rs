//! `wqm admin search-latency` — agent-usage observability over `search_events`.
//!
//! Three read-only views, all from data the MCP server's `updateSearchEvent`
//! path already logs but nothing aggregated:
//!   * (default) latency percentiles p50/p95/p99 of `latency_ms`, grouped by
//!     `tool` | `actor` | `op`. SQLite has no PERCENTILE_CONT, so percentiles
//!     are computed with a 2-CTE nearest-rank over ROW_NUMBER()/COUNT().
//!   * `--chain <session_id>` — reconstruct one session's tool-call chain in
//!     temporal order (P1.6.b part 1), classifying reformulations.
//!   * `--approve-proxy` — % of fast same-session follow-ups, a PROXY for
//!     client-side auto-approval (P1.6.c).
//!
//! Mirrors the structure of `token_savings.rs` (shared helpers reused from it).

use anyhow::{Context, Result};
use rusqlite::{params_from_iter, types::Value};
use serde::Serialize;
use tabled::Tabled;

use crate::output::canvas;
use crate::output::table::ColumnHints;
use crate::output::{self};

use super::perf_data::fmt_thousands;
use super::token_savings::{humanize_window, open_state_db, parse_window};

/// Auto-approve proxy threshold: a follow-up within this many seconds of the
/// previous same-session call counts as "fast" (~ the client auto-approved,
/// no human in the loop). Tunable — a slow LLM thinking between calls inflates
/// the gap, so this is a proxy, not ground truth.
const AUTO_APPROVE_GAP_S: f64 = 1.5;

// ── latency percentiles ───────────────────────────────────────────────────────

#[derive(Tabled, Serialize)]
struct LatencyRow {
    #[tabled(rename = "Group")]
    group: String,
    #[tabled(rename = "Calls")]
    calls: String,
    #[tabled(rename = "p50 ms")]
    p50: String,
    #[tabled(rename = "p95 ms")]
    p95: String,
    #[tabled(rename = "p99 ms")]
    p99: String,
    #[tabled(rename = "max ms")]
    max: String,
}

impl ColumnHints for LatencyRow {
    fn content_columns() -> &'static [usize] {
        &[0]
    }
    fn numeric_columns() -> &'static [usize] {
        &[1, 2, 3, 4, 5]
    }
}

#[derive(Debug, Serialize)]
struct LatencyAgg {
    group: String,
    calls: i64,
    p50: i64,
    p95: i64,
    p99: i64,
    max: i64,
}

/// Whitelist the group-by column before interpolation (no injection surface).
fn group_column(g: &str) -> Result<&'static str> {
    match g {
        "tool" => Ok("tool"),
        "actor" => Ok("actor"),
        "op" => Ok("op"),
        other => anyhow::bail!("--group-by must be tool|actor|op (got '{}')", other),
    }
}

fn query_latency(
    conn: &rusqlite::Connection,
    col: &str,
    window_hours: f64,
    project: Option<&str>,
) -> Result<Vec<LatencyAgg>> {
    // Nearest-rank percentile in pure SQLite: `counts` gives n per group,
    // `ranked` numbers each group's latency ascending; we pick the value at
    // rank = max(1, round(p*(n-1))+1). `max(1, …)` is the SCALAR max (2 args);
    // `MAX(CASE …)` / `MAX(v)` are aggregates (1 arg). `col` is whitelisted.
    let proj = if project.is_some() {
        " AND project_id = ?2"
    } else {
        ""
    };
    let sql = format!(
        "WITH base AS ( \
            SELECT {col} AS grp, latency_ms AS v FROM search_events \
            WHERE latency_ms IS NOT NULL AND ts >= datetime('now', ?1){proj} \
         ), \
         counts AS (SELECT grp, COUNT(*) AS n FROM base GROUP BY grp), \
         ranked AS ( \
            SELECT grp, v, ROW_NUMBER() OVER (PARTITION BY grp ORDER BY v) AS rn FROM base \
         ) \
         SELECT r.grp, c.n AS calls, \
            MAX(CASE WHEN r.rn = MAX(1, CAST(ROUND(0.50*(c.n-1)) AS INTEGER)+1) THEN r.v END) AS p50, \
            MAX(CASE WHEN r.rn = MAX(1, CAST(ROUND(0.95*(c.n-1)) AS INTEGER)+1) THEN r.v END) AS p95, \
            MAX(CASE WHEN r.rn = MAX(1, CAST(ROUND(0.99*(c.n-1)) AS INTEGER)+1) THEN r.v END) AS p99, \
            (SELECT MAX(v) FROM base b WHERE b.grp = r.grp) AS maxv \
         FROM ranked r JOIN counts c ON c.grp = r.grp \
         GROUP BY r.grp, c.n ORDER BY calls DESC",
        col = col,
        proj = proj,
    );
    let mut params: Vec<Value> = vec![Value::Text(format!("-{} hours", window_hours))];
    if let Some(p) = project {
        params.push(Value::Text(p.to_string()));
    }
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(params.iter()), |r| {
        Ok(LatencyAgg {
            group: r.get::<_, Option<String>>(0)?.unwrap_or_else(|| "<null>".into()),
            calls: r.get::<_, i64>(1)?,
            p50: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
            p95: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
            p99: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
            max: r.get::<_, Option<i64>>(5)?.unwrap_or(0),
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.context("decode latency row")?);
    }
    Ok(out)
}

fn print_latency_table(aggs: &[LatencyAgg], window_hours: f64, group_by: &str) {
    canvas::print_title(&format!(
        "Search Latency (last {}, by {})",
        humanize_window(window_hours),
        group_by
    ));
    canvas::print_blank();
    let rows: Vec<LatencyRow> = aggs
        .iter()
        .map(|a| LatencyRow {
            group: a.group.clone(),
            calls: fmt_thousands(a.calls),
            p50: fmt_thousands(a.p50),
            p95: fmt_thousands(a.p95),
            p99: fmt_thousands(a.p99),
            max: fmt_thousands(a.max),
        })
        .collect();
    output::print_table_auto(&rows);
}

// ── session tool-call chain (P1.6.b part 1) ───────────────────────────────────

#[derive(Tabled, Serialize)]
struct ChainRow {
    #[tabled(rename = "ts")]
    ts: String,
    #[tabled(rename = "tool")]
    tool: String,
    #[tabled(rename = "op")]
    op: String,
    #[tabled(rename = "query")]
    query: String,
    #[tabled(rename = "hits")]
    hits: String,
    #[tabled(rename = "ms")]
    ms: String,
    #[tabled(rename = "link")]
    link: String,
}

impl ColumnHints for ChainRow {
    fn content_columns() -> &'static [usize] {
        &[0, 1, 2, 3, 6]
    }
    fn numeric_columns() -> &'static [usize] {
        &[4, 5]
    }
}

#[derive(Debug, Serialize)]
struct ChainEvent {
    ts: String,
    tool: String,
    op: String,
    query: String,
    hits: Option<i64>,
    latency_ms: Option<i64>,
    link: String,
}

fn query_chain(conn: &rusqlite::Connection, session_id: &str) -> Result<Vec<ChainEvent>> {
    // Order a session's events by ts (the only chaining signal currently
    // written) and diff adjacent rows to flag reformulations. `escalation`
    // depends on parent_event_id, which NO current MCP call site writes — so it
    // is effectively inert today (see the note printed below the table).
    let sql = "WITH ordered AS ( \
            SELECT id, ts, tool, op, query_text, result_count, latency_ms, parent_event_id, \
                   LAG(query_text) OVER w AS prev_query, LAG(op) OVER w AS prev_op, \
                   (julianday(ts) - julianday(LAG(ts) OVER w)) * 86400.0 AS gap_s \
            FROM search_events WHERE session_id = ?1 WINDOW w AS (ORDER BY ts) \
         ) \
         SELECT ts, tool, op, substr(COALESCE(query_text,''),1,48) AS query, \
                result_count, latency_ms, \
                CASE WHEN prev_op = op AND op IN ('search','search_exact') \
                          AND gap_s < 60 AND prev_query IS NOT query_text THEN 'reformulation' \
                     WHEN parent_event_id IS NOT NULL THEN 'escalation' \
                     ELSE 'root' END AS link \
         FROM ordered ORDER BY ts";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([session_id], |r| {
        Ok(ChainEvent {
            ts: r.get::<_, Option<String>>(0)?.unwrap_or_default(),
            tool: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            op: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            query: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
            hits: r.get::<_, Option<i64>>(4)?,
            latency_ms: r.get::<_, Option<i64>>(5)?,
            link: r.get::<_, Option<String>>(6)?.unwrap_or_default(),
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.context("decode chain row")?);
    }
    Ok(out)
}

fn print_chain_table(events: &[ChainEvent], session_id: &str) {
    canvas::print_title(&format!("Session tool-call chain — {}", session_id));
    canvas::print_blank();
    if events.is_empty() {
        output::info("No events for that session_id.".to_string());
        return;
    }
    let rows: Vec<ChainRow> = events
        .iter()
        .map(|e| ChainRow {
            ts: e.ts.clone(),
            tool: e.tool.clone(),
            op: e.op.clone(),
            query: e.query.clone(),
            hits: e.hits.map(|h| h.to_string()).unwrap_or_else(|| "—".into()),
            ms: e.latency_ms.map(|m| m.to_string()).unwrap_or_else(|| "—".into()),
            link: e.link.clone(),
        })
        .collect();
    output::print_table_auto(&rows);
    output::info(
        "Note: 'escalation' depends on parent_event_id, which no MCP call site \
         currently writes, so the v38 token_savings view's had_followup/had_escalation \
         are inert until that is wired. Chain order + 'reformulation' come from \
         session_id + ts."
            .to_string(),
    );
}

// ── auto-approve proxy (P1.6.c) ───────────────────────────────────────────────

#[derive(Tabled, Serialize)]
struct ProxyRow {
    #[tabled(rename = "Tool")]
    tool: String,
    #[tabled(rename = "Chained")]
    chained_calls: String,
    #[tabled(rename = "Fast-followup %")]
    fast_followup_pct: String,
}

impl ColumnHints for ProxyRow {
    fn content_columns() -> &'static [usize] {
        &[0]
    }
    fn numeric_columns() -> &'static [usize] {
        &[1, 2]
    }
}

#[derive(Debug, Serialize)]
struct ProxyAgg {
    tool: String,
    chained_calls: i64,
    fast_followup_pct: Option<f64>,
}

fn query_proxy(conn: &rusqlite::Connection) -> Result<Vec<ProxyAgg>> {
    let sql = "WITH gaps AS ( \
            SELECT tool, \
                   (julianday(ts) - julianday(LAG(ts) OVER (PARTITION BY session_id ORDER BY ts))) \
                       * 86400.0 AS gap_s \
            FROM search_events \
         ) \
         SELECT tool, COUNT(*) AS chained_calls, \
                ROUND(100.0 * SUM(CASE WHEN gap_s < ?1 THEN 1 ELSE 0 END) / COUNT(*), 1) AS fast_pct \
         FROM gaps WHERE gap_s IS NOT NULL GROUP BY tool ORDER BY chained_calls DESC";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([AUTO_APPROVE_GAP_S], |r| {
        Ok(ProxyAgg {
            tool: r.get::<_, Option<String>>(0)?.unwrap_or_default(),
            chained_calls: r.get::<_, i64>(1)?,
            fast_followup_pct: r.get::<_, Option<f64>>(2)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.context("decode proxy row")?);
    }
    Ok(out)
}

fn print_proxy_table(aggs: &[ProxyAgg]) {
    canvas::print_title("Auto-approve proxy (fast same-session follow-ups)");
    canvas::print_blank();
    if aggs.is_empty() {
        output::info("No chained same-session calls in search_events.".to_string());
        return;
    }
    let rows: Vec<ProxyRow> = aggs
        .iter()
        .map(|a| ProxyRow {
            tool: a.tool.clone(),
            chained_calls: fmt_thousands(a.chained_calls),
            fast_followup_pct: a
                .fast_followup_pct
                .map(|p| format!("{:.1}%", p))
                .unwrap_or_else(|| "—".into()),
        })
        .collect();
    output::print_table_auto(&rows);
    output::info(format!(
        "PROXY only (gap < {}s ~ client auto-approved); the server never sees the \
         client's approval decision.",
        AUTO_APPROVE_GAP_S
    ));
}

// ── entrypoint ────────────────────────────────────────────────────────────────

/// Execute the `search-latency` subcommand.
pub async fn execute(
    window: String,
    group_by: String,
    project: Option<String>,
    json: bool,
    chain: Option<String>,
    approve_proxy: bool,
) -> Result<()> {
    let conn = open_state_db()?;

    if let Some(session_id) = chain {
        let events = query_chain(&conn, &session_id).context("Failed to query session chain")?;
        if json {
            println!("{}", serde_json::to_string_pretty(&events).unwrap());
        } else {
            print_chain_table(&events, &session_id);
        }
        return Ok(());
    }

    if approve_proxy {
        let aggs = query_proxy(&conn).context("Failed to query auto-approve proxy")?;
        if json {
            println!("{}", serde_json::to_string_pretty(&aggs).unwrap());
        } else {
            print_proxy_table(&aggs);
        }
        return Ok(());
    }

    let window_hours = parse_window(&window)?;
    let col = group_column(&group_by)?;
    let aggs = query_latency(&conn, col, window_hours, project.as_deref())
        .context("Failed to query search_events latency")?;

    if aggs.is_empty() {
        if json {
            println!("[]");
        } else {
            output::info(format!(
                "No search events with latency in the last {}.",
                humanize_window(window_hours)
            ));
        }
        return Ok(());
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&aggs).unwrap());
    } else {
        print_latency_table(&aggs, window_hours, &group_by);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory `search_events` with one row per latency value, stamped `now`
    /// so the default window includes them.
    fn mem_db(latencies: &[i64]) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE search_events ( \
                id INTEGER PRIMARY KEY, ts TEXT, session_id TEXT, project_id TEXT, \
                actor TEXT, tool TEXT, op TEXT, query_text TEXT, result_count INTEGER, \
                latency_ms INTEGER, parent_event_id TEXT);",
        )
        .unwrap();
        {
            let mut stmt = conn
                .prepare(
                    "INSERT INTO search_events (ts, session_id, tool, op, latency_ms) \
                     VALUES (datetime('now'), 's1', 'mcp_qdrant', 'search', ?1)",
                )
                .unwrap();
            for v in latencies {
                stmt.execute([*v]).unwrap();
            }
        }
        conn
    }

    #[test]
    fn group_column_whitelists() {
        assert_eq!(group_column("tool").unwrap(), "tool");
        assert_eq!(group_column("actor").unwrap(), "actor");
        assert_eq!(group_column("op").unwrap(), "op");
        assert!(group_column("latency_ms; DROP TABLE x").is_err());
    }

    #[test]
    fn nearest_rank_percentiles_on_1_to_100() {
        let conn = mem_db(&(1i64..=100).collect::<Vec<i64>>());
        let aggs = query_latency(&conn, "tool", 1000.0, None).unwrap();
        assert_eq!(aggs.len(), 1);
        let a = &aggs[0];
        assert_eq!(a.calls, 100);
        // round(p*(n-1))+1 over 1..100 (value == rank): p50→51, p95→95, p99→99.
        assert_eq!(a.p50, 51);
        assert_eq!(a.p95, 95);
        assert_eq!(a.p99, 99);
        assert_eq!(a.max, 100);
    }

    #[test]
    fn single_row_group_is_not_null() {
        let conn = mem_db(&[42i64]);
        let aggs = query_latency(&conn, "tool", 1000.0, None).unwrap();
        let a = &aggs[0];
        assert_eq!((a.calls, a.p50, a.p95, a.p99, a.max), (1, 42, 42, 42, 42));
    }
}
