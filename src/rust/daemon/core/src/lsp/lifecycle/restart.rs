//! Restart and health-check logic
//!
//! Implements exponential backoff restart, health-driven restart,
//! and periodic health checking for LSP server instances.

use std::time::{Duration, Instant};

use tracing::info;

use crate::lsp::LspResult;

use super::process::ServerInstance;
use super::{HealthMetrics, ServerStatus};

/// The request the periodic health probe sends.
///
/// `$/`-prefixed and unknown to every server on purpose: the LSP spec obliges a
/// server to answer such a request with `MethodNotFound`, which makes the reply
/// a pure liveness signal with no side effects. It must never be a real LSP
/// method — `shutdown` was, and it leaked a tsserver pair per probe (see
/// `health_check`).
pub(crate) const HEALTH_PROBE_METHOD: &str = "$/wqm/ping";

impl ServerInstance {
    /// Perform health check on the server
    pub async fn health_check(&self) -> LspResult<HealthMetrics> {
        let start_time = Instant::now();

        // Check if process is still running
        let process_healthy = {
            let mut process_guard = self.process.lock().await;
            match process_guard.as_mut() {
                Some(child) => match child.try_wait() {
                    Ok(Some(_)) => false, // Process has exited
                    Ok(None) => true,     // Process is still running
                    Err(_) => false,      // Error checking process
                },
                None => false, // No process
            }
        };

        let mut metrics = self.health_metrics.write().await;

        if !process_healthy {
            metrics.status = ServerStatus::Failed;
            metrics.consecutive_failures += 1;
            return Ok(metrics.clone());
        }

        // Liveness probe. ANY JSON-RPC reply — result or error — proves the
        // server's event loop is alive; `send_request` only returns `Err` on a
        // transport failure, and a server-side error object arrives as `Ok`.
        //
        // The method is deliberately one no server implements. The LSP spec
        // requires a `$/`-prefixed request the server does not know to be
        // answered with `MethodNotFound`, so the reply is guaranteed, instant,
        // and free of side effects.
        //
        // This used to send `shutdown` as the ping and then re-`initialize` on
        // the same connection to "keep it running". That is a protocol
        // violation — after `shutdown` a server expects `exit`, not a second
        // `initialize` — and typescript-language-server honoured it the only way
        // it could: every `initialize` spawned a fresh tsserver pair while the
        // previous pair stayed alive inside it. Measured on a live daemon: one
        // new pair every health-check interval, 67 MB each, all in state `Sl`,
        // never reaped — 78 processes / 2.9 GB after 38 minutes, ~8 GB/hour at
        // the 60 s interval, unbounded. The re-initialize is gone with it.
        let rpc_healthy = match tokio::time::timeout(
            Duration::from_secs(5),
            self.rpc_client
                .send_request(HEALTH_PROBE_METHOD, serde_json::json!(null)),
        )
        .await
        {
            Ok(Ok(_)) => true,   // replied (MethodNotFound counts — see above)
            Ok(Err(_)) => false, // transport error
            Err(_) => false,     // timeout
        };

        let response_time = start_time.elapsed();
        metrics.response_time_ms = response_time.as_millis() as u64;

        if rpc_healthy {
            metrics.status = ServerStatus::Running;
            metrics.last_healthy = chrono::Utc::now();
            metrics.consecutive_failures = 0;
        } else {
            metrics.status = if metrics.consecutive_failures > 3 {
                ServerStatus::Failed
            } else {
                ServerStatus::Degraded
            };
            metrics.consecutive_failures += 1;
        }

        // Update average response time (exponential moving average)
        let alpha = 0.1;
        metrics.avg_response_time_ms = alpha * (metrics.response_time_ms as f64)
            + (1.0 - alpha) * metrics.avg_response_time_ms;

        Ok(metrics.clone())
    }

