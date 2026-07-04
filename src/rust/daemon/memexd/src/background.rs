//! Phase 3: Background periodic task spawns.
//!
//! Spawns long-running tokio tasks for periodic maintenance: pause-state polling,
//! metrics collection, processing-timings cleanup, log pruning, inactivity timeout,
//! remote URL monitoring, git state change detection, uptime tracking, and the
//! Prometheus metrics endpoint.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::DateTime;
use sqlx::{Row, SqlitePool};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use workspace_qdrant_core::config::PrometheusExportConfig;
use workspace_qdrant_core::lsp::ServerStatus;
use workspace_qdrant_core::search_db::SearchDbManager;
use workspace_qdrant_core::{
    check_git_state_changes, check_remote_url_changes, metrics_history, poll_pause_state,
    processing_timings, Language, LanguageServerManager, MetricsServer, PriorityManager,
    SessionMonitor, SessionMonitorConfig, METRICS,
};

/// Handles for all background tasks so the orchestrator can abort them on shutdown.
pub struct BackgroundHandles {
    pub uptime_handle: JoinHandle<()>,
    pub pause_poll_handle: JoinHandle<()>,
    pub metrics_collect_handle: JoinHandle<()>,
    pub metrics_maint_handle: JoinHandle<()>,
    pub grpc_handle: Option<JoinHandle<()>>,
    pub metrics_handle: Option<JoinHandle<()>>,
    /// Handle for the LSP Prometheus metrics poller.  `None` when LSP is
    /// disabled or when the manager could not be initialized.
    pub lsp_metrics_handle: Option<JoinHandle<()>>,
}

/// Start the Prometheus metrics endpoint when `config.enabled` is true.
pub fn start_metrics_server(config: &PrometheusExportConfig) -> Option<JoinHandle<()>> {
    if !config.enabled {
        info!(
            "Prometheus metrics endpoint disabled (set telemetry.prometheus.enabled=true \
             or pass --metrics-port to enable)"
        );
        return None;
    }
    let mut metrics_server = match MetricsServer::from_config(config) {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to build metrics server from config: {}", e);
            return None;
        }
    };
    info!(
        "Starting Prometheus metrics endpoint on {}:{}",
        config.bind, config.port
    );
    let handle = tokio::spawn(async move {
        if let Err(e) = metrics_server.start().await {
            error!("Metrics server error: {}", e);
        }
    });
    Some(handle)
}

/// Build an effective PrometheusExportConfig by merging the CLI `--metrics-port`
/// override (if provided) on top of the config-file values. The CLI flag flips
/// `enabled=true` when set, preserving the documented behavior of the flag.
pub fn resolve_prometheus_config(
    base: PrometheusExportConfig,
    cli_override_port: Option<u16>,
) -> PrometheusExportConfig {
    match cli_override_port {
        Some(port) => PrometheusExportConfig {
            enabled: true,
            port,
            bind: base.bind,
        },
        None => base,
    }
}

/// Start uptime tracking (updates the global METRICS gauge every second).
pub fn start_uptime_tracker() -> JoinHandle<()> {
    let start_time = std::time::Instant::now();
    tokio::spawn(async move {
        let mut last_cpu = read_proc_cpu_seconds();
        let mut last_at = std::time::Instant::now();
        loop {
            METRICS.set_uptime(start_time.elapsed().as_secs_f64());

            // Process RSS + CPU% sampled from /proc/self. Each container reads
            // its own process, so this works on WSL2/Docker-Desktop where an
            // external cAdvisor cannot see the workload container cgroups.
            if let Some(rss) = read_process_rss_bytes() {
                METRICS.set_process_resident_memory_bytes(rss as i64);
            }
            let now_cpu = read_proc_cpu_seconds();
            let now_at = std::time::Instant::now();
            let dt = now_at.duration_since(last_at).as_secs_f64();
            if dt > 0.0 {
                let pct = ((now_cpu - last_cpu) / dt) * 100.0;
                METRICS.set_process_cpu_percent(pct.max(0.0));
            }
            last_cpu = now_cpu;
            last_at = now_at;

            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        }
    })
}

/// Resident set size of this process in bytes, from `/proc/self/statm`.
fn read_process_rss_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    // 4 KiB pages on the linux/amd64 runtime image.
    parse_rss_pages(&statm).map(|pages| pages * 4096)
}

/// Parse the resident-pages field (field 2) of `/proc/self/statm`.
fn parse_rss_pages(statm: &str) -> Option<u64> {
    statm.split_whitespace().nth(1)?.parse().ok()
}

/// Cumulative CPU seconds (user + system) for this process, from
/// `/proc/self/stat`. Returns 0 if unreadable.
fn read_proc_cpu_seconds() -> f64 {
    std::fs::read_to_string("/proc/self/stat")
        .map(|s| parse_cpu_seconds(&s))
        .unwrap_or(0.0)
}

/// Parse cumulative CPU seconds from a `/proc/self/stat` line: fields 14 utime +
/// 15 stime (in clock ticks, CLK_TCK=100). The `comm` field may contain spaces
/// and parens, so fields are taken after the final ')'.
fn parse_cpu_seconds(stat: &str) -> f64 {
    let after = match stat.rfind(')') {
        Some(i) => &stat[i + 1..],
        None => return 0.0,
    };
    let fields: Vec<&str> = after.split_whitespace().collect();
    // After ')', index 0 = state (field 3); utime → index 11, stime → index 12.
    if fields.len() > 12 {
        let utime: f64 = fields[11].parse().unwrap_or(0.0);
        let stime: f64 = fields[12].parse().unwrap_or(0.0);
        (utime + stime) / 100.0
    } else {
        0.0
    }
}

/// Start periodic DB polling for CLI-driven pause state changes (Task 543.10).
pub fn start_pause_polling(
    pool: SqlitePool,
    pause_flag: Arc<std::sync::atomic::AtomicBool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            match poll_pause_state(&pool, &pause_flag).await {
                Ok(true) => {
                    let is_paused = pause_flag.load(std::sync::atomic::Ordering::SeqCst);
                    info!(
                        "Pause state changed via DB: watchers are now {}",
                        if is_paused { "PAUSED" } else { "ACTIVE" }
                    );
                }
                Ok(false) => {} // No change
                Err(e) => {
                    warn!("Failed to poll pause state: {}", e);
                }
            }
        }
    })
}

/// Start periodic metrics history collection (Task 544.6).
pub fn start_metrics_collection(pool: SqlitePool) -> JoinHandle<()> {
    let handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            match metrics_history::write_snapshot(&pool).await {
                Ok(count) => {
                    if count > 0 {
                        debug!("Collected {} metrics to history", count);
                    }
                }
                Err(e) => {
                    warn!("Failed to collect metrics history: {}", e);
                }
            }
        }
    });
    info!("Metrics history collection started (60s interval)");
    handle
}

