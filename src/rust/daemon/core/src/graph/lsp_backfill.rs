//! R8.3 — LSP-authoritative call backfill (correctness-critical, testable core).
//!
//! Converts LSP call-hierarchy resolutions into authoritative CALLS over an
//! already-ingested graph. The live orchestration (activate the tenant's LSP
//! within the global server cap, iterate callers, resolve, stamp via
//! `make_calls_authoritative`, deactivate — serial across tenants, flag-gated)
//! is wired in memexd; this module holds the piece that must be exactly right:
//! mapping an LSP `ResolvedCall` to the REAL callee node in the graph.
//!
//! Why a lookup and not `compute_node_id(tenant, file, name, Function)`: the LSP
//! returns only `(name, file, line)`, not the symbol KIND. A Dart `.build()`
//! callee is a `method`, whose node_id differs from a Function-typed guess — so
//! we resolve the callee by `(file, name)`, preferring the node whose
//! `start_line` is closest to the LSP line. This is exactly the method-call case
//! R8 exists to fix, so getting the kind right is load-bearing.

use std::path::Path;
use std::sync::Arc;

use sqlx::Row;
use tokio::sync::RwLock;

use super::{NodeType, SharedGraphStore, SqliteGraphStore};
use crate::lsp::{symbol_column_in_line, LanguageServerManager, ResolvedCall};

/// A callable definition in the graph (a potential caller with outgoing calls).
#[derive(Debug, Clone)]
pub(crate) struct CallerNode {
    pub file_path: String,
    pub symbol_name: String,
    pub node_type: NodeType,
    pub start_line: u32,
}

/// Relativize an absolute LSP path to the project-relative form the graph keys
/// on. `None` for paths outside the project (stdlib/deps — no node here).
fn relativize(abs_file: &str, project_root: &str) -> Option<String> {
    let f = abs_file.replace('\\', "/");
    let root = project_root.replace('\\', "/");
    let root = root.trim_end_matches('/');
    let rel = f.strip_prefix(root)?.trim_start_matches('/');
    (!rel.is_empty()).then(|| rel.to_string())
}

/// All callable definitions for a tenant (functions/methods with a known line) —
/// the set the backfill asks the LSP to resolve outgoing calls for.
pub(crate) async fn tenant_callers(
    store: &SharedGraphStore<SqliteGraphStore>,
    tenant_id: &str,
) -> Vec<CallerNode> {
    let guard = store.read().await;
    let rows = sqlx::query(
        "SELECT file_path, symbol_name, symbol_type, start_line
         FROM graph_nodes
         WHERE tenant_id = ?1 AND file_path <> '' AND start_line IS NOT NULL
           AND symbol_type IN ('function','async_function','method')",
    )
    .bind(tenant_id)
    .fetch_all(guard.pool())
    .await
    .unwrap_or_default();
    rows.iter()
        .filter_map(|r| {
            let st: String = r.get("symbol_type");
            Some(CallerNode {
                file_path: r.get("file_path"),
                symbol_name: r.get("symbol_name"),
                node_type: NodeType::from_str(&st)?,
                start_line: r.get::<i64, _>("start_line") as u32,
            })
        })
        .collect()
}

/// Resolve an LSP-reported callee to its REAL graph node_id: the node in
/// `rel_file` named `name`, preferring the one whose `start_line` is closest to
/// the LSP `line` (so a method binds to its Method node, not a Function guess).
/// `None` when the callee has no node here (out-of-project / not indexed).
async fn callee_node_id(
    store: &SharedGraphStore<SqliteGraphStore>,
    tenant_id: &str,
    rel_file: &str,
    name: &str,
    line: u32,
) -> Option<String> {
    let guard = store.read().await;
    let rows = sqlx::query(
        "SELECT node_id, start_line FROM graph_nodes
         WHERE tenant_id = ?1 AND file_path = ?2 AND symbol_name = ?3",
    )
    .bind(tenant_id)
    .bind(rel_file)
    .bind(name)
    .fetch_all(guard.pool())
    .await
    .ok()?;
    rows.iter()
        .min_by_key(|r| {
            let sl = r
                .get::<Option<i64>, _>("start_line")
                .unwrap_or(i64::MAX / 2);
            (sl - line as i64).unsigned_abs()
        })
        .map(|r| r.get::<String, _>("node_id"))
}

/// Convert a caller's LSP `resolved` outgoing calls into args for
/// `GraphStore::make_calls_authoritative`: the distinct callee NAMES the LSP
/// resolved (to clear the fuzzy fan-out) and the REAL callee node_ids (to insert
/// precise edges). Out-of-project / unindexed callees are dropped.
pub(crate) async fn authoritative_args_for_caller(
    store: &SharedGraphStore<SqliteGraphStore>,
    tenant_id: &str,
    project_root: &str,
    resolved: &[ResolvedCall],
) -> (Vec<String>, Vec<String>) {
    let mut names: Vec<String> = Vec::new();
    let mut targets: Vec<String> = Vec::new();
    for call in resolved {
        let Some(rel) = relativize(&call.file, project_root) else {
            continue;
        };
        let Some(node_id) = callee_node_id(store, tenant_id, &rel, &call.name, call.line).await
        else {
            continue; // out-of-project or not indexed — leave the fuzzy edge.
        };
        if !names.contains(&call.name) {
            names.push(call.name.clone());
        }
        if !targets.contains(&node_id) {
            targets.push(node_id);
        }
    }
    (names, targets)
}

