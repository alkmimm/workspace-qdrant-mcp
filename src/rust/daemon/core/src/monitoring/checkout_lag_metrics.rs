//! `memexd_main_checkout_behind_commits`: how far each project's main checkout
//! of its trunk trails `origin/<trunk>` (see `git::main_checkout_lag`).
//!
//! Registered on the daemon registry on first use, beside the series
//! `metrics_core` builds. A project whose main folder has the trunk checked
//! out always reports a value — 0 when current — so "nothing wrong" is a real
//! sample, never a missing one.

use once_cell::sync::Lazy;
use prometheus::{IntGaugeVec, Opts};

use super::METRICS;

static MAIN_CHECKOUT_BEHIND: Lazy<IntGaugeVec> = Lazy::new(|| {
    let gauge = IntGaugeVec::new(
        Opts::new(
            "memexd_main_checkout_behind_commits",
            "Commits origin/<trunk> has that the project's main checkout of the trunk lacks",
        ),
        &["tenant_id", "project", "trunk"],
    )
    .expect("valid metric definition");
    if let Err(e) = METRICS.registry.register(Box::new(gauge.clone())) {
        tracing::warn!("register memexd_main_checkout_behind_commits: {e}");
    }
    gauge
});

/// One project's lag, as a sample of the current pass.
pub struct CheckoutLagSample<'a> {
    pub tenant_id: &'a str,
    pub project: &'a str,
    pub trunk: &'a str,
    pub behind: usize,
}

/// Replace every series with this pass's samples, so a project that left its
/// trunk (or was unwatched) drops out instead of reporting a stale number.
pub fn publish_checkout_lag(samples: &[CheckoutLagSample<'_>]) {
    MAIN_CHECKOUT_BEHIND.reset();
    for s in samples {
        MAIN_CHECKOUT_BEHIND
            .with_label_values(&[s.tenant_id, s.project, s.trunk])
            .set(s.behind as i64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pass_replaces_the_previous_samples() {
        publish_checkout_lag(&[
            CheckoutLagSample {
                tenant_id: "t1",
                project: "DOC-V2",
                trunk: "main",
                behind: 412,
            },
            CheckoutLagSample {
                tenant_id: "t2",
                project: "agent",
                trunk: "main",
                behind: 0,
            },
        ]);
        let out = METRICS.encode().unwrap();
        assert!(out.contains("memexd_main_checkout_behind_commits"), "{out}");
        assert!(
            out.contains(r#"project="DOC-V2""#) && out.contains("412"),
            "{out}"
        );

        publish_checkout_lag(&[CheckoutLagSample {
            tenant_id: "t2",
            project: "agent",
            trunk: "main",
            behind: 0,
        }]);
        let out = METRICS.encode().unwrap();
        assert!(!out.contains(r#"project="DOC-V2""#), "pulled away: {out}");
        assert!(
            out.contains(r#"project="agent""#),
            "a current checkout is a real 0: {out}"
        );
    }
}
