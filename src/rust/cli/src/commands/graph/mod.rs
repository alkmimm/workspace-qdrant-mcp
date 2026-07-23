//! Graph command - code relationship queries and algorithms
//!
//! Subcommands: query, impact, stats, pagerank, communities, betweenness, cycles, migrate

use anyhow::Result;
use clap::{Args, Subcommand};

mod betweenness;
mod communities;
mod cycles;
mod impact;
mod migrate;
mod pagerank;
mod query;
mod stats;

/// Graph command arguments
#[derive(Args)]
pub struct GraphArgs {
    #[command(subcommand)]
    command: GraphCommand,
}

/// Confidence is a best-path edge-weight product in [0,1] — not a percentage.
/// Reject out-of-range values here so `--min-confidence 50` fails loudly instead
/// of silently filtering out every node (all confidences are <= 1.0).
fn parse_confidence(s: &str) -> Result<f64, String> {
    let v: f64 = s.parse().map_err(|e| format!("not a number: {e}"))?;
    if (0.0..=1.0).contains(&v) {
        Ok(v)
    } else {
        Err(format!("must be within 0.0..=1.0 (a probability-like product, not a percentage), got {v}"))
    }
}

/// Graph subcommands
#[derive(Subcommand)]
enum GraphCommand {
    /// Query nodes related to a symbol within N hops
    Query {
        /// Node ID to query from
        #[arg(long)]
        node_id: String,

        /// Project tenant_id
        #[arg(long)]
        tenant: String,

        /// Maximum traversal depth (1-5)
        #[arg(long, default_value = "2")]
        hops: u32,

        /// Edge type filter (comma-separated: CALLS,IMPORTS,CONTAINS,USES_TYPE,EXTENDS,IMPLEMENTS)
        #[arg(long, value_delimiter = ',')]
        edge_types: Vec<String>,

        /// Drop nodes whose best-path confidence is below this (0-1; omit = all)
        #[arg(long, value_parser = parse_confidence)]
        min_confidence: Option<f64>,
    },

    /// Impact analysis: find nodes affected by changing a symbol
    Impact {
        /// Symbol name to analyze
        #[arg(long)]
        symbol: String,

        /// Project tenant_id
        #[arg(long)]
        tenant: String,

        /// Narrow to specific file path
        #[arg(long)]
        file: Option<String>,

        /// Drop impacted nodes whose best-path confidence is below this (0-1; omit = all)
        #[arg(long, value_parser = parse_confidence)]
        min_confidence: Option<f64>,
    },

    /// Graph statistics (node/edge counts)
    Stats {
        /// Project tenant_id (omit for all tenants)
        #[arg(long)]
        tenant: Option<String>,
    },

    /// Compute PageRank scores for graph nodes
    Pagerank {
        /// Project tenant_id
        #[arg(long)]
        tenant: String,

        /// Damping factor (default: 0.85)
        #[arg(long)]
        damping: Option<f64>,

        /// Maximum iterations (default: 100)
        #[arg(long)]
        max_iterations: Option<u32>,

        /// Convergence tolerance (default: 1e-6)
        #[arg(long)]
        tolerance: Option<f64>,

        /// Return only top K results
        #[arg(long)]
        top_k: Option<u32>,

        /// Edge type filter (comma-separated)
        #[arg(long, value_delimiter = ',')]
        edge_types: Vec<String>,
    },

    /// Detect code communities via label propagation
    Communities {
        /// Project tenant_id
        #[arg(long)]
        tenant: String,

        /// Max label propagation iterations (default: 50)
        #[arg(long)]
        max_iterations: Option<u32>,

        /// Minimum community size to include (default: 2)
        #[arg(long)]
        min_size: Option<u32>,

        /// Return only the top K largest communities
        #[arg(long)]
        top_k: Option<u32>,

        /// Edge type filter (comma-separated)
        #[arg(long, value_delimiter = ',')]
        edge_types: Vec<String>,
    },

    /// Compute betweenness centrality scores
    Betweenness {
        /// Project tenant_id
        #[arg(long)]
        tenant: String,

        /// Return only top K results
        #[arg(long)]
        top_k: Option<u32>,

        /// Sample N source nodes for large graphs (0 = all)
        #[arg(long)]
        max_samples: Option<u32>,

        /// Edge type filter (comma-separated)
        #[arg(long, value_delimiter = ',')]
        edge_types: Vec<String>,
    },

    /// Detect dependency cycles (circular CALLS/IMPORTS between symbols/files)
    Cycles {
        /// Project tenant_id
        #[arg(long)]
        tenant: String,

        /// Return only the top K cycles (cross-file first, then largest)
        #[arg(long)]
        top_k: Option<u32>,

        /// Minimum cycle size to report (default: 2; pass 1 to include self-recursion)
        #[arg(long)]
        min_size: Option<u32>,

        /// Edge type filter (comma-separated; default: CALLS,IMPORTS,USES_TYPE,EXTENDS,IMPLEMENTS)
        #[arg(long, value_delimiter = ',')]
        edge_types: Vec<String>,
    },

    /// Migrate graph data between backends
    Migrate {
        /// Source backend (sqlite or ladybug)
        #[arg(long, default_value = "sqlite")]
        from: String,

        /// Target backend (sqlite or ladybug)
        #[arg(long, default_value = "ladybug")]
        to: String,

        /// Migrate specific tenant (omit for all)
        #[arg(long)]
        tenant: Option<String>,

        /// Import batch size (default: 500)
        #[arg(long)]
        batch_size: Option<u32>,
    },
}

pub async fn execute(args: GraphArgs) -> Result<()> {
    match args.command {
        GraphCommand::Query {
            node_id,
            tenant,
            hops,
            edge_types,
            min_confidence,
        } => query::query_related(&node_id, &tenant, hops, edge_types, min_confidence).await,
        GraphCommand::Impact {
            symbol,
            tenant,
            file,
            min_confidence,
        } => impact::impact_analysis(&symbol, &tenant, file, min_confidence).await,
        GraphCommand::Stats { tenant } => stats::graph_stats(tenant).await,
        GraphCommand::Pagerank {
            tenant,
            damping,
            max_iterations,
            tolerance,
            top_k,
            edge_types,
        } => {
            pagerank::pagerank(
                &tenant,
                damping,
                max_iterations,
                tolerance,
                top_k,
                edge_types,
            )
            .await
        }
        GraphCommand::Communities {
            tenant,
            max_iterations,
            min_size,
            top_k,
            edge_types,
        } => communities::communities(&tenant, max_iterations, min_size, top_k, edge_types).await,
        GraphCommand::Betweenness {
            tenant,
            top_k,
            max_samples,
            edge_types,
        } => betweenness::betweenness(&tenant, top_k, max_samples, edge_types).await,
        GraphCommand::Cycles {
            tenant,
            top_k,
            min_size,
            edge_types,
        } => cycles::cycles(&tenant, top_k, min_size, edge_types).await,
        GraphCommand::Migrate {
            from,
            to,
            tenant,
            batch_size,
        } => migrate::migrate(&from, &to, tenant, batch_size).await,
    }
}