/// Periodically checkpoint the WAL to keep it bounded (#5 of the SQLite
/// write-contention work).
///
/// SQLite's automatic `wal_autocheckpoint` runs a PASSIVE checkpoint inline, but
/// PASSIVE can be starved by long-lived readers (the queue-depth / inventory
/// exporters, git/pause monitors), letting the WAL grow unbounded under
/// sustained write load — observed at ~730 MB during a reembed. A large WAL both
/// wastes disk and keeps read snapshots stale, which feeds SQLITE_BUSY_SNAPSHOT.
/// This loop runs a PASSIVE checkpoint every tick and forces a TRUNCATE once the
/// WAL crosses a frame threshold, so it stays bounded without churning readers
/// on every tick.
pub fn start_wal_checkpoint_loop(pool: SqlitePool) -> JoinHandle<()> {
    use workspace_qdrant_core::queue_config::{checkpoint_wal, CheckpointMode};

    // A WAL frame is one DB page (~4 KiB). Truncate once the log exceeds ~20k
    // frames (~80 MiB) so it stays bounded while TRUNCATE (which waits for
    // readers) fires rarely rather than on every tick.
    const TRUNCATE_THRESHOLD_FRAMES: i32 = 20_000;

    let handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            match checkpoint_wal(&pool, CheckpointMode::Passive).await {
                Ok(result) => {
                    if result.log_size > TRUNCATE_THRESHOLD_FRAMES {
                        debug!(
                            "WAL large ({} frames; {} checkpointed passively), forcing TRUNCATE",
                            result.log_size, result.checkpointed
                        );
                        match checkpoint_wal(&pool, CheckpointMode::Truncate).await {
                            Ok(t) => debug!(
                                "WAL TRUNCATE: busy={}, log_size={}, checkpointed={}",
                                t.busy, t.log_size, t.checkpointed
                            ),
                            Err(e) => warn!("WAL TRUNCATE checkpoint failed: {}", e),
                        }
                    }
                }
                Err(e) => warn!("WAL passive checkpoint failed: {}", e),
            }
        }
    });
    info!(
        "WAL checkpoint loop started (30s PASSIVE, TRUNCATE above {} frames)",
        TRUNCATE_THRESHOLD_FRAMES
    );
    handle
}

/// Periodically refresh unified queue depth gauges (issue-64 Task 4).
///
/// Queries the queue for pending/in_progress/failed counts grouped by
/// `(item_type, status)` and pushes them into the Prometheus gauge so
/// `/metrics` reflects real queue size without instrumenting every DB mutation.
pub fn start_queue_depth_exporter(pool: SqlitePool) -> JoinHandle<()> {
    use workspace_qdrant_core::queue_operations::QueueManager;

    let handle = tokio::spawn(async move {
        let manager = QueueManager::new(pool.clone());
        // 60s (was 10s): each tick runs 3 full-table aggregates on unified_queue
        // (by type/status, by tenant/status, stats) + a per-tenant rate query.
        // During a heavy reconcile these are the dominant SQLite-contention
        // source (28s GROUP BYs starving the connection pool); the gauges are
        // observability-only, so a coarser cadence trades nothing important.
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
        let known_pairs: std::sync::Arc<
            tokio::sync::Mutex<std::collections::HashSet<(String, String)>>,
        > = std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new()));
        let known_tenant_pairs: std::sync::Arc<
            tokio::sync::Mutex<std::collections::HashSet<(String, String)>>,
        > = std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new()));
        loop {
            interval.tick().await;
            match manager.get_unified_queue_depth_by_type_status().await {
                Ok(rows) => {
                    let mut seen = std::collections::HashSet::new();
                    for (item_type, status, count) in rows {
                        METRICS.set_unified_queue_depth(&item_type, &status, count);
                        seen.insert((item_type, status));
                    }
                    // Zero-out any (item_type, status) pairs we've seen before
                    // but aren't present now, so gauges don't get stuck.
                    let mut guard = known_pairs.lock().await;
                    for pair in guard.iter() {
                        if !seen.contains(pair) {
                            METRICS.set_unified_queue_depth(&pair.0, &pair.1, 0);
                        }
                    }
                    guard.extend(seen);
                }
                Err(e) => debug!("queue depth refresh failed: {}", e),
            }
            // Per-tenant indexing-progress gauge (Grafana / MCP indexing block).
            match manager.get_unified_queue_depth_by_tenant_status().await {
                Ok(rows) => {
                    let mut seen = std::collections::HashSet::new();
                    // Accumulate (pending + in_progress) per tenant for ETA.
                    let mut in_flight_by_tenant: std::collections::HashMap<String, i64> =
                        std::collections::HashMap::new();
                    for (tenant_id, status, count) in rows {
                        METRICS.set_unified_queue_depth_by_tenant(&tenant_id, &status, count);
                        if status == "pending" || status == "in_progress" {
                            *in_flight_by_tenant.entry(tenant_id.clone()).or_insert(0) += count;
                        }
                        seen.insert((tenant_id, status));
                    }
                    let mut guard = known_tenant_pairs.lock().await;
                    for pair in guard.iter() {
                        if !seen.contains(pair) {
                            METRICS.set_unified_queue_depth_by_tenant(&pair.0, &pair.1, 0);
                        }
                    }
                    guard.extend(seen);
                    drop(guard);

                    // Per-tenant ETA gauge: query the rate from
                    // tracked_files for every tenant we just observed.
                    // Tenants that drained (in-flight == 0) get ETA = 0
                    // so the Grafana panel shows "done" instead of stale.
                    use workspace_qdrant_core::indexing_progress::{
                        estimate_eta_seconds, eta_for_gauge, rate_files_per_sec,
                    };
                    for (tenant_id, in_flight) in in_flight_by_tenant {
                        let rate = rate_files_per_sec(&pool, &tenant_id).await;
                        // Split in_flight back into a (pending, in_progress)
                        // pair only for the API shape — the sum is what
                        // matters for ETA, so pass it all as `pending`.
                        let eta = estimate_eta_seconds(in_flight, 0, rate);
                        METRICS.set_indexing_eta_seconds(&tenant_id, eta_for_gauge(eta));
                    }
                }
                Err(e) => debug!("per-tenant queue depth refresh failed: {}", e),
            }
            match manager.get_unified_queue_stats().await {
                Ok(stats) => METRICS.set_unified_queue_stale_items(stats.stale_leases),
                Err(e) => debug!("queue stats refresh failed: {}", e),
            }
        }
    });
    info!("Queue depth exporter started (60s interval)");
    handle
}

