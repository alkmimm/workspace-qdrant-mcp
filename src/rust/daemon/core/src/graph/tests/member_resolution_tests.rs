//! Class members and their callers: homonymous methods of two classes in one
//! file, calls through an instance variable, and interface implementations.
//!
//! Live 2026-10-07 (Finance, `firestore_finance_writes.dart`): the graph held
//! ONE `set` for two classes, every call to `FirestoreFinanceBatch.set` landed
//! on `FirestoreFinanceTransaction.set`, the tests' calls were "ambiguous", and
//! test_gaps reported the method as untested with 47 production dependents.

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::Row;

use super::*;
use crate::graph::algorithms::detect_test_gaps;
use crate::tree_sitter::types::{ChunkType, SemanticChunk};

const T: &str = "test-tenant";

async fn store() -> SharedGraphStore<SqliteGraphStore> {
    let opts = SqliteConnectOptions::new()
        .filename(":memory:")
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();
    crate::graph::schema::apply_graph_schema(&pool).await;
    SharedGraphStore::new(SqliteGraphStore::new(pool))
}

fn class(name: &str, content: &str, lines: (usize, usize), file: &str) -> SemanticChunk {
    SemanticChunk::new(
        ChunkType::Class,
        name,
        content,
        lines.0,
        lines.1,
        "dart",
        file,
    )
}

fn method(
    parent: &str,
    name: &str,
    content: &str,
    lines: (usize, usize),
    file: &str,
    calls: &[&str],
) -> SemanticChunk {
    SemanticChunk::new(
        ChunkType::Method,
        name,
        content,
        lines.0,
        lines.1,
        "dart",
        file,
    )
    .with_parent(parent)
    .with_calls(calls.iter().map(|c| c.to_string()).collect())
}

fn function(name: &str, content: &str, file: &str, calls: &[&str]) -> SemanticChunk {
    SemanticChunk::new(ChunkType::Function, name, content, 1, 20, "dart", file)
        .with_calls(calls.iter().map(|c| c.to_string()).collect())
}

async fn ingest(
    store: &SharedGraphStore<SqliteGraphStore>,
    file: &str,
    generation: &str,
    chunks: &[SemanticChunk],
) {
    let extracted = extractor::extract_edges(chunks, T, file);
    store
        .reingest_file(T, file, generation, &extracted.nodes, &extracted.edges)
        .await
        .unwrap();
}

/// Resolved CALLS of `caller`: (target symbol, target parent, weight, metadata).
async fn calls_of(
    store: &SharedGraphStore<SqliteGraphStore>,
    caller: &str,
) -> Vec<(String, Option<String>, f64, String)> {
    let guard = store.read().await;
    sqlx::query(
        "SELECT t.symbol_name, t.parent_symbol, e.weight, COALESCE(e.metadata_json, '') AS meta
         FROM graph_edges e
         JOIN graph_nodes s ON s.node_id = e.source_node_id
         JOIN graph_nodes t ON t.node_id = e.target_node_id
         WHERE e.tenant_id = ?1 AND e.edge_type = 'CALLS' AND s.symbol_name = ?2
           AND t.file_path <> ''
         ORDER BY t.parent_symbol",
    )
    .bind(T)
    .bind(caller)
    .fetch_all(guard.pool())
    .await
    .unwrap()
    .iter()
    .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3)))
    .collect()
}

const WRITES: &str = "lib/cash_flow/infrastructure/firestore_finance_writes.dart";

/// The Finance file's shape: two classes, each with its own `set` forwarding
/// to a Firestore field.
async fn ingest_writes(store: &SharedGraphStore<SqliteGraphStore>) {
    let batch_set = "void set<T>(DocumentReference<T> document, T data) {\n    \
                     _batch.set(document, data);\n  }";
    let tx_set = "Transaction set<T>(DocumentReference<T> ref, T data) {\n    \
                  _transaction.set(ref, data);\n    return this;\n  }";
    let chunks = vec![
        class(
            "FirestoreFinanceBatch",
            &format!(
                "class FirestoreFinanceBatch implements WriteBatch {{\n  final WriteBatch _batch;\n  {batch_set}\n}}"
            ),
            (1, 7),
            WRITES,
        ),
        method("FirestoreFinanceBatch", "set", batch_set, (3, 5), WRITES, &["set"]),
        class(
            "FirestoreFinanceTransaction",
            &format!(
                "class FirestoreFinanceTransaction implements Transaction {{\n  final Transaction _transaction;\n  {tx_set}\n}}"
            ),
            (9, 16),
            WRITES,
        ),
        method("FirestoreFinanceTransaction", "set", tx_set, (11, 15), WRITES, &["set"]),
    ];
    ingest(store, WRITES, "w1", &chunks).await;
    // A homonym far away: by name alone the callers below are ambiguous.
    let auth = "packages/google_sign_in_dartio/lib/storage.dart";
    ingest(
        store,
        auth,
        "g1",
        &[
            class("TokenStorage", "class TokenStorage {}", (1, 9), auth),
            method(
                "TokenStorage",
                "set",
                "void set(String k) {}",
                (2, 4),
                auth,
                &[],
            ),
        ],
    )
    .await;
}

