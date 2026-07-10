//! Point update operations
//!
//! Update sparse vectors and payload fields on existing Qdrant points.

use qdrant_client::qdrant::Filter;
use tracing::info;

use crate::storage::client::StorageClient;
use crate::storage::convert::convert_json_to_qdrant_value;
use crate::storage::types::StorageError;

impl StorageClient {
    /// Update only the sparse named vector for a batch of points.
    ///
    /// Leaves the dense vector and payload untouched. Used by `rebalance-idf`
    /// to apply IDF correction factors without re-embedding dense vectors.
    pub async fn update_named_sparse_vectors(
        &self,
        collection_name: &str,
        updates: Vec<(String, std::collections::HashMap<u32, f32>)>,
    ) -> Result<(), StorageError> {
        use qdrant_client::qdrant::{
            point_id, vector, vectors, NamedVectors, PointVectors, SparseVector,
            UpdatePointVectorsBuilder, Vector, Vectors,
        };

        if updates.is_empty() {
            return Ok(());
        }

        let point_vectors: Vec<PointVectors> = updates
            .into_iter()
            .map(|(id, sparse_map)| {
                let mut entries: Vec<(u32, f32)> = sparse_map.into_iter().collect();
                entries.sort_by_key(|(idx, _)| *idx);
                let indices: Vec<u32> = entries.iter().map(|(i, _)| *i).collect();
                let values: Vec<f32> = entries.iter().map(|(_, v)| *v).collect();

                let sparse_vec = Vector {
                    vector: Some(vector::Vector::Sparse(SparseVector { indices, values })),
                    ..Default::default()
                };
                let mut named = std::collections::HashMap::new();
                named.insert("sparse".to_string(), sparse_vec);

                PointVectors {
                    id: Some(qdrant_client::qdrant::PointId {
                        point_id_options: Some(point_id::PointIdOptions::Uuid(id)),
                    }),
                    vectors: Some(Vectors {
                        vectors_options: Some(vectors::VectorsOptions::Vectors(NamedVectors {
                            vectors: named,
                        })),
                    }),
                }
            })
            .collect();

        let builder = UpdatePointVectorsBuilder::new(collection_name, point_vectors).wait(true);

        self.retry_operation(|| async {
            self.client
                .update_vectors(builder.clone())
                .await
                .map_err(|e| StorageError::Point(format!("Failed to update sparse vectors: {}", e)))
        })
        .await?;

        Ok(())
    }

