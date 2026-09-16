//! Tests for the per-record helpers on [`DaemonMetrics`] split out of
//! `metrics_core.rs` to keep that file under the project's 500-line ceiling.

use super::metrics_core::DaemonMetrics;
use prometheus::core::Collector;
use std::time::Duration;

#[test]
fn watcher_events_increment_per_label() {
    let m = DaemonMetrics::new();
    m.record_watcher_event("create");
    m.record_watcher_event("create");
    m.record_watcher_event("modify");
    m.record_watcher_event("delete");
    m.record_watcher_event("rename");
    assert_eq!(
        m.watcher_events_total.with_label_values(&["create"]).get(),
        2
    );
    assert_eq!(
        m.watcher_events_total.with_label_values(&["modify"]).get(),
        1
    );
    assert_eq!(
        m.watcher_events_total.with_label_values(&["delete"]).get(),
        1
    );
    assert_eq!(
        m.watcher_events_total.with_label_values(&["rename"]).get(),
        1
    );
}

#[test]
fn watcher_coalesced_increment_per_reason() {
    let m = DaemonMetrics::new();
    m.record_watcher_coalesced("debounce");
    m.record_watcher_coalesced("debounce");
    m.record_watcher_coalesced("duplicate");
    assert_eq!(
        m.watcher_coalesced_total
            .with_label_values(&["debounce"])
            .get(),
        2
    );
    assert_eq!(
        m.watcher_coalesced_total
            .with_label_values(&["duplicate"])
            .get(),
        1
    );
}

#[test]
fn grpc_records_count_and_duration() {
    let m = DaemonMetrics::new();
    m.record_grpc_call(
        "SystemService",
        "HealthCheck",
        true,
        Duration::from_millis(5),
    );
    m.record_grpc_call(
        "SystemService",
        "HealthCheck",
        true,
        Duration::from_millis(12),
    );
    m.record_grpc_call(
        "SystemService",
        "HealthCheck",
        false,
        Duration::from_millis(50),
    );
    m.record_grpc_call(
        "DocumentService",
        "Ingest",
        true,
        Duration::from_millis(300),
    );

    assert_eq!(
        m.grpc_requests_total
            .with_label_values(&["SystemService", "HealthCheck", "ok"])
            .get(),
        2
    );
    assert_eq!(
        m.grpc_requests_total
            .with_label_values(&["SystemService", "HealthCheck", "error"])
            .get(),
        1
    );
    assert_eq!(
        m.grpc_requests_total
            .with_label_values(&["DocumentService", "Ingest", "ok"])
            .get(),
        1
    );

    let hist = m
        .grpc_request_duration_seconds
        .with_label_values(&["SystemService", "HealthCheck"]);
    let metric = hist.collect();
    let sample_count = metric[0].get_metric()[0].get_histogram().get_sample_count();
    assert_eq!(sample_count, 3);
}

#[test]
fn encode_contains_extension_metric_names() {
    let m = DaemonMetrics::new();
    m.record_watcher_event("create");
    m.record_watcher_coalesced("debounce");
    m.record_grpc_call(
        "SystemService",
        "HealthCheck",
        true,
        Duration::from_millis(1),
    );
    let out = m.encode().expect("encode ok");
    assert!(out.contains("memexd_watcher_events_total"));
    assert!(out.contains("memexd_watcher_coalesced_total"));
    assert!(out.contains("memexd_grpc_requests_total"));
    assert!(out.contains("memexd_grpc_request_duration_seconds"));
}