/// Periodically refresh the indexed-project inventory metric from SQLite.
///
/// Exports one Prometheus series per indexed project row so Grafana can render
/// a live table of project metadata without querying SQLite directly.
pub fn start_indexed_project_inventory_exporter(pool: SqlitePool) -> JoinHandle<()> {
    let handle = tokio::spawn(async move {
        // 300s (was 30s): this is an expensive watch_folders ⨝ tracked_files
        // GROUP BY + ORDER BY on computed columns; at 30s it collided with the
        // reconcile upsert window. Inventory changes slowly, so 5min is ample.
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(300));
        loop {
            interval.tick().await;
            let query = r#"
                SELECT
                    wf.watch_id,
                    wf.tenant_id,
                    wf.path,
                    wf.enabled,
                    wf.is_active,
                    wf.is_paused,
                    wf.is_archived,
                    wf.is_worktree,
                    wf.is_git_tracked,
                    wf.git_remote_url,
                    COUNT(tf.file_id) AS document_count,
                    COALESCE(SUM(tf.chunk_count), 0) AS point_count,
                    wf.last_scan,
                    wf.last_activity_at
                FROM watch_folders wf
                LEFT JOIN tracked_files tf
                    ON tf.watch_folder_id = wf.watch_id
                WHERE wf.collection = 'projects'
                GROUP BY
                    wf.watch_id,
                    wf.tenant_id,
                    wf.path,
                    wf.enabled,
                    wf.is_active,
                    wf.is_paused,
                    wf.is_archived,
                    wf.is_worktree,
                    wf.is_git_tracked,
                    wf.git_remote_url
                ORDER BY document_count DESC, point_count DESC, wf.tenant_id ASC, wf.path ASC
            "#;

            match sqlx::query(query).fetch_all(&pool).await {
                Ok(rows) => {
                    METRICS.indexed_project_tracked_files.reset();
                    METRICS.indexed_project_points.reset();
                    METRICS.indexed_project_last_scan_seconds.reset();
                    METRICS.indexed_project_last_activity_seconds.reset();

                    for row in rows {
                        let watch_id: String = row.get("watch_id");
                        let tenant_id: String = row.get("tenant_id");
                        let path: String = row.get("path");
                        let enabled: i32 = row.get("enabled");
                        let is_active: i32 = row.get("is_active");
                        let is_paused: i32 = row.get("is_paused");
                        let is_archived: i32 = row.get("is_archived");
                        let is_worktree: i32 = row.get("is_worktree");
                        let is_git_tracked: i32 = row.get("is_git_tracked");
                        let git_remote_url: Option<String> = row.get("git_remote_url");
                        let document_count: i64 = row.get("document_count");
                        let point_count: i64 = row.get("point_count");
                        let last_scan_epoch: Option<i64> =
                            row.get::<Option<String>, _>("last_scan").and_then(|value| {
                                DateTime::parse_from_rfc3339(&value)
                                    .ok()
                                    .map(|dt| dt.timestamp())
                            });
                        let last_activity_epoch: Option<i64> = row
                            .get::<Option<String>, _>("last_activity_at")
                            .and_then(|value| {
                                DateTime::parse_from_rfc3339(&value)
                                    .ok()
                                    .map(|dt| dt.timestamp())
                            });

                        METRICS.set_indexed_project_tracked_files(
                            &watch_id,
                            &tenant_id,
                            &path,
                            enabled != 0,
                            is_active != 0,
                            is_paused != 0,
                            is_archived != 0,
                            is_worktree != 0,
                            is_git_tracked != 0,
                            git_remote_url.as_deref().unwrap_or(""),
                            document_count,
                        );
                        METRICS.set_indexed_project_points(&watch_id, point_count);
                        METRICS.set_indexed_project_last_scan(&watch_id, last_scan_epoch);
                        METRICS.set_indexed_project_last_activity(&watch_id, last_activity_epoch);
                    }
                }
                Err(e) => debug!("indexed project inventory refresh failed: {}", e),
            }
        }
    });
    info!("Indexed project inventory exporter started (30s interval)");
    handle
}

/// Periodically refresh the chunking-coverage metric from SQLite.
///
/// Exports `tracked_files_by_chunking{language, chunking_method, treesitter_status}`
/// so Grafana can show, per language, how many files use tree-sitter semantic
/// chunking vs whole-file fallback (e.g. when a grammar failed to download).
pub fn start_chunking_coverage_exporter(pool: SqlitePool) -> JoinHandle<()> {
    let handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(300));
        loop {
            interval.tick().await;
            // Coverage is derived from the qdrant_chunks mirror (ground truth),
            // not solely from the mutable `treesitter_status` column. A file
            // counts as semantically chunked when the column says `done` OR the
            // mirror holds any non-NULL `chunk_type` (function/method/class/…;
            // text fallback stores NULL). This makes the dashboard immune to
            // bookkeeping drift — e.g. the branch-dedup path that historically
            // clobbered `treesitter_status` back to `none` while the shared
            // semantic points stayed intact.
            let query = r#"
                SELECT
                    COALESCE(NULLIF(tf.language, ''), 'unknown') AS language,
                    COALESCE(NULLIF(tf.chunking_method, ''), 'none') AS chunking_method,
                    CASE
                        WHEN tf.treesitter_status = 'done'
                             OR EXISTS (
                                 SELECT 1 FROM qdrant_chunks qc
                                  WHERE qc.file_id = tf.file_id
                                    AND qc.chunk_type IS NOT NULL
                             )
                        THEN 'done'
                        ELSE COALESCE(NULLIF(tf.treesitter_status, ''), 'none')
                    END AS treesitter_status,
                    COUNT(*) AS file_count
                FROM tracked_files tf
                GROUP BY language, chunking_method, treesitter_status
            "#;

            match sqlx::query(query).fetch_all(&pool).await {
                Ok(rows) => {
                    METRICS.tracked_files_by_chunking.reset();
                    for row in rows {
                        let language: String = row.get("language");
                        let chunking_method: String = row.get("chunking_method");
                        let treesitter_status: String = row.get("treesitter_status");
                        let file_count: i64 = row.get("file_count");
                        METRICS
                            .tracked_files_by_chunking
                            .with_label_values(&[&language, &chunking_method, &treesitter_status])
                            .set(file_count);
                    }
                }
                Err(e) => debug!("chunking coverage refresh failed: {}", e),
            }
        }
    });
    info!("Chunking coverage exporter started (300s interval)");
    handle
}

