//! Graph impact analysis subcommand — nodes affected by changing a symbol

use anyhow::{Context, Result};

use crate::grpc::client::workspace_daemon::{ImpactAnalysisRequest, ImpactNodeProto};
use crate::grpc::client::DaemonClient;
use crate::output;

pub async fn impact_analysis(
    symbol_name: &str,
    tenant_id: &str,
    file_path: Option<String>,
    min_confidence: Option<f64>,
    max_hops: Option<u32>,
    branch: Option<String>,
) -> Result<()> {
    output::section("Impact Analysis");
    output::kv("Symbol", symbol_name);
    output::kv("Tenant", tenant_id);
    if let Some(ref fp) = file_path {
        output::kv("File", fp);
    }
    if let Some(mc) = min_confidence {
        output::kv("Min Confidence", format!("{mc}"));
    }
    output::separator();

    let mut client = DaemonClient::connect_default()
        .await
        .context("Cannot connect to daemon")?;

    let resp = client
        .graph()
        .impact_analysis(ImpactAnalysisRequest {
            tenant_id: tenant_id.to_string(),
            symbol_name: symbol_name.to_string(),
            file_path,
            // CLI shows all impacted nodes; top_k (MCP-only cap) stays unset.
            top_k: None,
            min_confidence,
            branch,
            max_hops,
        })
        .await
        .context("ImpactAnalysis RPC failed")?
        .into_inner();
    super::print_scope(resp.scope.as_ref());
    output::kv("Depth", format!("{} hops", resp.max_hops));

    if resp.impacted_nodes.is_empty() {
        println!("No impacted nodes found.");
    }

    // Group by impact type
    let mut direct = Vec::new();
    let mut indirect = Vec::new();

    for node in &resp.impacted_nodes {
        if node.distance <= 1 {
            direct.push(node);
        } else {
            indirect.push(node);
        }
    }

    if !direct.is_empty() {
        println!("\nDirect callers ({}):", direct.len());
        for n in &direct {
            println!("  {} ({})", qualified(n), n.file_path);
        }
    }

    if !indirect.is_empty() {
        println!("\nIndirect callers ({}):", indirect.len());
        for n in &indirect {
            println!(
                "  {} ({}) [distance: {}]",
                qualified(n),
                n.file_path,
                n.distance
            );
        }
    }

    println!(
        "\nTotal impacted: {} nodes ({}ms)",
        resp.total_impacted, resp.query_time_ms
    );
    if resp.dropped_below_confidence_floor > 0 {
        output::warning(format!(
            "{} caller(s) reach this definition only through an ambiguous same-name call \
             (confidence < 0.6) and were left out because --file pins one definition; \
             omit --file to include them",
            resp.dropped_below_confidence_floor
        ));
    }
    if resp.node_budget_reached {
        output::warning("The walk hit its node budget: the blast radius above is truncated");
    }

    Ok(())
}

/// `Class.method` for a member, the bare name otherwise.
fn qualified(node: &ImpactNodeProto) -> String {
    match node.parent_symbol.as_deref() {
        Some(parent) => format!("{parent}.{}", node.symbol_name),
        None => node.symbol_name.clone(),
    }
}