#[test]
fn indexed_project_inventory_metrics_are_exported() {
    let m = DaemonMetrics::new();
    m.set_indexed_project_tracked_files(
        "watch-123",
        "proj-123",
        "C:/dev/project",
        true,
        true,
        false,
        false,
        false,
        true,
        "https://github.com/user/repo.git",
        42,
    );
    m.set_indexed_project_points("watch-123", 128);
    m.set_indexed_project_last_scan("watch-123", Some(1_717_000_000));
    m.set_indexed_project_last_activity("watch-123", Some(1_717_000_123));

    assert_eq!(
        m.indexed_project_tracked_files
            .with_label_values(&[
                "watch-123",
                "proj-123",
                "C:/dev/project",
                "true",
                "true",
                "false",
                "false",
                "false",
                "true",
                "https://github.com/user/repo.git",
            ])
            .get(),
        42
    );
    assert_eq!(
        m.indexed_project_points
            .with_label_values(&["watch-123"])
            .get(),
        128
    );
    assert_eq!(
        m.indexed_project_last_scan_seconds
            .with_label_values(&["watch-123"])
            .get(),
        1_717_000_000
    );
    assert_eq!(
        m.indexed_project_last_activity_seconds
            .with_label_values(&["watch-123"])
            .get(),
        1_717_000_123
    );

    let out = m.encode().expect("encode ok");
    assert!(out.contains("memexd_indexed_project_tracked_files"));
    assert!(out.contains("memexd_indexed_project_points"));
    assert!(out.contains("memexd_indexed_project_last_scan_seconds"));
    assert!(out.contains("memexd_indexed_project_last_activity_seconds"));
}

#[test]
fn embedding_records_duration_and_batch_size() {
    let m = DaemonMetrics::new();
    m.record_embedding("all-MiniLM-L6-v2", 32, Duration::from_millis(150));
    m.record_embedding("all-MiniLM-L6-v2", 8, Duration::from_millis(60));

    let dur = m
        .embedding_duration_seconds
        .with_label_values(&["all-MiniLM-L6-v2"]);
    let metric = dur.collect();
    let sample_count = metric[0].get_metric()[0].get_histogram().get_sample_count();
    assert_eq!(sample_count, 2);
}

#[test]
fn sqlite_records_by_op() {
    let m = DaemonMetrics::new();
    m.record_sqlite("read", Duration::from_millis(2));
    m.record_sqlite("write", Duration::from_millis(5));
    m.record_sqlite("transaction", Duration::from_millis(10));
    for op in ["read", "write", "transaction"] {
        let h = m.sqlite_query_duration_seconds.with_label_values(&[op]);
        let metric = h.collect();
        assert_eq!(
            metric[0].get_metric()[0].get_histogram().get_sample_count(),
            1,
            "op {op} should have 1 sample"
        );
    }
}

#[test]
fn qdrant_records_duration_and_errors() {
    let m = DaemonMetrics::new();
    m.record_qdrant("upsert", Duration::from_millis(20), None);
    m.record_qdrant("upsert", Duration::from_millis(25), Some("timeout"));
    m.record_qdrant("search", Duration::from_millis(15), None);

    let upsert = m
        .qdrant_request_duration_seconds
        .with_label_values(&["upsert"]);
    assert_eq!(
        upsert.collect()[0].get_metric()[0]
            .get_histogram()
            .get_sample_count(),
        2
    );
    assert_eq!(
        m.qdrant_request_errors_total
            .with_label_values(&["upsert", "timeout"])
            .get(),
        1
    );
    assert_eq!(
        m.qdrant_request_errors_total
            .with_label_values(&["search", "timeout"])
            .get(),
        0
    );
}

#[test]
fn encode_contains_dependency_metric_names() {
    let m = DaemonMetrics::new();
    m.record_embedding("all-MiniLM-L6-v2", 1, Duration::from_millis(1));
    m.record_sqlite("read", Duration::from_millis(1));
    m.record_qdrant("search", Duration::from_millis(1), None);
    let out = m.encode().expect("encode ok");
    assert!(out.contains("memexd_embedding_duration_seconds"));
    assert!(out.contains("memexd_embedding_batch_size"));
    assert!(out.contains("memexd_sqlite_query_duration_seconds"));
    assert!(out.contains("memexd_qdrant_request_duration_seconds"));
}