/// One-shot startup sweep: fetch grammars for languages that have stuck
/// (text-chunked) files but whose grammar was never downloaded.
///
/// The per-file grammar-download trigger lives on the full-ingest path and is
/// skipped by the branch-dedup fast-path. A language whose files only ever hit
/// dedup (content already indexed on another branch) therefore never fetches its
/// grammar and stays text-chunked forever (observed: `r`, whose 13 files were all
/// dedup clones). This sweep closes that gap: for each stuck language with a
/// downloadable, not-yet-cached grammar, it downloads the grammar and enqueues
/// File→Uplift for the stuck files so they get semantic chunks.
///
/// Churn-free and self-converging: only a NEWLY downloaded grammar triggers
/// uplifts. A language whose grammar is already cached is skipped (any stuck rows
/// there are the branch-dedup status-carry concern, recovered by that fix or a
/// reembed — not a missing grammar). Once a grammar is cached, later startups
/// skip it, so no repeated re-uplifting.
pub fn start_grammar_backfill(pool: SqlitePool) -> JoinHandle<()> {
    tokio::spawn(async move {
        use workspace_qdrant_core::config::GrammarConfig;
        use workspace_qdrant_core::queue_operations::QueueManager;
        use workspace_qdrant_core::strategies::capability_upgrade::trigger_capability_upgrade;
        use workspace_qdrant_core::tracked_files_schema::UpgradeReason;
        use workspace_qdrant_core::tree_sitter::GrammarManager;
        use workspace_qdrant_core::{canonical_language, get_static_language, is_language_supported};

        let langs = match sqlx::query(
            "SELECT DISTINCT language FROM tracked_files \
             WHERE treesitter_status IN ('none','failed','skipped') \
               AND language IS NOT NULL AND language <> ''",
        )
        .fetch_all(&pool)
        .await
        {
            Ok(rows) => rows,
            Err(e) => {
                debug!("grammar backfill: language query failed: {e}");
                return;
            }
        };
        if langs.is_empty() {
            return;
        }

        let mut config = GrammarConfig::default();
        config.auto_download = true;
        let mut manager = GrammarManager::new(config);
        let queue_manager = QueueManager::new(pool.clone());

        let mut downloaded = 0u32;
        for row in &langs {
            // `lang` is the tracked_files label — which may be an ALIAS (the
            // classifier tags `.sh` as "shell", not the canonical "bash").
            let lang: String = row.get("language");
            // Static grammars need no download; non-registry labels (unknown,
            // plain text) cannot be fetched at all.
            if get_static_language(&lang).is_some() || !is_language_supported(&lang) {
                continue;
            }
            // Grammar cache + loader key off the canonical id (`tree_sitter_<id>`
            // symbol); fetching by the raw alias "shell" would look up a
            // non-existent `tree_sitter_shell` and fail. Resolve it here — but
            // still uplift by the ORIGINAL label below (that is how the files are
            // tagged in tracked_files).
            let canonical = canonical_language(&lang).unwrap_or_else(|| lang.clone());
            // Already cached → not a "never-downloaded" case; skip to stay
            // churn-free (avoids re-uplifting on every startup).
            if manager.cache_paths().grammar_exists(&canonical) {
                continue;
            }
            match manager.get_grammar(&canonical).await {
                Ok(_) => {
                    downloaded += 1;
                    info!(
                        language = %lang,
                        "grammar backfill: downloaded never-fetched grammar — uplifting stuck files"
                    );
                    for tenant in distinct_tenants_with_stuck_language(&pool, &lang).await {
                        trigger_capability_upgrade(
                            &pool,
                            &queue_manager,
                            &tenant,
                            UpgradeReason::GrammarAvailable,
                            Some(&lang),
                        )
                        .await;
                    }
                }
                Err(e) => {
                    warn!(language = %lang, error = %e, "grammar backfill: grammar download failed");
                }
            }
        }
        if downloaded > 0 {
            info!("grammar backfill complete: {downloaded} never-fetched grammar(s) downloaded");
        }
    })
}

/// Tenants (via `watch_folders`) that own stuck files of `language`.
async fn distinct_tenants_with_stuck_language(pool: &SqlitePool, language: &str) -> Vec<String> {
    sqlx::query(
        "SELECT DISTINCT wf.tenant_id \
         FROM tracked_files tf JOIN watch_folders wf ON tf.watch_folder_id = wf.watch_id \
         WHERE tf.language = ?1 \
           AND tf.treesitter_status IN ('none','failed','skipped')",
    )
    .bind(language)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| r.get::<String, _>("tenant_id"))
    .collect()
}

/// Start hourly metrics maintenance: aggregation + retention (Task 544.11-14).
pub fn start_metrics_maintenance(pool: SqlitePool) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(3600));
        loop {
            interval.tick().await;
            if let Err(e) = metrics_history::run_maintenance_now(&pool).await {
                warn!("Metrics maintenance failed: {}", e);
            }
        }
    })
}

/// Start hourly processing timings cleanup (Task 42).
pub fn start_timings_cleanup(pool: SqlitePool) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(3600));
        loop {
            interval.tick().await;
            processing_timings::cleanup_old_timings(&pool, 30).await;
        }
    })
}

/// Start periodic log pruning (cli-qol task 12).
pub fn start_log_pruning(pool: SqlitePool) -> JoinHandle<()> {
    let log_prune_dir = workspace_qdrant_core::logging::get_canonical_log_dir();
    let handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(3600));
        loop {
            interval.tick().await;
            match workspace_qdrant_core::log_pruner::run_if_due(
                &pool,
                &log_prune_dir,
                36, // retention: 36 hours
                12, // check interval: 12 hours
            )
            .await
            {
                Ok(Some(result)) => {
                    if result.files_deleted > 0 {
                        info!(
                            files = result.files_deleted,
                            bytes = result.bytes_freed,
                            "Log pruning completed"
                        );
                    }
                }
                Ok(None) => {} // not due yet
                Err(e) => warn!("Log pruning failed: {}", e),
            }
        }
    });
    info!("Log pruning started (36h retention, 12h interval)");
    handle
}

/// Start periodic inactivity timeout check (Task 569).
pub fn start_inactivity_timeout(pool: SqlitePool) -> JoinHandle<()> {
    let inactivity_timeout_secs: i64 = std::env::var("WQM_INACTIVITY_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(43200);

    let handle = tokio::spawn(async move {
        let state_mgr = workspace_qdrant_core::daemon_state::DaemonStateManager::with_pool(pool);
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(300)); // every 5 min
        loop {
            interval.tick().await;
            match state_mgr
                .deactivate_inactive_projects(inactivity_timeout_secs)
                .await
            {
                Ok(0) => {} // no stale projects
                Ok(n) => info!("Inactivity timeout: deactivated {} project group(s)", n),
                Err(e) => warn!("Inactivity timeout check failed: {}", e),
            }
        }
    });
    info!(
        "Inactivity timeout polling started (5min interval, {}s timeout)",
        std::env::var("WQM_INACTIVITY_TIMEOUT_SECS").unwrap_or_else(|_| "43200".to_string())
    );
    handle
}