    /// Update payload fields on a single point identified by UUID.
    ///
    /// Convenience wrapper that avoids exposing `qdrant_client::Filter` to
    /// callers in the CLI layer.
    pub async fn set_payload_on_point(
        &self,
        collection_name: &str,
        point_id: &str,
        payload: std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<(), StorageError> {
        use qdrant_client::qdrant::{point_id, PointId, SetPayloadPointsBuilder};

        let id = PointId {
            point_id_options: Some(point_id::PointIdOptions::Uuid(point_id.to_string())),
        };

        let qdrant_payload: std::collections::HashMap<String, qdrant_client::qdrant::Value> =
            payload
                .into_iter()
                .map(|(k, v)| (k, convert_json_to_qdrant_value(v)))
                .collect();

        // Vec<PointId> implements Into<PointsIdsList> which implements Into<PointsSelectorOneOf>.
        let set_payload_request = SetPayloadPointsBuilder::new(collection_name, qdrant_payload)
            .points_selector(vec![id])
            .wait(true);

        self.retry_operation(|| async {
            self.client
                .set_payload(set_payload_request.clone())
                .await
                .map_err(|e| StorageError::Point(format!("Failed to set payload on point: {}", e)))
        })
        .await?;

        Ok(())
    }

    /// Update payload fields on all points matching a filter.
    ///
    /// Used for cascade renames where tenant_id needs to be updated.
    pub async fn set_payload_by_filter(
        &self,
        collection_name: &str,
        filter: Filter,
        payload: std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<(), StorageError> {
        use qdrant_client::qdrant::SetPayloadPointsBuilder;

        if !self.collection_exists(collection_name).await? {
            return Err(StorageError::Collection(format!(
                "Collection not found: {}",
                collection_name
            )));
        }

        let qdrant_payload: std::collections::HashMap<String, qdrant_client::qdrant::Value> =
            payload
                .into_iter()
                .map(|(k, v)| (k, convert_json_to_qdrant_value(v)))
                .collect();

        let count = self
            .count_points_with_filter(collection_name, filter.clone())
            .await?;
        info!(
            "Updating payload on {} point(s) in collection '{}'",
            count, collection_name
        );

        let set_payload_request = SetPayloadPointsBuilder::new(collection_name, qdrant_payload)
            .points_selector(filter)
            .wait(true);

        self.retry_operation(|| async {
            self.client
                .set_payload(set_payload_request.clone())
                .await
                .map_err(|e| StorageError::Point(format!("Failed to set payload: {}", e)))
        })
        .await?;

        info!(
            "Updated payload on {} point(s) in '{}'",
            count, collection_name
        );

        Ok(())
    }

    /// Read the `branch` payload array shared by all points of a `base_point`.
    ///
    /// Layer 2: one physical point per (tenant, path, content), shared across
    /// branches via an array in the `branch` payload field. Tolerates a legacy
    /// scalar `branch` (decoded as a one-element set) for mixed data during the
    /// reembed window. Empty when no point matches.
    ///
    /// `pub` so the full-ingest path can snapshot the shared branch set BEFORE
    /// its upsert overwrites it, then restore the union afterwards (issue #224).
    pub async fn read_branch_set(
        &self,
        collection_name: &str,
        base_point: &str,
    ) -> Result<Vec<String>, StorageError> {
        use qdrant_client::qdrant::{value::Kind, Condition, Filter};
        let filter = Filter::must([Condition::matches("base_point", base_point.to_string())]);
        let points = self
            .scroll_with_filter(collection_name, filter, 1, None)
            .await?;
        let Some(p) = points.into_iter().next() else {
            return Ok(Vec::new());
        };
        let set = match p.payload.get("branch").and_then(|v| v.kind.as_ref()) {
            Some(Kind::ListValue(list)) => list
                .values
                .iter()
                .filter_map(|v| match v.kind.as_ref() {
                    Some(Kind::StringValue(s)) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            Some(Kind::StringValue(s)) => vec![s.clone()],
            _ => Vec::new(),
        };
        Ok(set)
    }

    /// Add `branch` to the shared `branch` array of every point under
    /// `base_point`. Returns the number of points sharing the base_point
    /// (0 ⇒ none exist; the caller should fall back to a full ingest).
    /// Idempotent — a no-op if the branch is already present.
    pub async fn add_branch_to_base_point(
        &self,
        collection_name: &str,
        base_point: &str,
        branch: &str,
    ) -> Result<usize, StorageError> {
        use qdrant_client::qdrant::{Condition, Filter};
        let filter = Filter::must([Condition::matches("base_point", base_point.to_string())]);
        let count = self
            .count_points_with_filter(collection_name, filter.clone())
            .await? as usize;
        if count == 0 {
            return Ok(0);
        }
        let mut set = self.read_branch_set(collection_name, base_point).await?;
        if !set.iter().any(|b| b == branch) {
            set.push(branch.to_string());
            let mut payload = std::collections::HashMap::new();
            payload.insert("branch".to_string(), serde_json::json!(set));
            self.set_payload_by_filter(collection_name, filter, payload)
                .await?;
        }
        Ok(count)
    }

    /// Remove `branch` from the shared `branch` array of every point under
    /// `base_point`. Returns the number of branches REMAINING (0 ⇒ the points
    /// are orphaned and the caller should delete them). No-op if absent.
    pub async fn remove_branch_from_base_point(
        &self,
        collection_name: &str,
        base_point: &str,
        branch: &str,
    ) -> Result<usize, StorageError> {
        use qdrant_client::qdrant::{Condition, Filter};
        let mut set = self.read_branch_set(collection_name, base_point).await?;
        let before = set.len();
        set.retain(|b| b != branch);
        if set.len() != before {
            let filter = Filter::must([Condition::matches("base_point", base_point.to_string())]);
            let mut payload = std::collections::HashMap::new();
            payload.insert("branch".to_string(), serde_json::json!(set));
            self.set_payload_by_filter(collection_name, filter, payload)
                .await?;
        }
        Ok(set.len())
    }

    /// Merge `extra_branches` into the shared `branch` array of a base_point,
    /// preserving whatever is already there.
    ///
    /// Issue #224: a full-ingest upsert (`insert_points_batch` in `store_track`)
    /// overwrites each point's payload — including `branch` — with only the
    /// CURRENT branch. A base_point that OTHER branches had tagged therefore
    /// loses those tags (Qdrant drifts below the `tracked_files` authority, so a
    /// branch-scoped search on the dropped branch silently misses the file). The
    /// full-ingest path snapshots the prior set BEFORE its upsert and calls this
    /// right AFTER to restore the union. Idempotent; no-op when `extra_branches`
    /// are all already present or the base_point has no points.
    pub async fn merge_branches_into_base_point(
        &self,
        collection_name: &str,
        base_point: &str,
        extra_branches: &[String],
    ) -> Result<(), StorageError> {
        use qdrant_client::qdrant::{Condition, Filter};
        if extra_branches.is_empty() {
            return Ok(());
        }
        let mut set = self.read_branch_set(collection_name, base_point).await?;
        if set.is_empty() {
            return Ok(()); // no points under this base_point — nothing to merge onto
        }
        if merge_branch_set(&mut set, extra_branches) {
            let filter = Filter::must([Condition::matches("base_point", base_point.to_string())]);
            let mut payload = std::collections::HashMap::new();
            payload.insert("branch".to_string(), serde_json::json!(set));
            self.set_payload_by_filter(collection_name, filter, payload)
                .await?;
        }
        Ok(())
    }
}

/// Append each of `extra` to `set` when absent, preserving order and skipping
/// duplicates. Returns `true` if anything was newly added. Pure — the union
/// rule for the shared `branch` array, unit-tested without a live Qdrant.
pub(crate) fn merge_branch_set(set: &mut Vec<String>, extra: &[String]) -> bool {
    let mut changed = false;
    for b in extra {
        if !set.iter().any(|x| x == b) {
            set.push(b.clone());
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::merge_branch_set;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn merges_new_branches_and_reports_change() {
        // The #224 case: the upsert left only [main]; restore the prior tags.
        let mut set = v(&["main"]);
        let changed = merge_branch_set(&mut set, &v(&["feature-a", "feature-b"]));
        assert!(changed);
        assert_eq!(set, v(&["main", "feature-a", "feature-b"]));
    }

    #[test]
    fn skips_duplicates_and_reports_no_change() {
        let mut set = v(&["main", "feature-a"]);
        let changed = merge_branch_set(&mut set, &v(&["main", "feature-a"]));
        assert!(!changed);
        assert_eq!(set, v(&["main", "feature-a"]));
    }

    #[test]
    fn partial_overlap_adds_only_the_missing() {
        let mut set = v(&["main"]);
        let changed = merge_branch_set(&mut set, &v(&["main", "feature-a"]));
        assert!(changed);
        assert_eq!(set, v(&["main", "feature-a"]));
    }

    #[test]
    fn empty_extra_is_a_noop() {
        let mut set = v(&["main"]);
        assert!(!merge_branch_set(&mut set, &[]));
        assert_eq!(set, v(&["main"]));
    }
}
