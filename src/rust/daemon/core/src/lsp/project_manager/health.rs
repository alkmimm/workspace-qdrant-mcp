//! Health monitoring: periodic checks, crash detection, restart with backoff.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use tokio::sync::RwLock;

use super::{
    LanguageServerManager, ProjectLanguageKey, ProjectServerState, ServerInstance, ServerStatus,
};

impl LanguageServerManager {
    /// Start the background health check task
    pub(crate) fn start_health_check_task(&self) {
        let interval = Duration::from_secs(self.config.health_check_interval_secs);
        let max_restarts = self.config.max_restarts;
        let stability_reset = Duration::from_secs(self.config.stability_reset_secs);
        let instances = Arc::clone(&self.instances);
        let servers = Arc::clone(&self.servers);
        let running = Arc::clone(&self.running);
        let ready_at = Arc::clone(&self.ready_at);
        let ready_signals = Arc::clone(&self.ready_signals);

        tokio::spawn(async move {
            let mut interval_timer = tokio::time::interval(interval);

            loop {
                interval_timer.tick().await;

                if !*running.read().await {
                    tracing::info!("Health check task shutting down");
                    break;
                }

                Self::perform_health_checks(
                    &instances,
                    &servers,
                    max_restarts,
                    stability_reset,
                    &ready_at,
                    &ready_signals,
                )
                .await;
            }
        });

        tracing::info!(
            interval_secs = self.config.health_check_interval_secs,
            max_restarts = self.config.max_restarts,
            "Health check background task started"
        );
    }

    /// Perform health checks on all active server instances
    async fn perform_health_checks(
        instances: &Arc<
            RwLock<HashMap<ProjectLanguageKey, Arc<tokio::sync::Mutex<ServerInstance>>>>,
        >,
        servers: &Arc<RwLock<HashMap<ProjectLanguageKey, ProjectServerState>>>,
        max_restarts: u32,
        stability_reset: Duration,
        ready_at: &Arc<RwLock<HashMap<ProjectLanguageKey, Instant>>>,
        ready_signals: &Arc<RwLock<HashMap<ProjectLanguageKey, Arc<AtomicBool>>>>,
    ) {
        let keys: Vec<_> = {
            let inst = instances.read().await;
            inst.keys().cloned().collect()
        };

        for key in keys {
            let instance = {
                let inst = instances.read().await;
                inst.get(&key).cloned()
            };

            let Some(instance) = instance else {
                continue;
            };

            let mut inst_guard = instance.lock().await;

            // Warm-up exemption: a server still building its initial index (a large
            // Dart/Java monorepo takes minutes) cannot answer the RPC ping in time —
            // and the ping is a `shutdown` request, so sending it to a busy server
            // risks leaving it half-shut. While the server is not yet ready, only
            // verify the OS process is alive; skip the RPC probe entirely so we never
            // restart a server mid-analysis (which merely restarts the analysis and
            // loops forever). A dead process is still detected and restarted.
            if !Self::server_is_ready(&key, ready_at, ready_signals).await {
                if !inst_guard.is_alive().await {
                    Self::handle_unhealthy_server(&key, &mut inst_guard, servers, max_restarts)
                        .await;
                }
                continue;
            }

            let health_result = inst_guard.health_check().await;

            match health_result {
                Ok(health_metrics) => match health_metrics.status {
                    ServerStatus::Running => {
                        Self::handle_healthy_server(
                            &key,
                            &mut inst_guard,
                            servers,
                            stability_reset,
                        )
                        .await;
                    }
                    // Only a hard `Failed` restarts. `health_check` escalates to
                    // Failed only after >3 consecutive misses; a transient `Degraded`
                    // (a busy analyzer missing a single ping) is tolerated — restarting
                    // on the first miss is what drove the analyze→restart→analyze loop.
                    ServerStatus::Failed => {
                        Self::handle_unhealthy_server(&key, &mut inst_guard, servers, max_restarts)
                            .await;
                    }
                    _ => {}
                },
                Err(e) => {
                    tracing::warn!(
                        project_id = %key.project_id,
                        language = ?key.language,
                        error = %e,
                        "Health check failed with error"
                    );
                }
            }
        }
    }

    /// Mirror of `is_server_ready_for_file`'s readiness test: ready once the server
    /// has signalled its initial indexing finished, or once its warm-up grace has
    /// elapsed (a missing entry is treated as ready). Used to EXEMPT a still-warming
    /// server from the disruptive RPC health probe.
    async fn server_is_ready(
        key: &ProjectLanguageKey,
        ready_at: &Arc<RwLock<HashMap<ProjectLanguageKey, Instant>>>,
        ready_signals: &Arc<RwLock<HashMap<ProjectLanguageKey, Arc<AtomicBool>>>>,
    ) -> bool {
        {
            let signals = ready_signals.read().await;
            if let Some(sig) = signals.get(key) {
                if sig.load(Ordering::Relaxed) {
                    return true;
                }
            }
        }
        let ready_at = ready_at.read().await;
        ready_at
            .get(key)
            .map(|&t| Instant::now() >= t)
            .unwrap_or(true)
    }