/// Sample MCP read-tool adoption/health from `search_events` into Prometheus
/// gauges, so agent usage and friction are visible in Grafana:
///   - `memexd_mcp_search_events_recent{op,actor}`  — events in the last 24h
///   - `memexd_mcp_search_empty_recent{op,actor}`   — empty (0-result) events
///   - `memexd_mcp_search_unresolved_recent{outcome,actor}` — resolution misses
/// The empty ratio (empty/events) and unresolved rate are the adoption signals:
/// semantic `search` should stay near-zero empty; a rising exact/grep empty ratio
/// means agents dead-end and fall back to native tools.
///
/// The `actor` label is load-bearing: the search-quality benchmark runs as
/// `actor='user'` and fires natural-language questions through exact/grep mode
/// (where ~0 results is expected by design), which would otherwise pin the
/// exact/grep empty ratio high and mask the real, agent-only (`actor='claude'`)
/// signal. Grouping by actor keeps both queryable and separable.
pub fn start_search_adoption_sampler(pool: SqlitePool) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
        loop {
            interval.tick().await;

            // Cutoff in the SAME ISO-Z format search_events.ts is stored in, so the
            // TEXT `ts >= cutoff` comparison is a correct lexical compare.
            let by_op = r#"
                SELECT op,
                       actor,
                       COUNT(*) AS total,
                       SUM(CASE WHEN result_count = 0 THEN 1 ELSE 0 END) AS empty
                FROM search_events
                WHERE ts >= strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-1 day')
                GROUP BY op, actor
            "#;
            match sqlx::query(by_op).fetch_all(&pool).await {
                Ok(rows) => {
                    // reset() drops stale label series so a no-longer-used (op,actor) disappears.
                    METRICS.mcp_search_events_recent.reset();
                    METRICS.mcp_search_empty_recent.reset();
                    for row in rows {
                        let op: String = row.try_get("op").unwrap_or_default();
                        let actor: String = row.try_get("actor").unwrap_or_default();
                        let total: i64 = row.try_get("total").unwrap_or(0);
                        let empty: i64 = row.try_get("empty").unwrap_or(0);
                        METRICS
                            .mcp_search_events_recent
                            .with_label_values(&[&op, &actor])
                            .set(total);
                        METRICS
                            .mcp_search_empty_recent
                            .with_label_values(&[&op, &actor])
                            .set(empty);
                    }
                }
                Err(e) => warn!("search-adoption sampler (by op) failed: {}", e),
            }

            let unresolved = r#"
                SELECT outcome, actor, COUNT(*) AS n
                FROM search_events
                WHERE ts >= strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-1 day')
                  AND outcome IN ('unresolved_tenant', 'unresolved_project', 'error')
                GROUP BY outcome, actor
            "#;
            match sqlx::query(unresolved).fetch_all(&pool).await {
                Ok(rows) => {
                    METRICS.mcp_search_unresolved_recent.reset();
                    for row in rows {
                        let outcome: String = row.try_get("outcome").unwrap_or_default();
                        let actor: String = row.try_get("actor").unwrap_or_default();
                        let n: i64 = row.try_get("n").unwrap_or(0);
                        METRICS
                            .mcp_search_unresolved_recent
                            .with_label_values(&[&outcome, &actor])
                            .set(n);
                    }
                }
                Err(e) => warn!("search-adoption sampler (unresolved) failed: {}", e),
            }
        }
    })
}

/// Start the session-liveness reaper (migration v42 model).
///
/// Deletes `project_sessions` rows whose `last_heartbeat_at` has gone stale and
/// re-projects `watch_folders.is_active = COUNT(live sessions)`, so a project
/// whose MCP session died WITHOUT a clean `unregister_session` — a crashed
/// client, or a fire-and-forget admin `RegisterProject` activate — is demoted
/// within ~one check interval instead of leaking `is_active` until the coarse
/// 12h inactivity timeout (`start_inactivity_timeout`, a separate
/// `last_activity_at`-based backstop this complements).
///
/// `heartbeat_timeout_secs` (default 90s) is ~3x the MCP heartbeat interval
/// (`HEARTBEAT_INTERVAL_MS`, 30s) so a single missed heartbeat never
/// false-reaps a live session. Override with `WQM_SESSION_HEARTBEAT_TIMEOUT_SECS`
/// / `WQM_SESSION_CHECK_INTERVAL_SECS`.
pub fn start_session_monitor(pool: SqlitePool) -> JoinHandle<()> {
    let heartbeat_timeout_secs: u64 = std::env::var("WQM_SESSION_HEARTBEAT_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(90);
    let check_interval_secs: u64 = std::env::var("WQM_SESSION_CHECK_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);

    tokio::spawn(async move {
        let monitor = SessionMonitor::new(
            PriorityManager::new(pool),
            SessionMonitorConfig {
                heartbeat_timeout_secs,
                check_interval_secs,
            },
        );
        if let Err(e) = monitor.start().await {
            error!("Failed to start session-liveness reaper: {}", e);
            return;
        }
        info!(
            "Session-liveness reaper started (heartbeat_timeout={}s, check_interval={}s)",
            heartbeat_timeout_secs, check_interval_secs
        );
        // `start()` detached the reaper loop; hold `monitor` (and thus its
        // cancellation token) for the daemon's lifetime so the loop keeps
        // running. This task ends only when the process exits.
        let _monitor = monitor;
        std::future::pending::<()>().await;
    })
}

/// Start periodic remote URL change detection (Task 584).
pub fn start_remote_url_monitor(pool: SqlitePool) -> JoinHandle<()> {
    let handle = tokio::spawn(async move {
        let queue_manager =
            workspace_qdrant_core::queue_operations::QueueManager::new(pool.clone());
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            match check_remote_url_changes(&pool, &queue_manager).await {
                Ok(result) => {
                    if result.changes_detected > 0 {
                        info!(
                            "Remote URL monitoring: {} change(s) detected, {} checked, {} error(s)",
                            result.changes_detected, result.projects_checked, result.errors
                        );
                    }
                }
                Err(e) => {
                    warn!("Remote URL monitoring failed: {}", e);
                }
            }
        }
    });
    info!("Remote URL monitoring started (30s interval)");
    handle
}

/// Start periodic git state change detection (transitions 1-5).
pub fn start_git_state_monitor(pool: SqlitePool) -> JoinHandle<()> {
    let handle = tokio::spawn(async move {
        let queue_manager =
            workspace_qdrant_core::queue_operations::QueueManager::new(pool.clone());
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            match check_git_state_changes(&pool, &queue_manager).await {
                Ok(result) => {
                    if result.transitions_detected > 0 {
                        info!(
                            "Git state monitoring: {} transition(s) detected, {} checked, {} error(s)",
                            result.transitions_detected, result.projects_checked, result.errors
                        );
                    }
                }
                Err(e) => {
                    warn!("Git state monitoring failed: {}", e);
                }
            }
        }
    });
    info!("Git state monitoring started (60s interval)");
    handle
}

/// Periodically refresh `file_metadata`-derived gauges from search.db
/// (Task #3 of the FTS5 size-guard series).
///
/// Queries `file_metadata` grouped by `(tenant_id, branch)` every 30s and
/// pushes file_count, total_bytes, and fts5_skipped_count into the matching
/// Prometheus gauges. Skipped pairs that disappear (e.g., after a project
/// is removed) are zeroed-out so panels don't show stale series — mirrors
/// the convention in `start_queue_depth_exporter`.
///
/// Cardinality: one series per (tenant_id, branch) pair across each gauge.
/// On this stack that's ~5 tenants × ~5 branches = ~25 series total, well
/// within Prometheus comfort zone. Adding a path-level label would explode
/// to ~10k series and is intentionally NOT included; for per-file inspection
/// use the admin UI / sidecar SQL queries.
pub fn start_file_metadata_exporter(search_db: Arc<SearchDbManager>) -> JoinHandle<()> {
    let handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
        // Tracks every (tenant_id, branch) pair we've ever emitted so we can
        // zero-out gauges for pairs that vanish (deleted projects / branches).
        let known: std::sync::Arc<tokio::sync::Mutex<std::collections::HashSet<(String, String)>>> =
            std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new()));

        loop {
            interval.tick().await;
            match search_db.file_metadata_stats_by_tenant_branch().await {
                Ok(rows) => {
                    let mut seen = std::collections::HashSet::new();
                    for row in rows {
                        METRICS.set_file_metadata_stats(
                            &row.tenant_id,
                            &row.branch,
                            row.file_count,
                            row.total_bytes,
                            row.skipped_count,
                        );
                        seen.insert((row.tenant_id, row.branch));
                    }
                    let mut guard = known.lock().await;
                    for pair in guard.iter() {
                        if !seen.contains(pair) {
                            METRICS.set_file_metadata_stats(&pair.0, &pair.1, 0, 0, 0);
                        }
                    }
                    guard.extend(seen);
                }
                Err(e) => debug!("file_metadata stats refresh failed: {}", e),
            }
        }
    });
    info!("file_metadata exporter started (30s interval)");
    handle
}

