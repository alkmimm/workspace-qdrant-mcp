//! `FailoverDenseProvider` — preferred endpoint with explicit, configured
//! fallback.
//!
//! Wraps two providers serving the SAME model (e.g. a GPU embedding server as
//! the preferred endpoint and a CPU server as standby). Every call tries the
//! primary first; on any error it logs a WARN, memoizes the failure for
//! `retry_after` (so a dead primary is not re-dialed on every request), and
//! serves from the fallback. After the memo expires the primary is retried —
//! recovery is automatic, no restart needed.
//!
//! This is NOT the "silent fallback to a different provider" the
//! `DenseProvider` contract forbids: both endpoints MUST serve the same
//! model/dimensionality (vectors are model-bound, not server-bound), the
//! fallback is explicitly configured (`embedding.fallback_base_url`), and
//! every switchover is logged.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tracing::{debug, info, warn};

use super::DenseProvider;
use crate::embedding::types::{DenseEmbedding, EmbeddingError};

/// How long a primary failure is memoized before the primary is retried.
const PRIMARY_RETRY_SECS: u64 = 60;

/// Preferred + fallback dense provider pair (same model on both endpoints).
pub struct FailoverDenseProvider {
    primary: Arc<dyn DenseProvider>,
    fallback: Arc<dyn DenseProvider>,
    /// While `Some(t)` and `now < t`, skip the primary without dialing it.
    primary_down_until: Mutex<Option<Instant>>,
    retry_after: Duration,
    label: String,
}

impl std::fmt::Debug for FailoverDenseProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FailoverDenseProvider")
            .field("primary", &self.primary.provider_label())
            .field("fallback", &self.fallback.provider_label())
            .field("retry_after", &self.retry_after)
            .finish_non_exhaustive()
    }
}

impl FailoverDenseProvider {
    pub fn new(primary: Arc<dyn DenseProvider>, fallback: Arc<dyn DenseProvider>) -> Self {
        Self::with_retry_after(primary, fallback, Duration::from_secs(PRIMARY_RETRY_SECS))
    }

    /// Constructor with an explicit memo duration (used by tests).
    pub fn with_retry_after(
        primary: Arc<dyn DenseProvider>,
        fallback: Arc<dyn DenseProvider>,
        retry_after: Duration,
    ) -> Self {
        let label = format!(
            "{} (fallback: {})",
            primary.provider_label(),
            fallback.provider_label()
        );
        Self {
            primary,
            fallback,
            primary_down_until: Mutex::new(None),
            retry_after,
            label,
        }
    }

    /// True when the primary should be attempted (no active down-memo).
    fn primary_eligible(&self) -> bool {
        let guard = self.primary_down_until.lock().expect("memo lock poisoned");
        match *guard {
            Some(until) => Instant::now() >= until,
            None => true,
        }
    }

    fn memoize_primary_down(&self) {
        let mut guard = self.primary_down_until.lock().expect("memo lock poisoned");
        *guard = Some(Instant::now() + self.retry_after);
    }

    fn clear_primary_down(&self) {
        let mut guard = self.primary_down_until.lock().expect("memo lock poisoned");
        if guard.take().is_some() {
            info!(
                provider = %self.primary.provider_label(),
                "Primary embedding endpoint recovered — leaving fallback"
            );
        }
    }
}

/// True when a primary error is a permanent payload rejection the fallback
/// cannot recover from. HTTP 400/413/422 (e.g. `string_too_long`) mean the
/// input itself is unacceptable — both endpoints serve the same model with the
/// same input cap, so the fallback would reject the identical input the same
/// way. Failing over would only burn a second call, and demoting the (healthy)
/// primary would route ALL traffic to the slower fallback for the whole memo
/// window over one poison document.
///
/// These are exactly the codes the queue processor's error classifier keys as
/// `permanent_data` (`unified_queue_processor::metrics`); matching the
/// structured variant here keeps the two in lockstep without re-parsing the
/// Display string. Every other error (5xx, connection refused, timeout, auth)
/// is treated as an endpoint failure and triggers failover.
fn is_permanent_payload_rejection(err: &EmbeddingError) -> bool {
    matches!(
        err,
        EmbeddingError::RemoteError {
            status_code: 400 | 413 | 422,
            ..
        }
    )
}