#[test]
fn lsp_server_state_and_snapshot_gauges() {
    let m = DaemonMetrics::new();
    m.set_lsp_server_state("rust", true);
    m.set_lsp_server_state("dart", true);
    m.set_lsp_server_state("go", false);
    m.set_lsp_snapshot(7, 2);

    assert_eq!(m.lsp_server_state.with_label_values(&["rust"]).get(), 1);
    assert_eq!(m.lsp_server_state.with_label_values(&["dart"]).get(), 1);
    assert_eq!(m.lsp_server_state.with_label_values(&["go"]).get(), 0);
    assert_eq!(m.lsp_available_languages.get(), 7);
    assert_eq!(m.lsp_active_servers.get(), 2);

    // A language flipping running→stopped must update in place, not append.
    m.set_lsp_server_state("dart", false);
    assert_eq!(m.lsp_server_state.with_label_values(&["dart"]).get(), 0);
}

#[test]
fn encode_contains_lsp_metric_names() {
    let m = DaemonMetrics::new();
    m.set_lsp_server_state("rust", true);
    m.set_lsp_snapshot(8, 7);
    let out = m.encode().expect("encode ok");
    assert!(out.contains("memexd_lsp_server_state"));
    assert!(out.contains("memexd_lsp_available_languages"));
    assert!(out.contains("memexd_lsp_active_servers"));
    // Label wiring sanity: the language label is present in the exposition.
    assert!(out.contains("language=\"rust\""));
}

/// The divergence that hid the tsserver leak: the registry said 5 servers
/// while 60 processes sat under the daemon, and no series could show it.
/// These two must be exposed SIDE BY SIDE with names a panel can subtract.
#[test]
fn lsp_os_processes_exposed_beside_the_registry_count() {
    let m = DaemonMetrics::new();
    m.set_lsp_snapshot(8, 5);
    m.set_lsp_os_processes(10, 50, 4_000_000_000);

    assert_eq!(m.lsp_active_servers.get(), 5);
    assert_eq!(m.lsp_os_processes.with_label_values(&["child"]).get(), 10);
    assert_eq!(
        m.lsp_os_processes.with_label_values(&["descendant"]).get(),
        50
    );
    assert_eq!(m.lsp_os_processes_rss_bytes.get(), 4_000_000_000);

    let out = m.encode().expect("encode ok");
    assert!(out.contains("memexd_lsp_os_processes{depth=\"child\"} 10"));
    assert!(out.contains("memexd_lsp_os_processes{depth=\"descendant\"} 50"));
    assert!(out.contains("memexd_lsp_os_processes_rss_bytes 4000000000"));
}

/// The failover switch used to be a log line. Both labels must be written on
/// every transition so a dashboard reads the state, never infers it from an
/// absent series.
#[test]
fn embedding_endpoint_gauge_writes_both_labels_on_every_transition() {
    let m = DaemonMetrics::new();

    m.set_embedding_endpoint(true);
    assert_eq!(
        m.embedding_endpoint_active
            .with_label_values(&["primary"])
            .get(),
        1
    );
    assert_eq!(
        m.embedding_endpoint_active
            .with_label_values(&["fallback"])
            .get(),
        0
    );

    m.set_embedding_endpoint(false);
    m.embedding_failover();
    assert_eq!(
        m.embedding_endpoint_active
            .with_label_values(&["primary"])
            .get(),
        0
    );
    assert_eq!(
        m.embedding_endpoint_active
            .with_label_values(&["fallback"])
            .get(),
        1
    );
    assert_eq!(m.embedding_failover_total.get(), 1);

    let out = m.encode().expect("encode ok");
    assert!(out.contains("memexd_embedding_endpoint_active{endpoint=\"fallback\"} 1"));
    assert!(out.contains("memexd_embedding_failover_total 1"));
}