/// Spawn the LSP Prometheus metrics collector (30-second polling loop).
///
/// Every tick it reads `LanguageServerManager::stats()` /
/// `available_languages()` / `active_languages()` and pushes the snapshot
/// into the global `METRICS` gauges.  The task is fire-and-forget during
/// normal operation; `abort_background_tasks` stops it on shutdown.
pub fn start_lsp_metrics_collector(
    lsp_manager: Arc<tokio::sync::RwLock<LanguageServerManager>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
        info!("LSP metrics collector started (30s interval)");
        loop {
            interval.tick().await;
            let mgr = lsp_manager.read().await;

            let stats = mgr.stats().await;
            let available = mgr.available_languages().await;
            let active = mgr.active_languages().await;

            METRICS.set_lsp_snapshot(available.len() as i64, stats.active_servers as i64);

            // Mark all detected-available languages as their running state.
            let active_set: std::collections::HashSet<&str> =
                active.iter().map(|l| l.identifier()).collect();

            for lang in &available {
                METRICS.set_lsp_server_state(
                    lang.identifier(),
                    active_set.contains(lang.identifier()),
                );
            }

            debug!(
                available = available.len(),
                active_servers = stats.active_servers,
                "LSP metrics snapshot updated"
            );
        }
    })
}

/// Spawn the graph stub-edge resolver (120-second loop).
///
/// Tree-sitter emits name-only "stub" callee/import targets (empty file_path)
/// that never match the callee's real node — so the raw call graph is 100%
/// dangling. This task periodically repoints each resolvable stub edge to the
/// real project symbol of the same name (see
/// `GraphStore::resolve_stub_edges`), turning the dangling baseline into a
/// usable intra-project relationship graph for PageRank/communities/impact.
/// Stdlib/external names (no project node) stay dangling and are excluded.
///
/// Runs periodically (not per-file) so it stays O(dangling) per tenant and
/// converges as indexing settles. It also heals the *existing* graph in place,
/// so no reindex is required to benefit.
pub fn start_graph_stub_resolver(
    graph_store: crate::database::ConcreteGraphStore,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(120));
        info!("Graph stub-edge resolver started (120s interval)");
        loop {
            interval.tick().await;
            // Enumerate tenants present in the graph. The read guard is dropped
            // before resolve_stub_edges (which takes the write lock) to avoid
            // self-deadlock on the shared store's RwLock.
            let tenants: Vec<String> = {
                let guard = graph_store.read().await;
                sqlx::query_scalar("SELECT DISTINCT tenant_id FROM graph_edges")
                    .fetch_all(guard.pool())
                    .await
                    .unwrap_or_default()
            };
            for tenant in tenants {
                match graph_store.resolve_stub_edges(&tenant).await {
                    Ok(n) if n > 0 => {
                        METRICS
                            .graph_stub_resolved_total
                            .with_label_values(&[tenant.as_str()])
                            .inc_by(n as u64);
                        info!(tenant = %tenant, repointed = n, "Graph stub resolver repointed edges")
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!(tenant = %tenant, error = %e, "Graph stub resolution failed")
                    }
                }
            }
        }
    })
}

