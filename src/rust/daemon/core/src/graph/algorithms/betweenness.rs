/// Betweenness centrality using Brandes' algorithm.
use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Wall-clock budget for the Brandes source loop. Betweenness is the heaviest
/// centrality action (exact is O(V·(V+E))); on a large graph the default
/// "all sources" run blows past the gRPC call timeout. When the budget is hit we
/// stop and return the partial (approximate) scores accumulated so far — the same
/// safety net community detection got via `LP_TIME_BUDGET` (#153). Callers can
/// still pass `max_samples` to cap the source set explicitly.
const BETWEENNESS_TIME_BUDGET: Duration = Duration::from_secs(20);
use sqlx::SqlitePool;
use tracing::info;

use super::{load_adjacency_graph, GenericityFilter};

/// Betweenness centrality score for a node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BetweennessEntry {
    pub node_id: String,
    pub symbol_name: String,
    pub symbol_type: String,
    pub file_path: String,
    pub score: f64,
}

/// Betweenness scores plus how much of the source set actually contributed.
///
/// `budget_hit` is the fact a caller must see: when the wall-clock budget cut
/// the Brandes loop short, `entries` are an approximation over
/// `sources_processed` of `sources_total` sources — and the cut point depends
/// on machine load, so two identical calls need not agree. A caller that
/// compares results across runs (or across deployments) must treat a
/// `budget_hit` answer as unreliable, or bound the work with `max_samples` at
/// or below `sources_processed` so every run walks the same sources.
#[derive(Debug, Clone)]
pub struct BetweennessReport {
    /// Ranked entries: score descending, node id ascending on ties.
    pub entries: Vec<BetweennessEntry>,
    /// Sources the run set out to walk (`max_samples` or every node).
    pub sources_total: usize,
    /// Sources actually walked before the budget (or the end) stopped it.
    pub sources_processed: usize,
    /// True when the time budget stopped the loop before `sources_total`.
    pub budget_hit: bool,
}

/// Compute approximate betweenness centrality using Brandes' algorithm.
///
/// For each node s, runs BFS from s, then accumulates dependency values
/// along shortest paths. Normalized to [0, 1]. Returns only the ranked
/// entries; use [`compute_betweenness_report`] when the caller must know
/// whether the budget truncated the run.
pub async fn compute_betweenness_centrality(
    pool: &SqlitePool,
    tenant_id: &str,
    edge_types: Option<&[&str]>,
    max_samples: Option<usize>,
) -> Result<Vec<BetweennessEntry>, sqlx::Error> {
    compute_betweenness_report(
        pool,
        tenant_id,
        edge_types,
        max_samples,
        BETWEENNESS_TIME_BUDGET,
    )
    .await
    .map(|r| r.entries)
}

/// [`compute_betweenness_centrality`] with the run's provenance attached. The
/// budget is a parameter so a test can drive the truncation path with
/// `Duration::ZERO` instead of building a graph large enough to take 20 s.
pub async fn compute_betweenness_report(
    pool: &SqlitePool,
    tenant_id: &str,
    edge_types: Option<&[&str]>,
    max_samples: Option<usize>,
    budget: Duration,
) -> Result<BetweennessReport, sqlx::Error> {
    let graph =
        load_adjacency_graph(pool, tenant_id, edge_types, GenericityFilter::All, false).await?;

    if graph.nodes.len() < 3 {
        let mut entries: Vec<BetweennessEntry> = graph
            .nodes
            .iter()
            .map(|(id, info)| BetweennessEntry {
                node_id: id.clone(),
                symbol_name: info.symbol_name.clone(),
                symbol_type: info.symbol_type.clone(),
                file_path: info.file_path.clone(),
                score: 0.0,
            })
            .collect();
        entries.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        let n = entries.len();
        return Ok(BetweennessReport {
            entries,
            sources_total: n,
            sources_processed: n,
            budget_hit: false,
        });
    }

    // Sorted, not `keys()` order: `HashMap` iteration is randomized per map
    // instance, and this list decides BOTH which sources a `max_samples` run
    // walks (`.take(limit)` below) and which sources a budget-truncated run
    // reaches before it stops. With the old order the same call sampled a
    // different source set every time and, on any graph big enough to hit the
    // budget, returned different scores and a different top-k on every call
    // (measured live: two identical `bridges` calls, 15 entries, every score
    // different, last entry different). Sorting ties the source set to the
    // graph's content, so the only remaining variable is how FAR the budget
    // lets the loop run — which the report exposes instead of hiding.
    let mut node_ids: Vec<&String> = graph.nodes.keys().collect();
    node_ids.sort_unstable();

    let mut neighbors: HashMap<&str, Vec<&str>> = HashMap::new();
    for (src, targets) in &graph.outgoing {
        for tgt in targets {
            neighbors
                .entry(src.as_str())
                .or_default()
                .push(tgt.as_str());
            neighbors
                .entry(tgt.as_str())
                .or_default()
                .push(src.as_str());
        }
    }
    // The undirected lists were assembled by iterating a HashMap of directed
    // lists, so their order is arbitrary; Brandes' path-count accumulation is
    // order-independent in exact arithmetic but not in floating point.
    for list in neighbors.values_mut() {
        list.sort_unstable();
    }

    let mut betweenness: HashMap<&str, f64> =
        node_ids.iter().map(|id| (id.as_str(), 0.0)).collect();

    let sources: Vec<&str> = match max_samples {
        Some(limit) if limit < node_ids.len() => {
            node_ids.iter().take(limit).map(|id| id.as_str()).collect()
        }
        _ => node_ids.iter().map(|id| id.as_str()).collect(),
    };

    let start = Instant::now();
    let mut processed = 0usize;
    let mut budget_hit = false;
    for &source in &sources {
        if start.elapsed() >= budget {
            budget_hit = true;
            info!(
                tenant_id,
                processed,
                total_sources = sources.len(),
                "Betweenness time budget hit — returning partial (approximate) scores"
            );
            break;
        }
        brandes_bfs(source, &neighbors, &mut betweenness);
        processed += 1;
    }

    // Normalize against the sources actually processed (sample_scale divides by
    // this), so a budget-truncated run still scales its scores correctly.
    let processed_sources = &sources[..processed];
    let entries = normalize_betweenness(betweenness, &graph.nodes, &node_ids, processed_sources);

    info!(
        tenant_id,
        nodes = entries.len(),
        sources = processed,
        budget_hit,
        "Betweenness centrality computation complete"
    );

    Ok(BetweennessReport {
        entries,
        sources_total: sources.len(),
        sources_processed: processed,
        budget_hit,
    })
}