    /// Restart the server instance
    pub async fn restart(&mut self) -> LspResult<()> {
        info!("Restarting LSP server: {}", self.metadata.name);

        // Update restart policy
        self.restart_policy.current_attempts += 1;
        self.restart_policy.last_restart = Some(Instant::now());

        // Stop current process
        self.stop_process().await?;

        // Wait for restart delay with exponential backoff
        let delay = self.calculate_restart_delay();
        tokio::time::sleep(delay).await;

        // Start new process
        self.start().await?;

        info!("LSP server {} restarted successfully", self.metadata.name);
        Ok(())
    }

    /// Calculate restart delay with exponential backoff
    fn calculate_restart_delay(&self) -> Duration {
        let base_delay_secs = self.restart_policy.base_delay.as_secs_f64();
        let multiplier = self.restart_policy.backoff_multiplier;
        let attempts = self.restart_policy.current_attempts as f64;

        let delay_secs = base_delay_secs * multiplier.powf(attempts - 1.0);
        let max_delay_secs = self.restart_policy.max_delay.as_secs_f64();

        Duration::from_secs_f64(delay_secs.min(max_delay_secs))
    }

    /// Check server health and restart if needed
    ///
    /// Returns true if server needed restart, false otherwise.
    pub async fn check_and_restart_if_needed(&mut self) -> LspResult<bool> {
        // Check if process is alive
        if self.is_alive().await {
            return Ok(false); // No restart needed
        }

        // Process died - check restart policy
        if !self.restart_policy.enabled {
            info!(
                "LSP server {} crashed but restart is disabled",
                self.metadata.name
            );
            let mut metrics = self.health_metrics.write().await;
            metrics.status = ServerStatus::Failed;
            return Ok(false);
        }

        if self.restart_policy.current_attempts >= self.restart_policy.max_attempts {
            info!(
                "LSP server {} crashed and exceeded max restart attempts ({})",
                self.metadata.name, self.restart_policy.max_attempts
            );
            let mut metrics = self.health_metrics.write().await;
            metrics.status = ServerStatus::Failed;
            return Ok(false);
        }

        // Check if we should reset restart counter based on time window
        if let Some(last) = self.restart_policy.last_restart {
            if last.elapsed() > self.restart_policy.reset_window {
                self.restart_policy.current_attempts = 0;
            }
        }

        // Attempt restart
        let attempt_number = self.restart_policy.current_attempts + 1;
        info!(
            "LSP server {} crashed, attempting restart (attempt {} of {})",
            self.metadata.name, attempt_number, self.restart_policy.max_attempts
        );

        // Update metrics
        {
            let mut metrics = self.health_metrics.write().await;
            metrics.status = ServerStatus::Failed;
            metrics.consecutive_failures += 1;
        }

        // Perform restart with backoff
        self.restart().await?;

        info!("LSP server {} restarted successfully", self.metadata.name);

        Ok(true)
    }