/// Spawn the LSP-authoritative CALLS backfill (R8.3b/R8.4) — flag-gated, OFF by
/// default.
///
/// When `WQM_GRAPH_LSP_BACKFILL=1`, after a settle delay it walks each tenant
/// SERIALLY and, within a tenant, each LANGUAGE that has callers and an available
/// server — NOT a single "dominant" language (a monorepo like DOC-V2 is
/// Java-dominant but its Dart `.build()` calls are the whole point). For each
/// (tenant, language): start the LSP (subject to the global server cap — raise
/// `WQM_LSP_MAX_GLOBAL_SERVERS` to give the backfill a slot), WAIT for it to warm
/// (`is_server_ready_for_file` + a settle dwell) because `start_server` returns
/// before initial indexing finishes so querying it cold resolves nothing, then run
/// `run_backfill_tenant` (scoped to that language) to stamp precise CALLS over the
/// fuzzy fan-out, then stop the server. A server that turns unhealthy during
/// warm-up (a crash-looping jdtls on generated/partial Java) is stopped + skipped
/// immediately, which also halts the health monitor's restart loop. Dormant and
/// zero-cost unless the flag is set. Warm/timeout knobs (seconds):
/// `WQM_GRAPH_LSP_BACKFILL_WARM_SECS` (default 45),
/// `WQM_GRAPH_LSP_BACKFILL_TIMEOUT_SECS` (default 300).
pub fn start_graph_lsp_backfill(
    graph_store: crate::database::ConcreteGraphStore,
    lsp_manager: Arc<tokio::sync::RwLock<LanguageServerManager>>,
    state_pool: SqlitePool,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if std::env::var("WQM_GRAPH_LSP_BACKFILL").ok().as_deref() != Some("1") {
            info!("Graph LSP backfill disabled (set WQM_GRAPH_LSP_BACKFILL=1 to enable)");
            return;
        }
        // Let ingestion/indexing quiesce before activating heavyweight LSP servers.
        tokio::time::sleep(tokio::time::Duration::from_secs(300)).await;
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(6 * 3600));
        info!("Graph LSP backfill enabled (6h interval)");
        // Warm-up knobs (env-tunable so the observation can be re-tuned without a
        // rebuild): how long to keep polling for a server to warm, and how long to
        // dwell after it reports ready before resolving (large projects — a 3k-file
        // Dart monorepo — keep analysing past the grace).
        let warm_after_ready = tokio::time::Duration::from_secs(
            std::env::var("WQM_GRAPH_LSP_BACKFILL_WARM_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(45),
        );
        let ready_timeout = tokio::time::Duration::from_secs(
            std::env::var("WQM_GRAPH_LSP_BACKFILL_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(300),
        );
        const POLL_SECS: u64 = 5;
        const MIN_CALLERS: i64 = 20;
        loop {
            interval.tick().await;
            let tenants: Vec<String> = {
                let guard = graph_store.read().await;
                sqlx::query_scalar("SELECT DISTINCT tenant_id FROM graph_nodes")
                    .fetch_all(guard.pool())
                    .await
                    .unwrap_or_default()
            };
            for tenant in tenants {
                // Absolute project root for this tenant (LSP needs absolute paths).
                let root: Option<String> = sqlx::query_scalar(
                    "SELECT path FROM watch_folders WHERE tenant_id = ?1 AND enabled = 1 LIMIT 1",
                )
                .bind(&tenant)
                .fetch_optional(&state_pool)
                .await
                .ok()
                .flatten();
                let Some(root) = root else { continue };

                // Enumerate the tenant's languages by CALLER count, grouped by the
                // LSP Language they map to (e.g. typescript + tsx -> one server).
                // Per-language, NOT a single "dominant" language: DOC-V2 is
                // Java-dominant but its Dart .build() calls are the whole point.
                let lang_rows: Vec<(String, i64)> = {
                    let guard = graph_store.read().await;
                    sqlx::query_as(
                        "SELECT language, COUNT(*) c FROM graph_nodes \
                         WHERE tenant_id = ?1 AND language IS NOT NULL AND language <> '' \
                           AND symbol_type IN ('function','async_function','method') \
                         GROUP BY language",
                    )
                    .bind(&tenant)
                    .fetch_all(guard.pool())
                    .await
                    .unwrap_or_default()
                };
                let mut groups: HashMap<Language, (Vec<String>, i64)> = HashMap::new();
                for (lang_id, c) in lang_rows {
                    let entry = groups
                        .entry(Language::from_id(&lang_id))
                        .or_insert_with(|| (Vec::new(), 0));
                    entry.0.push(lang_id);
                    entry.1 += c;
                }

                for (language, (lang_ids, callers_n)) in groups {
                    // Skip trivial languages — not worth a heavyweight server.
                    if callers_n < MIN_CALLERS {
                        continue;
                    }

                    // Start the server (respects the global cap; refusal skips it).
                    let started = {
                        let mgr = lsp_manager.read().await;
                        mgr.start_server(&tenant, language.clone(), std::path::Path::new(&root))
                            .await
                    };
                    if let Err(e) = started {
                        warn!(tenant = %tenant, language = ?language, error = %e,
                            "LSP backfill: could not start server (cap/unsupported) — skipping language");
                        continue;
                    }

                    // A representative source file of this language for the
                    // readiness check (its extension identifies the language).
                    let rep_abs: Option<String> = {
                        let guard = graph_store.read().await;
                        let ph: Vec<String> =
                            (0..lang_ids.len()).map(|i| format!("?{}", i + 2)).collect();
                        let sql = format!(
                            "SELECT file_path FROM graph_nodes \
                             WHERE tenant_id = ?1 AND file_path <> '' \
                               AND symbol_type IN ('function','async_function','method') \
                               AND language IN ({}) LIMIT 1",
                            ph.join(", ")
                        );
                        let mut q = sqlx::query_scalar::<_, String>(&sql).bind(&tenant);
                        for l in &lang_ids {
                            q = q.bind(l);
                        }
                        q.fetch_optional(guard.pool())
                            .await
                            .ok()
                            .flatten()
                            .map(|rel| format!("{}/{}", root.trim_end_matches('/'), rel))
                    };

                    // Warm-up wait + health guard. start_server returns before the
                    // server finishes indexing, so querying it immediately yields
                    // nothing (the cold-LSP no-op). Poll until ready + a settle
                    // dwell; bail early if the server turns unhealthy — a
                    // crash-looping jdtls (generated/partial Java, no build model)
                    // never becomes healthy, and stopping it here also prevents the
                    // health monitor's restart loop.
                    let start = tokio::time::Instant::now();
                    let mut ready_since: Option<tokio::time::Instant> = None;
                    let mut warm = false;
                    loop {
                        let (gone, unhealthy, ready) = {
                            let mgr = lsp_manager.read().await;
                            let state = mgr.get_server_state(&tenant, language.clone()).await;
                            let gone = state.is_none();
                            let unhealthy = state
                                .map(|st| {
                                    st.marked_unavailable
                                        || matches!(
                                            st.status,
                                            ServerStatus::Failed | ServerStatus::Stopping
                                        )
                                })
                                .unwrap_or(false);
                            let ready = match rep_abs.as_ref() {
                                Some(f) => {
                                    mgr.is_server_ready_for_file(&tenant, std::path::Path::new(f))
                                        .await
                                }
                                None => start.elapsed() >= warm_after_ready,
                            };
                            (gone, unhealthy, ready)
                        };
                        if gone {
                            warn!(tenant = %tenant, language = ?language,
                                "LSP backfill: server vanished during warm-up — skipping");
                            break;
                        }
                        if unhealthy {
                            warn!(tenant = %tenant, language = ?language,
                                "LSP backfill: server unhealthy (e.g. crash-looping jdtls) — skipping");
                            break;
                        }
                        if ready {
                            let since = ready_since.get_or_insert_with(|| {
                                info!(tenant = %tenant, language = ?language,
                                    warm_secs = warm_after_ready.as_secs(),
                                    "LSP backfill: server ready — warming before resolve");
                                tokio::time::Instant::now()
                            });
                            if since.elapsed() >= warm_after_ready {
                                warm = true;
                                break;
                            }
                        }
                        if start.elapsed() >= ready_timeout {
                            warn!(tenant = %tenant, language = ?language,
                                "LSP backfill: warm-up timed out — skipping");
                            break;
                        }
                        tokio::time::sleep(tokio::time::Duration::from_secs(POLL_SECS)).await;
                    }

                    if warm {
                        // Trigger + await the initial project analysis ONCE, before
                        // the resolve loop. call-hierarchy returns empty until the
                        // analyzer finishes; opening a representative file here (and
                        // waiting for that analysis to settle) is what unblocks slow
                        // analyzers (Dart ~1min) — paid once per tenant, not per file.
                        if let Some(f) = rep_abs.as_ref() {
                            let mgr = lsp_manager.read().await;
                            let p = std::path::Path::new(f);
                            let _ = mgr.open_document(p).await;
                            mgr.wait_for_initial_analysis(p).await;
                            // Leave it open — the resolve loop keeps ≥1 doc open so
                            // the project stays analysed throughout.
                        }
                        let superseded =
                            workspace_qdrant_core::graph::lsp_backfill::run_backfill_tenant(
                                &graph_store,
                                &lsp_manager,
                                &tenant,
                                &root,
                                &lang_ids,
                            )
                            .await;
                        info!(tenant = %tenant, language = ?language, superseded,
                            "LSP backfill: superseded fuzzy CALLS with precise LSP edges");
                    }

                    // Always stop the enrichment server we started (frees the slot
                    // and deregisters it so the health monitor won't keep an
                    // unhealthy one alive).
                    if let Err(e) =
                        lsp_manager.read().await.stop_server(&tenant, language.clone()).await
                    {
                        warn!(tenant = %tenant, language = ?language, error = %e,
                            "LSP backfill: stop_server failed");
                    }
                }
            }
        }
    })
}