#[async_trait]
impl DenseProvider for FailoverDenseProvider {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<DenseEmbedding>, EmbeddingError> {
        if self.primary_eligible() {
            match self.primary.embed(texts).await {
                Ok(out) => {
                    self.clear_primary_down();
                    return Ok(out);
                }
                // A permanent payload rejection is about the INPUT, not the
                // endpoint: the fallback would reject it identically. Propagate
                // directly — do not demote the (healthy) primary, do not waste a
                // fallback attempt. The downstream classifier marks it
                // `permanent_data` so the item is dropped without a retry.
                Err(e) if is_permanent_payload_rejection(&e) => {
                    debug!(
                        primary = %self.primary.provider_label(),
                        error = %e,
                        "Primary rejected input as a permanent payload error — propagating without failover"
                    );
                    return Err(e);
                }
                Err(e) => {
                    warn!(
                        primary = %self.primary.provider_label(),
                        fallback = %self.fallback.provider_label(),
                        retry_after_secs = self.retry_after.as_secs(),
                        error = %e,
                        "Primary embedding endpoint failed — switching to fallback"
                    );
                    self.memoize_primary_down();
                }
            }
        }
        self.fallback.embed(texts).await
    }

    fn output_dim(&self) -> usize {
        // Both endpoints serve the same model, so the dims agree; report the
        // one we are currently routing to so probe-driven drift updates win.
        if self.primary_eligible() {
            self.primary.output_dim()
        } else {
            self.fallback.output_dim()
        }
    }

    fn provider_label(&self) -> &str {
        &self.label
    }

