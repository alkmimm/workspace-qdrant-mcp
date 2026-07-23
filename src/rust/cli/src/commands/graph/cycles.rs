//! Graph cycles subcommand — dependency-cycle (SCC) detection

use anyhow::{Context, Result};

use crate::grpc::client::workspace_daemon::CycleRequest;
use crate::grpc::client::DaemonClient;
use crate::output;

pub async fn cycles(
    tenant_id: &str,
    top_k: Option<u32>,
    min_size: Option<u32>,
    edge_types: Vec<String>,
) -> Result<()> {
    output::section("Dependency Cycles");
    output::kv("Tenant", tenant_id);
    if let Some(k) = top_k {
        output::kv("Top K", k.to_string());
    }
    if let Some(m) = min_size {
        output::kv("Min cycle size", m.to_string());
    }
    output::separator();

    let mut client = DaemonClient::connect_default()
        .await
        .context("Cannot connect to daemon")?;

    let resp = client
        .graph()
        .detect_cycles(CycleRequest {
            tenant_id: tenant_id.to_string(),
            edge_types,
            min_cycle_size: min_size,
            top_k,
        })
        .await
        .context("DetectCycles RPC failed")?
        .into_inner();

    if resp.cycles.is_empty() {
        println!("No dependency cycles found.");
        return Ok(());
    }

    for (i, c) in resp.cycles.iter().enumerate() {
        let scope = if c.cross_file { "CROSS-FILE" } else { "same-file" };
        println!(
            "\nCycle #{}  ({} nodes, {} file(s), {})",
            i + 1,
            c.members.len(),
            c.files.len(),
            scope
        );
        for m in &c.members {
            let loc = if m.file_path.is_empty() {
                "(stub)".to_string()
            } else {
                m.file_path.clone()
            };
            println!("    {:<32} {:<10} {}", m.symbol_name, m.symbol_type, loc);
        }
    }

    println!(
        "\nShowing {}/{} cycles ({}ms)",
        resp.cycles.len(),
        resp.total,
        resp.query_time_ms
    );

    Ok(())
}