/// Spawn the graph metrics exporter (5-minute loop).
///
/// Refreshes the per-tenant graph gauges (`graph_nodes`, `graph_edges`,
/// `graph_unresolved_stubs`) from `graph.db` so the Grafana "Code Graph"
/// dashboard reflects current node/edge-type distribution and the count of
/// *internal* stubs the resolver still owes (external/stdlib refs are excluded
/// — see the query). Gauges are `reset()`
/// each tick so tenants/types that vanish stop reporting stale series. Query
/// latency is NOT exported here — it is already covered by
/// `grpc_request_duration_seconds{service="GraphService"}`.
pub fn start_graph_metrics_refresh(
    graph_store: crate::database::ConcreteGraphStore,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // 5-minute cadence: these are Grafana dashboard gauges (node/edge counts
        // per tenant) that move only with ingestion, so a 60s full-table scan was
        // wasteful background DB load. With the v2 covering index each pass is also
        // an index-only scan rather than a full table scan + sort.
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(300));
        info!("Graph metrics exporter started (300s interval)");
        loop {
            interval.tick().await;

            // Snapshot counts under the read guard, then drop it before touching
            // the Prometheus registry (mirrors start_graph_stub_resolver, which
            // also avoids holding the store lock across unrelated work).
            let (nodes, stubs, edges, by_language) = {
                let guard = graph_store.read().await;
                let pool = guard.pool();
                let nodes: Vec<(String, String, i64)> = sqlx::query_as(
                    "SELECT tenant_id, symbol_type, COUNT(*) \
                     FROM graph_nodes GROUP BY tenant_id, symbol_type",
                )
                .fetch_all(pool)
                .await
                .unwrap_or_default();
                // Count only *actionable* unresolved stubs: a stub (empty
                // file_path) whose symbol_name DOES have a real in-project
                // definition (file_path <> '') but stayed dangling — i.e. the
                // resolver should have repointed it. Stubs with no in-project
                // match are external/stdlib refs (e.g. Override, ByteString,
                // assertThat) — an expected, permanent floor that only pollutes
                // the metric, so they are excluded. The (tenant_id, symbol_name)
                // index keeps the EXISTS check cheap.
                let stubs: Vec<(String, i64)> = sqlx::query_as(
                    "SELECT s.tenant_id, COUNT(*) \
                     FROM graph_nodes s \
                     WHERE s.file_path = '' \
                       AND EXISTS ( \
                         SELECT 1 FROM graph_nodes r \
                         WHERE r.tenant_id = s.tenant_id \
                           AND r.symbol_name = s.symbol_name \
                           AND r.file_path <> '' \
                       ) \
                     GROUP BY s.tenant_id",
                )
                .fetch_all(pool)
                .await
                .unwrap_or_default();
                let edges: Vec<(String, String, i64)> = sqlx::query_as(
                    "SELECT tenant_id, edge_type, COUNT(*) \
                     FROM graph_edges GROUP BY tenant_id, edge_type",
                )
                .fetch_all(pool)
                .await
                .unwrap_or_default();
                // Project x language: node count per tenant per language (NULL/''
                // folded to 'unknown'). Powers the "Project x Language" view.
                let by_language: Vec<(String, String, i64)> = sqlx::query_as(
                    "SELECT tenant_id, COALESCE(NULLIF(language, ''), 'unknown') AS language, \
                            COUNT(*) \
                     FROM graph_nodes GROUP BY tenant_id, language",
                )
                .fetch_all(pool)
                .await
                .unwrap_or_default();
                (nodes, stubs, edges, by_language)
            };

            METRICS.graph_nodes.reset();
            METRICS.graph_edges.reset();
            METRICS.graph_unresolved_stubs.reset();
            for (tenant, node_type, n) in &nodes {
                METRICS
                    .graph_nodes
                    .with_label_values(&[tenant.as_str(), node_type.as_str()])
                    .set(*n);
            }
            for (tenant, n) in &stubs {
                METRICS
                    .graph_unresolved_stubs
                    .with_label_values(&[tenant.as_str()])
                    .set(*n);
            }
            for (tenant, edge_type, n) in &edges {
                METRICS
                    .graph_edges
                    .with_label_values(&[tenant.as_str(), edge_type.as_str()])
                    .set(*n);
            }
            METRICS.graph_nodes_by_language.reset();
            for (tenant, language, n) in &by_language {
                METRICS
                    .graph_nodes_by_language
                    .with_label_values(&[tenant.as_str(), language.as_str()])
                    .set(*n);
            }
        }
    })
}

/// Spawn all periodic background tasks and return their handles.
pub fn spawn_all(
    pool: &SqlitePool,
    search_db: &Arc<SearchDbManager>,
    pause_flag: &Arc<std::sync::atomic::AtomicBool>,
    prometheus_config: &PrometheusExportConfig,
) -> BackgroundHandles {
    let metrics_handle = start_metrics_server(prometheus_config);
    let uptime_handle = start_uptime_tracker();
    let pause_poll_handle = start_pause_polling(pool.clone(), Arc::clone(pause_flag));
    let metrics_collect_handle = start_metrics_collection(pool.clone());
    let metrics_maint_handle = start_metrics_maintenance(pool.clone());

    // Fire-and-forget background tasks (handles not needed for shutdown abort)
    let _timings = start_timings_cleanup(pool.clone());
    let _log_prune = start_log_pruning(pool.clone());
    let _inactivity = start_inactivity_timeout(pool.clone());
    let _session_monitor = start_session_monitor(pool.clone());
    let _remote = start_remote_url_monitor(pool.clone());
    let _git_state = start_git_state_monitor(pool.clone());
    let _queue_depth = start_queue_depth_exporter(pool.clone());
    let _wal_checkpoint = start_wal_checkpoint_loop(pool.clone());
    let _project_inventory = start_indexed_project_inventory_exporter(pool.clone());
    let _search_adoption = start_search_adoption_sampler(pool.clone());
    let _chunking_coverage = start_chunking_coverage_exporter(pool.clone());
    let _grammar_backfill = start_grammar_backfill(pool.clone());
    let _file_metadata = start_file_metadata_exporter(Arc::clone(search_db));

    BackgroundHandles {
        uptime_handle,
        pause_poll_handle,
        metrics_collect_handle,
        metrics_maint_handle,
        grpc_handle: None, // Filled in later by grpc_setup
        metrics_handle,
        lsp_metrics_handle: None, // Filled in after Phase 4 (LSP manager init)
    }
}

#[cfg(test)]
mod proc_sampling_tests {
    use super::{parse_cpu_seconds, parse_rss_pages};

    #[test]
    fn rss_pages_from_statm() {
        // statm: size resident shared text lib data dt
        assert_eq!(parse_rss_pages("1234 567 12 0 0 90 0"), Some(567));
        assert_eq!(parse_rss_pages(""), None);
        assert_eq!(parse_rss_pages("only-one-field"), None);
    }

    #[test]
    fn cpu_seconds_from_stat() {
        // comm with spaces + parens; after ')': state(0) … utime(11) stime(12).
        let stat = "1 (memexd worker) S 0 0 0 0 -1 0 0 0 0 0 100 50 0 0 20 0 1 0 1";
        assert_eq!(parse_cpu_seconds(stat), (100.0 + 50.0) / 100.0);
        // Malformed → 0.0, no panic.
        assert_eq!(parse_cpu_seconds("garbage no paren"), 0.0);
    }
}