/// Live backfill entry for ONE tenant whose LSP is already activated + warm: for
/// each caller, ask the LSP for outgoing calls and stamp authoritative CALLS via
/// `make_calls_authoritative`. Best-effort per caller (a failed resolve simply
/// leaves the fuzzy edge). Returns the number of fuzzy edges superseded.
///
/// The memexd scheduler owns activation: it must start the tenant's language
/// server (within the global server cap) BEFORE calling this and stop it after —
/// see `docs/plans/2026-07-01-r8-lsp-authoritative-edges-plan.md`. This is a
/// no-op when the LSP resolves nothing (cold/unsupported), so it is safe to call
/// speculatively.
pub async fn run_backfill_tenant(
    store: &SharedGraphStore<SqliteGraphStore>,
    lsp: &Arc<RwLock<LanguageServerManager>>,
    tenant_id: &str,
    project_root: &str,
) -> u64 {
    let callers = tenant_callers(store, tenant_id).await;
    let root = project_root.trim_end_matches('/');
    let mut superseded: u64 = 0;
    for caller in &callers {
        let abs = format!("{root}/{}", caller.file_path);
        // Column of the symbol on its definition line (UTF-16, LSP encoding),
        // read best-effort from the source (0 if the file/line is unavailable).
        let column = tokio::fs::read_to_string(&abs)
            .await
            .ok()
            .and_then(|c| {
                c.lines()
                    .nth(caller.start_line as usize)
                    .map(|l| symbol_column_in_line(l, &caller.symbol_name))
            })
            .unwrap_or(0);
        let resolved = {
            let mgr = lsp.read().await;
            mgr.resolved_outgoing_calls(Path::new(&abs), caller.start_line, column)
                .await
                .unwrap_or_default()
        };
        if resolved.is_empty() {
            continue;
        }
        let (names, targets) =
            authoritative_args_for_caller(store, tenant_id, project_root, &resolved).await;
        if names.is_empty() {
            continue;
        }
        let caller_id = super::compute_node_id(
            tenant_id,
            &caller.file_path,
            &caller.symbol_name,
            caller.node_type,
        );
        if let Ok(n) = store
            .make_calls_authoritative(tenant_id, &caller_id, &caller.file_path, &names, &targets)
            .await
        {
            superseded += n;
        }
    }
    superseded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::GraphNode;

    async fn mk_store() -> SharedGraphStore<SqliteGraphStore> {
        use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(":memory:")
                    .create_if_missing(true),
            )
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE graph_nodes (
                node_id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, symbol_name TEXT NOT NULL,
                symbol_type TEXT NOT NULL, file_path TEXT NOT NULL, start_line INTEGER,
                end_line INTEGER, signature TEXT, language TEXT,
                created_at TEXT NOT NULL DEFAULT '', updated_at TEXT NOT NULL DEFAULT '')",
        )
        .execute(&pool)
        .await
        .unwrap();
        SharedGraphStore::new(SqliteGraphStore::new(pool))
    }

    #[tokio::test]
    async fn authoritative_args_resolve_method_and_function_to_real_nodes() {
        let t = "t1";
        let store = mk_store().await;
        // A METHOD `build` (the DOC-V2 case) and a free FUNCTION `helper`.
        let build = {
            let mut n = GraphNode::new(t, "lib/widget.dart", "build", NodeType::Method);
            n.start_line = Some(42);
            n
        };
        let helper = {
            let mut n = GraphNode::new(t, "lib/util.dart", "helper", NodeType::Function);
            n.start_line = Some(3);
            n
        };
        store.upsert_nodes(&[build.clone(), helper.clone()]).await.unwrap();

        // LSP resolved the caller's calls to those absolute paths + lines.
        let resolved = vec![
            ResolvedCall {
                name: "build".into(),
                file: "/home/u/doc-v2/lib/widget.dart".into(),
                line: 42,
            },
            ResolvedCall {
                name: "helper".into(),
                file: "/home/u/doc-v2/lib/util.dart".into(),
                line: 3,
            },
            // Out-of-project callee (Flutter SDK) — must be dropped.
            ResolvedCall {
                name: "setState".into(),
                file: "/opt/flutter/lib/framework.dart".into(),
                line: 100,
            },
        ];

        let (names, targets) =
            authoritative_args_for_caller(&store, t, "/home/u/doc-v2", &resolved).await;

        assert_eq!(names, vec!["build".to_string(), "helper".to_string()]);
        // The METHOD `build` binds to its Method node_id (NOT a Function guess).
        assert!(
            targets.contains(&build.node_id),
            "build must resolve to the real Method node"
        );
        assert!(targets.contains(&helper.node_id));
        assert_eq!(targets.len(), 2, "out-of-project setState dropped");
        assert!(
            !names.contains(&"setState".to_string()),
            "out-of-project callee must not clear a fuzzy edge"
        );
    }
}
