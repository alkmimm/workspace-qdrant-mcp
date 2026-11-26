//! Metadata Uplifting Background Process (Task 18).
//!
//! When the queue is empty and the system is idle, scans Qdrant for chunks
//! with missing/empty `concept_tags` and backfills them from the dynamic
//! lexicon, without re-chunking or re-embedding.
//!
//! Tracks `uplift_generation` per point to avoid infinite re-processing.
//! Pauses immediately if new queue items appear.
//!
//! The former LSP-enrichment retry lane was retired (F2.1, 2026-07-02): the
//! uplift never actually re-ran LSP (it only generated tags), so points
//! marked `pending` at ingest could never become `success` — the filter only
//! produced perpetual rescans of un-improvable points. Graph LSP resolution
//! lives on the R8 warm-backfill lane instead.

use std::collections::HashMap;
use std::sync::Arc;

use qdrant_client::qdrant::{Condition, Filter, PointId, RetrievedPoint};
use tracing::{debug, info, warn};

use crate::lexicon::LexiconManager;
use crate::storage::{StorageClient, StorageError};

/// Configuration for the metadata uplift process.
#[derive(Debug, Clone)]
pub struct UpliftConfig {
    /// Maximum points to process per uplift batch.
    pub batch_size: u32,
    /// Minimum seconds between uplift attempts.
    pub min_interval_secs: u64,
    /// Current uplift generation (incremented each pass).
    pub current_generation: u64,
}

impl Default for UpliftConfig {
    fn default() -> Self {
        Self {
            batch_size: 10,
            min_interval_secs: 300, // 5 minutes
            current_generation: 1,
        }
    }
}

/// Result of a single uplift pass.
#[derive(Debug, Clone, Default)]
pub struct UpliftStats {
    /// Points scanned (scrolled from Qdrant).
    pub scanned: u64,
    /// Points updated with new metadata.
    pub updated: u64,
    /// Points skipped (already at current generation or no improvement possible).
    pub skipped: u64,
    /// Errors encountered.
    pub errors: u64,
}

/// Return the eligible points from one page. The cursor belongs to the
/// unfiltered page, including points that already carry the generation marker.
fn points_needing_uplift(response: Vec<RetrievedPoint>, generation: u64) -> Vec<UpliftCandidate> {
    let mut candidates = Vec::new();
    for point in response {
        let Some(point_id) = point.id.filter(|id| id.point_id_options.is_some()) else {
            continue;
        };

        let mut payload_map: HashMap<String, serde_json::Value> = HashMap::new();
        for (key, value) in &point.payload {
            payload_map.insert(key.clone(), qdrant_value_to_json(value));
        }

        // Check uplift_generation
        let current_gen = payload_map
            .get("uplift_generation")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        if current_gen >= generation {
            continue; // Already uplifted at current generation
        }

        candidates.push(UpliftCandidate {
            point_id,
            payload: payload_map,
        });
    }

    candidates
}

/// A point that needs metadata uplift.
#[derive(Debug, Clone)]
pub struct UpliftCandidate {
    pub point_id: PointId,
    pub payload: HashMap<String, serde_json::Value>,
}

/// Run one uplift pass: find candidates and update their metadata.
///
/// Returns stats about what was processed.
pub async fn run_uplift_pass(
    storage_client: &Arc<StorageClient>,
    lexicon_manager: &Arc<LexiconManager>,
    collections: &[String],
    config: &UpliftConfig,
    cursors: &mut HashMap<String, PointId>,
) -> UpliftStats {
    let mut stats = UpliftStats::default();

    for collection in collections {
        let filter = Filter::must([Condition::is_empty("concept_tags")]);
        let (page, next) = match storage_client
            .scroll_filtered_page(
                collection,
                filter,
                config.batch_size,
                cursors.get(collection).cloned(),
            )
            .await
        {
            Ok(page) => page,
            Err(e) => {
                stats.errors += 1;
                debug!("Skipping uplift for '{}': {}", collection, e);
                continue;
            }
        };
        let candidates = points_needing_uplift(page, config.current_generation);
        let errors_before = stats.errors;

        if !candidates.is_empty() {
            info!(
                "Found {} points needing uplift in '{}'",
                candidates.len(),
                collection
            );
        }

        for candidate in &candidates {
            stats.scanned += 1;

            match uplift_single_point(
                storage_client,
                lexicon_manager,
                collection,
                candidate,
                config,
            )
            .await
            {
                Ok(true) => stats.updated += 1,
                Ok(false) => stats.skipped += 1,
                Err(e) => {
                    warn!(
                        "Failed to uplift point {:?} in '{}': {}",
                        candidate.point_id, collection, e
                    );
                    stats.errors += 1;
                }
            }
        }
        // A page with only marked points still advances. If an update fails,
        // retry the page and skip points already stamped on the next pass.
        if stats.errors == errors_before {
            match next {
                Some(next) => {
                    cursors.insert(collection.clone(), next);
                }
                None => {
                    cursors.remove(collection);
                }
            }
        }
    }

    stats
}

