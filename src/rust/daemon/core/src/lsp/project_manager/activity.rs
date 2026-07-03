//! Idle LSP-server eviction and project-server restore.

use super::{Language, LanguageServerManager, ProjectLspResult};

impl LanguageServerManager {
    /// Evict LSP servers that haven't been used within `idle_timeout`.
    ///
    /// Returns the list of (project_id, language) pairs that were stopped.
    pub async fn evict_idle_servers(
        &self,
        idle_timeout: std::time::Duration,
    ) -> Vec<(String, Language)> {
        let now = std::time::Instant::now();

        // Collect keys of idle servers
        let idle_keys: Vec<_> = {
            let servers = self.servers.read().await;
            servers
                .iter()
                .filter(|(_, state)| {
                    if let Some(last) = state.last_enrichment_at {
                        now.duration_since(last) > idle_timeout
                    } else {
                        // Never enriched — treat as idle
                        true
                    }
                })
                .map(|(key, _)| key.clone())
                .collect()
        };

        let mut evicted = Vec::new();
        for key in idle_keys {
            tracing::info!(
                project_id = %key.project_id,
                language = ?key.language,
                "Evicting idle LSP server"
            );
            if self
                .stop_server(&key.project_id, key.language.clone())
                .await
                .is_ok()
            {
                evicted.push((key.project_id.clone(), key.language));
            }
        }
        evicted
    }

    /// Restore servers for a project that was previously active.
    ///
    /// Note: Server state persistence has been removed as part of 3-table SQLite
    /// compliance. This method now returns an empty list as there is no persisted
    /// state to restore. Servers will be started fresh when `start_server` is called.
    pub async fn restore_project_servers(
        &self,
        _project_id: &str,
    ) -> ProjectLspResult<Vec<Language>> {
        Ok(vec![])
    }
}