    /// Update state for a healthy server, resetting restart count after stability period
    async fn handle_healthy_server(
        key: &ProjectLanguageKey,
        inst_guard: &mut ServerInstance,
        servers: &Arc<RwLock<HashMap<ProjectLanguageKey, ProjectServerState>>>,
        stability_reset: Duration,
    ) {
        let mut servers_guard = servers.write().await;
        let Some(state) = servers_guard.get_mut(key) else {
            return;
        };

        state.status = ServerStatus::Running;
        state.last_healthy_time = Some(Utc::now());

        if state.restart_count > 0 {
            if let Some(last_healthy) = state.last_healthy_time {
                let stable_for = Utc::now() - last_healthy;
                if stable_for > chrono::Duration::from_std(stability_reset).unwrap_or_default() {
                    tracing::info!(
                        project_id = %state.project_id,
                        language = ?state.language,
                        old_count = state.restart_count,
                        "Resetting restart count after stability period"
                    );
                    state.restart_count = 0;
                    state.marked_unavailable = false;
                    inst_guard.reset_restart_attempts();
                }
            }
        }
    }

    /// Attempt to restart an unhealthy server or mark it as permanently failed
    async fn handle_unhealthy_server(
        key: &ProjectLanguageKey,
        inst_guard: &mut ServerInstance,
        servers: &Arc<RwLock<HashMap<ProjectLanguageKey, ProjectServerState>>>,
        max_restarts: u32,
    ) {
        // Read current state to decide what to do
        let (should_restart, is_unavailable) = {
            let mut servers_guard = servers.write().await;
            let Some(state) = servers_guard.get_mut(key) else {
                return;
            };

            state.status = ServerStatus::Failed;
            state.last_error = Some(format!("Health check failed: {:?}", state.status));

            if state.marked_unavailable {
                (false, true)
            } else if state.restart_count < max_restarts {
                tracing::warn!(
                    project_id = %state.project_id,
                    language = ?state.language,
                    restart_count = state.restart_count + 1,
                    max_restarts,
                    "Server failed health check, attempting restart"
                );
                (true, false)
            } else {
                tracing::error!(
                    project_id = %state.project_id,
                    language = ?state.language,
                    restart_count = state.restart_count,
                    "Server permanently failed after max restart attempts"
                );
                state.marked_unavailable = true;
                state.status = ServerStatus::Failed;
                (false, false)
            }
            // servers_guard dropped here, releasing the lock before restart
        };

        if is_unavailable {
            return;
        }

        if !should_restart {
            return;
        }

        // Perform restart with lock released
        match inst_guard.restart().await {
            Ok(()) => {
                let mut servers_guard = servers.write().await;
                if let Some(state) = servers_guard.get_mut(key) {
                    state.restart_count += 1;
                    state.status = ServerStatus::Initializing;
                    state.last_error = None;
                }
                tracing::info!(
                    project_id = %key.project_id,
                    language = ?key.language,
                    "Server restarted successfully"
                );
            }
            Err(e) => {
                tracing::error!(
                    project_id = %key.project_id,
                    language = ?key.language,
                    error = %e,
                    "Failed to restart server"
                );
                let mut servers_guard = servers.write().await;
                if let Some(state) = servers_guard.get_mut(key) {
                    state.restart_count += 1;
                    state.last_error = Some(format!("Restart failed: {}", e));
                }
            }
        }
    }

    /// Handle a potential server crash after an RPC error.
    ///
    /// Returns true if a crash was detected, false otherwise.
    pub(crate) async fn handle_potential_crash(
        &self,
        key: &ProjectLanguageKey,
        error_msg: &str,
    ) -> bool {
        let instance_arc = {
            let instances = self.instances.read().await;
            instances.get(key).cloned()
        };

        let Some(instance_arc) = instance_arc else {
            return false;
        };

        let is_alive = {
            let instance = instance_arc.lock().await;
            instance.is_alive().await
        };

        if is_alive {
            return false;
        }

        tracing::error!(
            project_id = %key.project_id,
            language = ?key.language,
            error = %error_msg,
            "LSP server crashed during query"
        );

        {
            let mut servers = self.servers.write().await;
            if let Some(state) = servers.get_mut(key) {
                state.status = ServerStatus::Failed;
                state.last_error = Some(format!("Server crashed: {}", error_msg));
                tracing::info!(
                    project_id = %key.project_id,
                    language = ?key.language,
                    restart_count = state.restart_count,
                    "Marked LSP server as Failed - health check will attempt restart"
                );
            }
        }

        {
            let mut metrics = self.metrics.write().await;
            metrics.failed_enrichments += 1;
        }

        true
    }