/// Normalize raw betweenness scores and convert to sorted `BetweennessEntry` list.
fn normalize_betweenness<'a>(
    betweenness: HashMap<&'a str, f64>,
    nodes: &'a HashMap<String, super::NodeInfo>,
    node_ids: &[&String],
    sources: &[&str],
) -> Vec<BetweennessEntry> {
    let n = node_ids.len() as f64;
    let normalizer = if n > 2.0 {
        (n - 1.0) * (n - 2.0) / 2.0
    } else {
        1.0
    };
    // A run the budget stopped before its FIRST source has no sample to scale
    // (n / 0 is +inf, and 0 × inf is NaN): every raw score is still 0.0, so
    // leave the scale at 1 and let the zeros through as zeros.
    let sample_scale = if sources.is_empty() {
        1.0
    } else if sources.len() < node_ids.len() {
        n / sources.len() as f64
    } else {
        1.0
    };

    let mut results: Vec<BetweennessEntry> = betweenness
        .into_iter()
        .filter_map(|(id, raw_score)| {
            nodes.get(id).map(|info| BetweennessEntry {
                node_id: id.to_string(),
                symbol_name: info.symbol_name.clone(),
                symbol_type: info.symbol_type.clone(),
                file_path: info.file_path.clone(),
                score: ((raw_score * sample_scale) / normalizer).min(1.0),
            })
        })
        .collect();

    // Score ties are the rule, not the exception (most leaves score exactly
    // 0.0), and the input order is a HashMap's — so a score-only sort put a
    // different tied entry at the top_k boundary on every call. Node id (a
    // content hash) is the tiebreaker that makes the cut a function of the
    // graph.
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.node_id.cmp(&b.node_id))
    });
    results
}

/// Run BFS from a single source and accumulate betweenness contributions.
fn brandes_bfs<'a>(
    source: &'a str,
    neighbors: &HashMap<&'a str, Vec<&'a str>>,
    betweenness: &mut HashMap<&'a str, f64>,
) {
    let mut stack: Vec<&str> = Vec::new();
    let mut predecessors: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut sigma: HashMap<&str, f64> = HashMap::new(); // num shortest paths
    let mut dist: HashMap<&str, i64> = HashMap::new();

    sigma.insert(source, 1.0);
    dist.insert(source, 0);

    // BFS
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(source);

    while let Some(v) = queue.pop_front() {
        stack.push(v);
        let d_v = dist[v];

        for &w in neighbors.get(v).unwrap_or(&Vec::new()) {
            // First visit
            if !dist.contains_key(w) {
                dist.insert(w, d_v + 1);
                queue.push_back(w);
            }
            // Shortest path through v
            if dist[w] == d_v + 1 {
                let sigma_v = *sigma.get(v).unwrap_or(&0.0);
                *sigma.entry(w).or_default() += sigma_v;
                predecessors.entry(w).or_default().push(v);
            }
        }
    }

    // Back-propagation of dependencies
    let mut delta: HashMap<&str, f64> = HashMap::new();

    while let Some(w) = stack.pop() {
        if let Some(preds) = predecessors.get(w) {
            let sigma_w = *sigma.get(w).unwrap_or(&1.0);
            let delta_w = *delta.get(w).unwrap_or(&0.0);

            for &v in preds {
                let sigma_v = *sigma.get(v).unwrap_or(&1.0);
                let contribution = (sigma_v / sigma_w) * (1.0 + delta_w);
                *delta.entry(v).or_default() += contribution;
            }
        }

        if w != source {
            *betweenness.entry(w).or_default() += delta.get(w).unwrap_or(&0.0);
        }
    }
}
