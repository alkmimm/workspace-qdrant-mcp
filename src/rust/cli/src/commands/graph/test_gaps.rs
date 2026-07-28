//! Graph test-gaps subcommand — production symbols no test reaches
//!
//! Coverage here is CALL-GRAPH REACHABILITY from test code (an approximation),
//! not execution coverage — see the daemon algorithm docs.

use anyhow::{Context, Result};

use crate::grpc::client::workspace_daemon::TestGapsRequest;
use crate::grpc::client::DaemonClient;
use crate::output;

pub async fn test_gaps(tenant_id: &str, top_k: Option<u32>, edge_types: Vec<String>) -> Result<()> {
    output::section("Test Gaps (production symbols no test reaches)");
    output::kv("Tenant", tenant_id);
    if let Some(k) = top_k {
        output::kv("Top K", k.to_string());
    }
    output::separator();

    let mut client = DaemonClient::connect_default()
        .await
        .context("Cannot connect to daemon")?;

    let resp = client
        .graph()
        .detect_test_gaps(TestGapsRequest {
            tenant_id: tenant_id.to_string(),
            edge_types,
            top_k,
        })
        .await
        .context("DetectTestGaps RPC failed")?
        .into_inner();

    let pct = if resp.total_production > 0 {
        (resp.covered as f64 / resp.total_production as f64) * 100.0
    } else {
        100.0
    };
    output::kv(
        "Reached by a test",
        format!(
            "{}/{} production symbols ({:.1}%)",
            resp.covered, resp.total_production, pct
        ),
    );
    output::kv("Gaps", resp.gap_count.to_string());
    output::separator();

    if resp.gaps.is_empty() {
        println!("No test gaps — every production symbol is reached by a test (call-graph reachability).");
        return Ok(());
    }

    println!("{:<36} {:<10} {:>6}  FILE", "SYMBOL", "TYPE", "DEPS");
    for g in &resp.gaps {
        println!(
            "{:<36} {:<10} {:>6}  {}",
            g.symbol_name, g.symbol_type, g.production_dependents, g.file_path
        );
    }

    println!(
        "\nShowing {}/{} gaps ({}ms). DEPS = production symbols that depend on it.",
        resp.gaps.len(),
        resp.gap_count,
        resp.query_time_ms
    );
    println!(
        "Note: coverage = call-graph reachability from test code, NOT execution coverage. \
         Tests are detected by file path — Rust INLINE #[cfg(test)] unit tests are NOT counted \
         (they live on production paths), so a Rust-heavy project over-reports gaps."
    );

    Ok(())
}