    /// Reset restart attempts counter (e.g., after extended stable period)
    pub fn reset_restart_attempts(&mut self) {
        self.restart_policy.current_attempts = 0;
        self.restart_policy.last_restart = None;
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::lsp::detection::ServerCapabilities;
    use crate::lsp::{DetectedServer, Language, LspConfig};

    use super::*;

    #[tokio::test]
    async fn test_server_instance_restart_policy_getter() {
        let detected = DetectedServer {
            name: "test-server".to_string(),
            path: PathBuf::from("/usr/bin/test"),
            languages: vec![Language::Rust],
            version: Some("1.0".to_string()),
            capabilities: ServerCapabilities::default(),
            priority: 1,
        };

        let instance = ServerInstance::new(detected, LspConfig::default())
            .await
            .unwrap();
        let policy = instance.restart_policy();

        assert!(policy.enabled);
        assert_eq!(policy.max_attempts, 5);
    }

    #[tokio::test]
    async fn test_server_instance_reset_restart_attempts() {
        let detected = DetectedServer {
            name: "test-server".to_string(),
            path: PathBuf::from("/usr/bin/test"),
            languages: vec![Language::Rust],
            version: Some("1.0".to_string()),
            capabilities: ServerCapabilities::default(),
            priority: 1,
        };

        let mut instance = ServerInstance::new(detected, LspConfig::default())
            .await
            .unwrap();

        // Simulate some restart attempts
        instance.restart_policy.current_attempts = 3;
        instance.restart_policy.last_restart = Some(Instant::now());

        // Reset
        instance.reset_restart_attempts();

        assert_eq!(instance.restart_policy.current_attempts, 0);
        assert!(instance.restart_policy.last_restart.is_none());
    }

    /// A stand-in LSP server: speaks Content-Length JSON-RPC over stdio, logs
    /// every method it receives to `WQM_FAKE_LSP_LOG`, and answers every request
    /// with `MethodNotFound`. That last part is deliberate — it makes the fake
    /// indistinguishable from a real server on the HEALTH outcome, so the only
    /// thing that can tell the fixed probe from the broken one is what was sent.
    const FAKE_LSP_SERVER: &str = r#"
import json, os, sys
log = open(os.environ["WQM_FAKE_LSP_LOG"], "a")
inp, out = sys.stdin.buffer, sys.stdout.buffer
while True:
    length = None
    while True:
        line = inp.readline()
        if not line:
            sys.exit(0)
        if line in (b"\r\n", b"\n"):
            break
        if line.lower().startswith(b"content-length:"):
            length = int(line.split(b":", 1)[1].strip())
    if length is None:
        continue
    msg = json.loads(inp.read(length))
    log.write(str(msg.get("method", "?")) + "\n")
    log.flush()
    if "id" in msg:
        body = json.dumps({"jsonrpc": "2.0", "id": msg["id"],
                           "error": {"code": -32601, "message": "method not found"}}).encode()
        out.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
        out.flush()
"#;

    /// The leak this pins: the health probe used to send `shutdown` and then
    /// `initialize` on the same connection, and typescript-language-server
    /// spawned a fresh tsserver pair on every `initialize` while the previous
    /// pair stayed alive — measured live at one new pair per probe interval,
    /// 67 MB each, ~8 GB/hour, unbounded.
    ///
    /// The fake answers MethodNotFound to everything, so BOTH the broken and the
    /// fixed probe would report the server healthy. That is the point: the
    /// outcome cannot distinguish them, only the wire can. Asserting the method
    /// log is exactly `["$/wqm/ping"]` fails on a `shutdown`, fails on a
    /// trailing `initialize`, and fails on any future "helpful" extra traffic.
    #[tokio::test]
    async fn health_probe_sends_only_a_side_effect_free_ping() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake_lsp.py");
        std::fs::write(&script, FAKE_LSP_SERVER).unwrap();
        let log = dir.path().join("methods.log");

        let mut child = tokio::process::Command::new("python3")
            .arg(&script)
            .env("WQM_FAKE_LSP_LOG", &log)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("python3 must be available (it ships in the rust base image)");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        let detected = DetectedServer {
            name: "fake-lsp".to_string(),
            path: PathBuf::from("python3"),
            languages: vec![Language::TypeScript],
            version: None,
            capabilities: ServerCapabilities::default(),
            priority: 1,
        };
        let instance = ServerInstance::new(detected, LspConfig::default())
            .await
            .unwrap();
        instance
            .rpc_client
            .connect_stdio(stdin, stdout)
            .await
            .unwrap();
        *instance.process.lock().await = Some(child);

        let metrics = instance.health_check().await.unwrap();
        assert_eq!(
            metrics.status,
            ServerStatus::Running,
            "a MethodNotFound reply is a live server, not a failed one"
        );
        assert_eq!(metrics.consecutive_failures, 0);

        let methods = std::fs::read_to_string(&log).unwrap();
        let methods: Vec<&str> = methods.lines().collect();
        assert_eq!(
            methods,
            vec![HEALTH_PROBE_METHOD],
            "the probe must be the ping and NOTHING else — `shutdown` or a trailing \
             `initialize` here is the tsserver leak coming back"
        );
        assert!(
            HEALTH_PROBE_METHOD.starts_with("$/"),
            "only a `$/` method is guaranteed a MethodNotFound reply by the spec"
        );
    }
}