/// Uplift a single point's metadata.
///
/// Updates:
/// - Applies new tags from dynamic lexicon if concept_tags is empty
/// - Sets uplift_generation to current
///
/// Returns true if the point was updated, false if skipped.
async fn uplift_single_point(
    storage_client: &Arc<StorageClient>,
    lexicon_manager: &Arc<LexiconManager>,
    collection: &str,
    candidate: &UpliftCandidate,
    config: &UpliftConfig,
) -> Result<bool, StorageError> {
    let mut updates: HashMap<String, serde_json::Value> = HashMap::new();
    let mut changed = false;

    // Check if concept_tags is missing or empty
    let has_tags = candidate
        .payload
        .get("concept_tags")
        .and_then(|v| v.as_array())
        .map(|arr| !arr.is_empty())
        .unwrap_or(false);

    if !has_tags {
        // Try to generate tags from the content using lexicon
        if let Some(content) = candidate.payload.get("content").and_then(|v| v.as_str()) {
            let tokens: Vec<String> = content
                .split_whitespace()
                .map(|w| {
                    w.to_lowercase()
                        .trim_matches(|c: char| !c.is_alphanumeric())
                        .to_string()
                })
                .filter(|w| w.len() >= 2)
                .collect();

            // Find distinctive terms (high DF relative to corpus)
            let corpus_size = lexicon_manager.corpus_size(collection).await;
            if corpus_size > 0 {
                let mut term_scores: Vec<(String, f64)> = Vec::new();
                for term in &tokens {
                    let df = lexicon_manager.document_frequency(collection, term).await;
                    if df > 0 {
                        // IDF score: ln(N / (1 + df))
                        let idf = (corpus_size as f64 / (1.0 + df as f64)).ln();
                        if idf > 0.5 {
                            // Only distinctive terms
                            term_scores.push((term.clone(), idf));
                        }
                    }
                }
                term_scores
                    .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                let top_tags: Vec<String> =
                    term_scores.into_iter().take(5).map(|(t, _)| t).collect();

                if !top_tags.is_empty() {
                    updates.insert("concept_tags".to_string(), serde_json::json!(top_tags));
                    changed = true;
                }
            }
        }
    }

    // Always stamp uplift_generation — for a point that yielded no tags this
    // marker is what stops the next pass from re-scanning it (the generation
    // guard in find_points_needing_uplift).
    updates.insert(
        "uplift_generation".to_string(),
        serde_json::json!(config.current_generation),
    );

    storage_client
        .set_payload_on_point_id(collection, candidate.point_id.clone(), updates)
        .await?;

    Ok(changed)
}

/// Convert a Qdrant Value to serde_json::Value.
fn qdrant_value_to_json(value: &qdrant_client::qdrant::Value) -> serde_json::Value {
    match &value.kind {
        Some(qdrant_client::qdrant::value::Kind::StringValue(s)) => {
            serde_json::Value::String(s.clone())
        }
        Some(qdrant_client::qdrant::value::Kind::IntegerValue(i)) => {
            serde_json::json!(*i)
        }
        Some(qdrant_client::qdrant::value::Kind::DoubleValue(d)) => {
            serde_json::json!(*d)
        }
        Some(qdrant_client::qdrant::value::Kind::BoolValue(b)) => {
            serde_json::json!(*b)
        }
        Some(qdrant_client::qdrant::value::Kind::ListValue(list)) => {
            let items: Vec<serde_json::Value> =
                list.values.iter().map(qdrant_value_to_json).collect();
            serde_json::Value::Array(items)
        }
        Some(qdrant_client::qdrant::value::Kind::StructValue(s)) => {
            let map: serde_json::Map<String, serde_json::Value> = s
                .fields
                .iter()
                .map(|(k, v)| (k.clone(), qdrant_value_to_json(v)))
                .collect();
            serde_json::Value::Object(map)
        }
        Some(qdrant_client::qdrant::value::Kind::NullValue(_)) | None => serde_json::Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uplift_page_keeps_real_ids_for_unmarked_points() {
        let uuid = PointId::from("00183067-42d9-c9df-d9a1-4a5f1e7e8d54");
        let numeric = PointId::from(42u64);
        let marked = RetrievedPoint {
            id: Some(PointId::from(7u64)),
            payload: [("uplift_generation".to_string(), 1i64.into())].into(),
            ..Default::default()
        };
        let page = [uuid.clone(), numeric.clone()].map(|id| RetrievedPoint {
            id: Some(id),
            ..Default::default()
        });
        let eligible = points_needing_uplift([vec![marked], Vec::from(page)].concat(), 1);
        let ids: Vec<_> = eligible.into_iter().map(|p| p.point_id).collect();
        assert_eq!(ids, vec![uuid, numeric]);
    }

    #[test]
    fn test_uplift_config_defaults() {
        let config = UpliftConfig::default();
        assert_eq!(config.batch_size, 10);
        assert_eq!(config.min_interval_secs, 300);
        assert_eq!(config.current_generation, 1);
    }

    #[test]
    fn test_uplift_stats_default() {
        let stats = UpliftStats::default();
        assert_eq!(stats.scanned, 0);
        assert_eq!(stats.updated, 0);
        assert_eq!(stats.skipped, 0);
        assert_eq!(stats.errors, 0);
    }

    #[test]
    fn test_qdrant_value_to_json_string() {
        let value = qdrant_client::qdrant::Value {
            kind: Some(qdrant_client::qdrant::value::Kind::StringValue(
                "hello".to_string(),
            )),
        };
        assert_eq!(qdrant_value_to_json(&value), serde_json::json!("hello"));
    }

    #[test]
    fn test_qdrant_value_to_json_int() {
        let value = qdrant_client::qdrant::Value {
            kind: Some(qdrant_client::qdrant::value::Kind::IntegerValue(42)),
        };
        assert_eq!(qdrant_value_to_json(&value), serde_json::json!(42));
    }

    #[test]
    fn test_qdrant_value_to_json_null() {
        let value = qdrant_client::qdrant::Value { kind: None };
        assert_eq!(qdrant_value_to_json(&value), serde_json::Value::Null);
    }
}