#[tokio::test]
async fn homonymous_methods_of_two_classes_in_one_file_stay_two_nodes() {
    let store = store().await;
    ingest_writes(&store).await;
    let guard = store.read().await;
    let rows: Vec<(String, Option<String>, i64)> = sqlx::query_as(
        "SELECT node_id, parent_symbol, start_line FROM graph_nodes
         WHERE tenant_id = ?1 AND file_path = ?2 AND symbol_name = 'set' ORDER BY start_line",
    )
    .bind(T)
    .bind(WRITES)
    .fetch_all(guard.pool())
    .await
    .unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_ne!(rows[0].0, rows[1].0);
    assert_eq!(rows[0].1.as_deref(), Some("FirestoreFinanceBatch"));
    assert_eq!(rows[0].2, 3);
    assert_eq!(rows[1].1.as_deref(), Some("FirestoreFinanceTransaction"));
}

/// Calls through an instance variable bind to that variable's class, from a
/// test as from production code; a Firestore field's `set` binds to nothing
/// of ours; and the test then covers the method.
#[tokio::test]
async fn a_call_through_an_instance_variable_binds_to_its_class() {
    let store = store().await;
    ingest_writes(&store).await;
    let body = "() async {\n  final batch = FirestoreFinanceBatch(firestore: db);\n  \
                batch.set(db.doc('a'), {'price': 10});\n}";
    let test_file = "test/features/cash_flow/finance_revision_test.dart";
    ingest(
        &store,
        test_file,
        "t1",
        &[function(
            "main",
            body,
            test_file,
            &["FirestoreFinanceBatch", "set"],
        )],
    )
    .await;
    let repo = "lib/cash_flow/infrastructure/firestore_cash_flow_repository.dart";
    ingest(
        &store,
        repo,
        "r1",
        &[function(
            "transferItem",
            body,
            repo,
            &["FirestoreFinanceBatch", "set"],
        )],
    )
    .await;
    store
        .resolve_stub_edges(T, &GenerationBranches::unknown())
        .await
        .unwrap();

    for caller in ["main", "transferItem"] {
        let calls: Vec<_> = calls_of(&store, caller)
            .await
            .into_iter()
            .filter(|c| c.0 == "set")
            .collect();
        assert_eq!(calls.len(), 1, "{caller}: {calls:?}");
        assert_eq!(
            calls[0].1.as_deref(),
            Some("FirestoreFinanceBatch"),
            "{caller}"
        );
        assert!((calls[0].2 - 0.97).abs() < 1e-9, "{caller}: {calls:?}");
        assert!(calls[0].3.contains("\"receiver\""), "{caller}: {calls:?}");
    }
    // `_batch` is a Firestore WriteBatch: not this tenant's code.
    assert!(
        calls_of(&store, "set").await.is_empty(),
        "a library receiver must not bind to our same-named methods"
    );

    let guard = store.read().await;
    let report = detect_test_gaps(guard.pool(), T, &GraphScope::all(), None, 50)
        .await
        .unwrap();
    assert!(
        !report.gaps.iter().any(|g| g.symbol_name == "set"
            && g.file_path == WRITES
            && g.production_dependents > 0),
        "the test covers FirestoreFinanceBatch.set: {:?}",
        report.gaps
    );
}

/// A variable typed by a class binds to that class's implementation, not to
/// the interface's declaration nor a sibling's; the constructor wins over the
/// declared interface type; and an untyped receiver keeps the 1/N ambiguous
/// fan-out (with its provenance) rather than guessing.
#[tokio::test]
async fn interface_implementations_resolve_through_the_constructed_type() {
    let store = store().await;
    let shapes = "lib/shapes.dart";
    ingest(
        &store,
        shapes,
        "s1",
        &[
            class(
                "Shape",
                "abstract class Shape {\n  double area();\n}",
                (1, 3),
                shapes,
            ),
            method("Shape", "area", "double area();", (2, 2), shapes, &[]),
            class("Circle", "class Circle implements Shape {}", (5, 8), shapes),
            method(
                "Circle",
                "area",
                "double area() => 3.14;",
                (6, 6),
                shapes,
                &[],
            ),
            class(
                "Square",
                "class Square implements Shape {}",
                (10, 13),
                shapes,
            ),
            method(
                "Square",
                "area",
                "double area() => 4;",
                (11, 11),
                shapes,
                &[],
            ),
        ],
    )
    .await;
    let report = "lib/report/report.dart";
    let typed =
        "void typed() {\n  final c = Circle();\n  c.area();\n  Shape s = Square();\n  s.area();\n}";
    let untyped = "void untyped(dynamic shape) {\n  shape.area();\n}";
    ingest(
        &store,
        report,
        "p1",
        &[
            function("typed", typed, report, &["Circle", "Square", "area"]),
            function("untyped", untyped, report, &["area"]),
        ],
    )
    .await;
    store
        .resolve_stub_edges(T, &GenerationBranches::unknown())
        .await
        .unwrap();

    let typed_targets: Vec<Option<String>> = calls_of(&store, "typed")
        .await
        .into_iter()
        .filter(|c| c.0 == "area")
        .map(|c| c.1)
        .collect();
    assert_eq!(
        typed_targets,
        vec![Some("Circle".to_string()), Some("Square".to_string())]
    );

    let untyped: Vec<_> = calls_of(&store, "untyped")
        .await
        .into_iter()
        .filter(|c| c.0 == "area")
        .collect();
    assert_eq!(untyped.len(), 3, "{untyped:?}");
    assert!(untyped
        .iter()
        .all(|c| (c.2 - 1.0 / 3.0).abs() < 1e-9 && c.3.contains("\"ambiguous\"")));
}