    /// Check health of all active servers and restart crashed ones.
    ///
    /// Returns `(checked_count, restarted_count, failed_count)`.
    pub async fn check_all_servers_health(&self) -> (usize, usize, usize) {
        let keys: Vec<ProjectLanguageKey> = {
            let servers = self.servers.read().await;
            servers
                .iter()
                .filter(|(_, state)| state.is_active)
                .map(|(k, _)| k.clone())
                .collect()
        };

        tracing::debug!("Checking health of {} active LSP servers", keys.len());

        let mut checked = 0;
        let mut restarted = 0;
        let mut failed = 0;

        for key in keys {
            checked += 1;
            match self.check_single_server_health(&key).await {
                SingleHealthResult::Ok => {}
                SingleHealthResult::Restarted => restarted += 1,
                SingleHealthResult::Failed => failed += 1,
            }
        }

        if restarted > 0 || failed > 0 {
            tracing::info!(
                "Health check complete: {} checked, {} restarted, {} failed",
                checked,
                restarted,
                failed
            );
        }

        (checked, restarted, failed)
    }

    /// Check health of a specific project's servers.
    ///
    /// Returns `(checked_count, restarted_count, failed_count)`.
    pub async fn check_project_servers_health(&self, project_id: &str) -> (usize, usize, usize) {
        let keys: Vec<ProjectLanguageKey> = {
            let servers = self.servers.read().await;
            servers
                .iter()
                .filter(|(k, state)| k.project_id == project_id && state.is_active)
                .map(|(k, _)| k.clone())
                .collect()
        };

        let mut checked = 0;
        let mut restarted = 0;
        let mut failed = 0;

        for key in keys {
            checked += 1;
            match self.check_single_server_health(&key).await {
                SingleHealthResult::Ok => {}
                SingleHealthResult::Restarted => restarted += 1,
                SingleHealthResult::Failed => failed += 1,
            }
        }

        (checked, restarted, failed)
    }

    /// Check and optionally restart a single server
    async fn check_single_server_health(&self, key: &ProjectLanguageKey) -> SingleHealthResult {
        let restart_needed = {
            let instances = self.instances.read().await;
            if let Some(instance_arc) = instances.get(key) {
                let instance = instance_arc.lock().await;
                !instance.is_alive().await
            } else {
                false
            }
        };

        if !restart_needed {
            return SingleHealthResult::Ok;
        }

        tracing::info!(
            "LSP server for {:?} in project {} has crashed, attempting restart",
            key.language,
            key.project_id
        );

        let instances = self.instances.write().await;
        if let Some(instance_arc) = instances.get(key) {
            let mut instance = instance_arc.lock().await;
            match instance.check_and_restart_if_needed().await {
                Ok(true) => {
                    tracing::info!(
                        "Successfully restarted LSP server for {:?} in project {}",
                        key.language,
                        key.project_id
                    );
                    SingleHealthResult::Restarted
                }
                Ok(false) | Err(_) => {
                    let mut servers = self.servers.write().await;
                    if let Some(state) = servers.get_mut(key) {
                        state.status = ServerStatus::Failed;
                        state.is_active = false;
                    }
                    SingleHealthResult::Failed
                }
            }
        } else {
            SingleHealthResult::Ok
        }
    }
}

/// Result of checking a single server's health
enum SingleHealthResult {
    Ok,
    Restarted,
    Failed,
}

#[cfg(test)]
mod readiness_exemption_tests {
    use super::{
        Arc, AtomicBool, Duration, HashMap, Instant, LanguageServerManager, ProjectLanguageKey,
        RwLock,
    };
    use crate::lsp::Language;

    fn key() -> ProjectLanguageKey {
        ProjectLanguageKey::new("t1", Language::Dart)
    }

    #[tokio::test]
    async fn missing_ready_at_is_ready() {
        let ready_at = Arc::new(RwLock::new(HashMap::new()));
        let signals = Arc::new(RwLock::new(HashMap::new()));
        assert!(LanguageServerManager::server_is_ready(&key(), &ready_at, &signals).await);
    }

    #[tokio::test]
    async fn warming_server_is_not_ready() {
        // ready_at in the future → still within the warm-up grace → exempt from probe.
        let mut m = HashMap::new();
        m.insert(key(), Instant::now() + Duration::from_secs(120));
        let ready_at = Arc::new(RwLock::new(m));
        let signals = Arc::new(RwLock::new(HashMap::new()));
        assert!(!LanguageServerManager::server_is_ready(&key(), &ready_at, &signals).await);
    }

    #[tokio::test]
    async fn past_grace_is_ready() {
        let mut m = HashMap::new();
        m.insert(key(), Instant::now() - Duration::from_secs(1));
        let ready_at = Arc::new(RwLock::new(m));
        let signals = Arc::new(RwLock::new(HashMap::new()));
        assert!(LanguageServerManager::server_is_ready(&key(), &ready_at, &signals).await);
    }

    #[tokio::test]
    async fn index_signal_overrides_pending_grace() {
        // Signalled index-done → ready even though the grace has not elapsed.
        let mut m = HashMap::new();
        m.insert(key(), Instant::now() + Duration::from_secs(120));
        let ready_at = Arc::new(RwLock::new(m));
        let mut sigs = HashMap::new();
        sigs.insert(key(), Arc::new(AtomicBool::new(true)));
        let signals = Arc::new(RwLock::new(sigs));
        assert!(LanguageServerManager::server_is_ready(&key(), &ready_at, &signals).await);
    }
}
