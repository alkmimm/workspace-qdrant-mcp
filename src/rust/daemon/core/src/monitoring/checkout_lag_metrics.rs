//! `memexd_main_checkout_behind_commits`: how far each project's main checkout
//! of its trunk trails `origin/<trunk>` (see `git::main_checkout_lag`).
//!
//! Registered on the daemon registry on first use, beside the series
//! `metrics_core` builds. A project whose main folder has the trunk checked
//! out always reports a value — 0 when current — so "nothing wrong" is a real
//! sample, never a missing one.

use std::collections::HashMap;

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

/// Which projects a pass should log as stale: those at or past the threshold
/// whose count changed since they were last logged. A WARN per project per
/// change, not one every 5 minutes for as long as nobody pulls.
#[derive(Debug, Default)]
pub struct LagWarnings {
    /// Last count logged, per tenant still at or past the threshold.
    logged: HashMap<String, usize>,
}

impl LagWarnings {
    /// The tenants of `pass` (`(tenant_id, behind)`) to log now. A tenant that
    /// fell below the threshold, or is absent from the pass (unwatched, left
    /// its trunk), is forgotten: crossing again logs again.
    pub fn due<'a>(&mut self, pass: &[(&'a str, usize)], threshold: usize) -> Vec<&'a str> {
        let mut logged = HashMap::new();
        let mut due = Vec::new();
        for &(tenant, behind) in pass {
            if behind < threshold {
                continue;
            }
            if self.logged.get(tenant) != Some(&behind) {
                due.push(tenant);
            }
            logged.insert(tenant.to_string(), behind);
        }
        self.logged = logged;
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stale_project_is_logged_once_per_change() {
        let mut w = LagWarnings::default();
        assert_eq!(w.due(&[("doc", 412), ("agent", 0)], 20), vec!["doc"]);
        assert!(w.due(&[("doc", 412)], 20).is_empty(), "unchanged: quiet");
        assert_eq!(w.due(&[("doc", 413)], 20), vec!["doc"], "origin moved on");
        assert!(w.due(&[("doc", 19)], 20).is_empty(), "below the line");
        assert_eq!(w.due(&[("doc", 413)], 20), vec!["doc"], "crossed again");
        assert!(w.due(&[], 20).is_empty(), "unwatched");
        assert_eq!(
            w.due(&[("doc", 413)], 20),
            vec!["doc"],
            "back after leaving the pass"
        );
        assert_eq!(w.due(&[("doc", 20)], 20), vec!["doc"], "the line itself");
    }

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
