//! Tests for tracking-related WriteActor commands:
//! LogSearchEvent.

use crate::write_actor::commands::*;

use super::common::setup_test_db;

// ── LogSearchEvent tests ─────────────────────────────────────────────

#[tokio::test]
async fn log_search_event_inserts_record() {
    let (pool, handle) = setup_test_db().await;

    handle
        .log_search_event(LogSearchEventData {
            id: "evt-1".into(),
            session_id: Some("sess-1".into()),
            project_id: Some("proj-1".into()),
            actor: "claude".into(),
            // Real CHECK-passing values (the fixture now carries the
            // production constraints): tool 'search' / op 'semantic' were
            // never valid on the live table.
            tool: "mcp_qdrant".into(),
            op: "search".into(),
            query_text: Some("find auth module".into()),
            filters: None,
            top_k: Some(10),
            result_count: Some(5),
            latency_ms: Some(42),
            top_result_refs: None,
            outcome: Some("success".into()),
            parent_event_id: None,
        })
        .await
        .unwrap();

    let count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM search_events WHERE id = 'evt-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1);

    let actor =
        sqlx::query_scalar::<_, String>("SELECT actor FROM search_events WHERE id = 'evt-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(actor, "claude");
}

/// Regression for the v47/v48 silent loss: every dispatcher-instrumented op
/// must INSERT — pre-widening the CHECK rejected new op names and the
/// fire-and-forget write path swallowed the error, so the events vanished
/// with zero trace. Runs against the production DDL (see common.rs fixture).
#[tokio::test]
async fn log_search_event_accepts_rules_and_scratchpad_ops() {
    let (pool, handle) = setup_test_db().await;

    for op in [
        "rules",
        "scratchpad",
        "graph",
        "store",
        "embedding",
        "workspace_index",
        "search_eval",
    ] {
        handle
            .log_search_event(LogSearchEventData {
                id: format!("evt-{op}"),
                session_id: Some("sess-1".into()),
                project_id: None,
                actor: "claude".into(),
                tool: "mcp_qdrant".into(),
                op: op.into(),
                query_text: Some("list".into()),
                filters: None,
                top_k: None,
                result_count: None,
                latency_ms: None,
                top_result_refs: None,
                outcome: None,
                parent_event_id: None,
            })
            .await
            .unwrap_or_else(|e| panic!("op='{op}' must pass the CHECK: {e}"));
    }

    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM search_events WHERE id LIKE 'evt-%'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 7);
}

/// A logged search event refreshes `last_activity_at` for the target project
/// (so cross-project reads surface in the admin UI) but must NOT flip
/// `is_active` — activity is not an active session (see exec_log_search_event).
#[tokio::test]
async fn log_search_event_touches_last_activity_but_not_is_active() {
    let (pool, handle) = setup_test_db().await;

    // Seed an INACTIVE project whose last_activity_at is frozen in the past.
    sqlx::query(
        "INSERT INTO watch_folders \
         (watch_id, path, collection, tenant_id, is_active, last_activity_at, created_at, updated_at) \
         VALUES ('w-doc', '/repo/doc', 'projects', 'doc-tenant', 0, \
                 '2020-01-01T00:00:00.000Z', '2020-01-01T00:00:00.000Z', '2020-01-01T00:00:00.000Z')",
    )
    .execute(&pool)
    .await
    .unwrap();

    handle
        .log_search_event(LogSearchEventData {
            id: "evt-doc".into(),
            session_id: None,
            project_id: Some("doc-tenant".into()),
            actor: "claude".into(),
            tool: "mcp_qdrant".into(),
            op: "search".into(),
            query_text: Some("cross-project query".into()),
            filters: None,
            top_k: Some(10),
            result_count: Some(3),
            latency_ms: Some(12),
            top_result_refs: None,
            outcome: Some("success".into()),
            parent_event_id: None,
        })
        .await
        .unwrap();

    let (is_active, last_activity) = sqlx::query_as::<_, (i64, String)>(
        "SELECT is_active, last_activity_at FROM watch_folders WHERE tenant_id = 'doc-tenant'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    // last_activity_at moved forward off the frozen 2020 value…
    assert!(
        last_activity.as_str() > "2020-01-01T00:00:00.000Z",
        "expected last_activity_at to be refreshed, got {last_activity}"
    );
    // …but is_active stayed 0 (no synthetic session, no LSP, no reprioritize).
    assert_eq!(is_active, 0, "read-path touch must not activate the project");
}

/// The touch is scoped by tenant: a search event for one project must not
/// bump last_activity_at of a different project.
#[tokio::test]
async fn log_search_event_touch_is_scoped_to_target_tenant() {
    let (pool, handle) = setup_test_db().await;

    sqlx::query(
        "INSERT INTO watch_folders \
         (watch_id, path, collection, tenant_id, is_active, last_activity_at, created_at, updated_at) \
         VALUES ('w-other', '/repo/other', 'projects', 'other-tenant', 0, \
                 '2020-01-01T00:00:00.000Z', '2020-01-01T00:00:00.000Z', '2020-01-01T00:00:00.000Z')",
    )
    .execute(&pool)
    .await
    .unwrap();

    handle
        .log_search_event(LogSearchEventData {
            id: "evt-scope".into(),
            session_id: None,
            project_id: Some("doc-tenant".into()),
            actor: "claude".into(),
            tool: "grep".into(),
            op: "grep".into(),
            query_text: None,
            filters: None,
            top_k: None,
            result_count: Some(0),
            latency_ms: Some(3),
            top_result_refs: None,
            outcome: Some("success".into()),
            parent_event_id: None,
        })
        .await
        .unwrap();

    let untouched = sqlx::query_scalar::<_, String>(
        "SELECT last_activity_at FROM watch_folders WHERE tenant_id = 'other-tenant'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        untouched, "2020-01-01T00:00:00.000Z",
        "a search event for another tenant must not touch this project's activity"
    );
}
