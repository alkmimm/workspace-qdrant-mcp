//! Helper utilities for the GraphService.

use tonic::Status;
use workspace_qdrant_core::graph::EdgeType;

/// Parse edge_types from proto string list, returning None for "all".
pub(crate) fn parse_edge_type_filter(types: &[String]) -> Result<Option<Vec<String>>, Status> {
    if types.is_empty() {
        return Ok(None);
    }
    // Validate all types are known
    for t in types {
        if EdgeType::from_str(t).is_none() {
            return Err(Status::invalid_argument(format!(
                "unknown edge type: {}",
                t
            )));
        }
    }
    Ok(Some(types.to_vec()))
}

/// Retain only nodes whose confidence >= `min_confidence`, when a positive
/// threshold is given. `None`, `Some(0.0)`, or a negative value = no filter, so
/// the CLI and unset MCP calls keep the full list (backward-compatible).
///
/// Applied daemon-side BEFORE the `top_k` cap and the reported total, so `top_k`
/// fills with nodes that PASS the filter and the total reflects the filtered
/// universe (not the pre-filter count). A node's confidence is the best-path
/// edge-weight PRODUCT and every weight is <= 1, so `product >= min` implies
/// every edge on that best path is >= min — filtering by node confidence is thus
/// equivalent to pruning weak edges during traversal, without the traversal-
/// internal bookkeeping.
///
/// Caveats: the traversal's NODE_BUDGET is spent BEFORE this filter runs (a
/// heavily fanned-out low-confidence frontier can still truncate how far the
/// walk reaches), and a node's confidence follows the traversal's
/// shortest-path-first semantics (same-depth ties keep the strongest edge; a
/// longer stronger path does not override a shorter weaker one). Thresholds
/// > 1.0 are rejected at the handlers before this helper is reached.
pub(crate) fn retain_min_confidence<T>(
    nodes: &mut Vec<T>,
    min_confidence: Option<f64>,
    confidence_of: impl Fn(&T) -> f64,
) {
    if let Some(min) = min_confidence {
        if min > 0.0 {
            nodes.retain(|n| confidence_of(n) >= min);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retain_min_confidence_none_or_zero_keeps_all() {
        let original = vec![0.1_f64, 0.7, 1.0];
        let mut v = original.clone();
        retain_min_confidence(&mut v, None, |c| *c);
        assert_eq!(v, original, "None must not filter");
        retain_min_confidence(&mut v, Some(0.0), |c| *c);
        assert_eq!(v, original, "0.0 must not filter (disabled)");
        retain_min_confidence(&mut v, Some(-1.0), |c| *c);
        assert_eq!(v, original, "negative must not filter");
    }

    #[test]
    fn retain_min_confidence_threshold_is_inclusive() {
        let mut v = vec![0.1_f64, 0.5, 0.7, 1.0];
        retain_min_confidence(&mut v, Some(0.5), |c| *c);
        assert_eq!(v, vec![0.5, 0.7, 1.0], ">= threshold kept (inclusive)");
    }

    #[test]
    fn retain_min_confidence_drops_homonym_fanout() {
        // Real case: unique/scoped edges (>=0.7) survive; 1/N homonym fan-out
        // (~0.167, e.g. six same-named `is_empty` candidates) is dropped at the
        // typical precision threshold.
        let mut v = vec![0.7_f64, 0.9, 0.166_66, 0.166_66];
        retain_min_confidence(&mut v, Some(0.5), |c| *c);
        assert_eq!(v, vec![0.7, 0.9]);
    }
}