    fn metrics_label(&self) -> &'static str {
        self.primary.metrics_label()
    }

    async fn probe(&self) -> Result<(), EmbeddingError> {
        match self.primary.probe().await {
            Ok(()) => {
                self.clear_primary_down();
                Ok(())
            }
            Err(primary_err) => {
                warn!(
                    primary = %self.primary.provider_label(),
                    error = %primary_err,
                    "Primary embedding endpoint probe failed — probing fallback"
                );
                self.memoize_primary_down();
                self.fallback.probe().await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Debug)]
    struct MockProvider {
        name: &'static str,
        dim: usize,
        fail: AtomicBool,
        /// When `fail` is set and this is `Some(code)`, the provider fails with
        /// a structured `RemoteError` carrying that HTTP status instead of the
        /// default `GenerationError`. Lets a test drive the 4xx payload-
        /// rejection path. Immutable after construction.
        remote_status: Option<u16>,
        embed_calls: AtomicUsize,
    }

    impl MockProvider {
        fn new(name: &'static str, fail: bool) -> Arc<Self> {
            Arc::new(Self {
                name,
                dim: 4,
                fail: AtomicBool::new(fail),
                remote_status: None,
                embed_calls: AtomicUsize::new(0),
            })
        }

        /// A provider whose `embed` fails with a `RemoteError` carrying
        /// `status` (a permanent 4xx payload rejection). Flip `fail` to `false`
        /// afterwards to make it healthy again.
        fn new_remote_error(name: &'static str, status: u16) -> Arc<Self> {
            Arc::new(Self {
                name,
                dim: 4,
                fail: AtomicBool::new(true),
                remote_status: Some(status),
                embed_calls: AtomicUsize::new(0),
            })
        }
    }

    #[async_trait]
    impl DenseProvider for MockProvider {
        async fn embed(&self, texts: &[&str]) -> Result<Vec<DenseEmbedding>, EmbeddingError> {
            self.embed_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                return Err(match self.remote_status {
                    Some(status_code) => EmbeddingError::RemoteError {
                        status_code,
                        message: format!("{} rejected input", self.name),
                    },
                    None => EmbeddingError::GenerationError {
                        message: format!("{} down", self.name),
                    },
                });
            }
            Ok(texts
                .iter()
                .map(|t| {
                    let mut vector = vec![0.0_f32; self.dim];
                    vector[0] = 1.0;
                    DenseEmbedding {
                        vector,
                        model_name: self.name.to_string(),
                        sequence_length: t.len(),
                    }
                })
                .collect())
        }

        fn output_dim(&self) -> usize {
            self.dim
        }

        fn provider_label(&self) -> &str {
            self.name
        }

        fn metrics_label(&self) -> &'static str {
            "openai_compatible_other"
        }

        async fn probe(&self) -> Result<(), EmbeddingError> {
            if self.fail.load(Ordering::SeqCst) {
                Err(EmbeddingError::GenerationError {
                    message: format!("{} down", self.name),
                })
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn healthy_primary_serves_and_fallback_is_untouched() {
        let primary = MockProvider::new("gpu", false);
        let fallback = MockProvider::new("cpu", false);
        let failover = FailoverDenseProvider::new(primary.clone(), fallback.clone());

        let out = failover.embed(&["a", "b"]).await.expect("embed ok");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].model_name, "gpu");
        assert_eq!(primary.embed_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fallback.embed_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn failed_primary_switches_to_fallback_and_is_memoized() {
        let primary = MockProvider::new("gpu", true);
        let fallback = MockProvider::new("cpu", false);
        let failover = FailoverDenseProvider::with_retry_after(
            primary.clone(),
            fallback.clone(),
            Duration::from_secs(3600),
        );

        let out = failover.embed(&["a"]).await.expect("fallback serves");
        assert_eq!(out[0].model_name, "cpu");
        assert_eq!(primary.embed_calls.load(Ordering::SeqCst), 1);

        // Second call within the memo window must NOT re-dial the primary.
        let out = failover.embed(&["b"]).await.expect("fallback serves");
        assert_eq!(out[0].model_name, "cpu");
        assert_eq!(primary.embed_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fallback.embed_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn permanent_payload_rejection_propagates_without_failover_or_memo() {
        // A 4xx payload rejection (e.g. 422 string_too_long) is a property of
        // the INPUT, not the endpoint: the fallback serves the same model with
        // the same cap and would reject it identically. So the primary must NOT
        // be demoted (one poison doc must not route all traffic to the slower
        // fallback for the memo window) and the fallback must NOT be dialed.
        let primary = MockProvider::new_remote_error("gpu", 422);
        let fallback = MockProvider::new("cpu", false);
        let failover = FailoverDenseProvider::with_retry_after(
            primary.clone(),
            fallback.clone(),
            Duration::from_secs(3600),
        );

        let err = failover
            .embed(&["poison"])
            .await
            .expect_err("422 payload rejection must propagate");
        assert!(
            matches!(
                err,
                EmbeddingError::RemoteError {
                    status_code: 422,
                    ..
                }
            ),
            "expected the primary's 422 to propagate verbatim, got: {err}"
        );
        assert_eq!(primary.embed_calls.load(Ordering::SeqCst), 1);
        // Fallback never dialed — it can't succeed where the primary failed on
        // the same input.
        assert_eq!(fallback.embed_calls.load(Ordering::SeqCst), 0);

        // Primary was NOT memoized down: a subsequent healthy call still tries
        // the primary first and wins (had it been demoted, this would route to
        // the "cpu" fallback instead).
        primary.fail.store(false, Ordering::SeqCst);
        let out = failover.embed(&["b"]).await.expect("primary serves");
        assert_eq!(out[0].model_name, "gpu");
        assert_eq!(primary.embed_calls.load(Ordering::SeqCst), 2);
        assert_eq!(fallback.embed_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn primary_is_retried_after_memo_expires_and_recovers() {
        let primary = MockProvider::new("gpu", true);
        let fallback = MockProvider::new("cpu", false);
        let failover = FailoverDenseProvider::with_retry_after(
            primary.clone(),
            fallback.clone(),
            Duration::ZERO,
        );

        let out = failover.embed(&["a"]).await.expect("fallback serves");
        assert_eq!(out[0].model_name, "cpu");

        // Primary comes back; with an expired memo it is retried and wins.
        primary.fail.store(false, Ordering::SeqCst);
        let out = failover.embed(&["b"]).await.expect("primary serves");
        assert_eq!(out[0].model_name, "gpu");
        assert_eq!(primary.embed_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn both_down_propagates_fallback_error() {
        let primary = MockProvider::new("gpu", true);
        let fallback = MockProvider::new("cpu", true);
        let failover = FailoverDenseProvider::new(primary, fallback);

        let err = failover.embed(&["a"]).await.expect_err("both down");
        assert!(err.to_string().contains("cpu down"));
    }

    #[tokio::test]
    async fn probe_is_healthy_when_only_fallback_responds() {
        let primary = MockProvider::new("gpu", true);
        let fallback = MockProvider::new("cpu", false);
        let failover = FailoverDenseProvider::new(primary, fallback.clone());

        failover.probe().await.expect("fallback probe ok");
        // And the memo now routes embeds straight to the fallback.
        let out = failover.embed(&["a"]).await.expect("fallback serves");
        assert_eq!(out[0].model_name, "cpu");
    }
}
