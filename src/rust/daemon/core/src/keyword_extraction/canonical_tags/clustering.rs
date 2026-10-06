//! Clustering algorithms for canonical tag deduplication and hierarchy building.

use tracing::warn;

use super::super::semantic_rerank::cosine_similarity;
use super::linkage::{average_linkage_clusters, matrix_bytes};
use super::types::{CanonicalTag, TagWithVector};

/// Most bytes one level's similarity matrix may take. Past it only the most
/// documented tags are clustered and the rest stay singleton clusters: the
/// long tail of the hierarchy degrades instead of the daemon's memory. 2 GiB
/// clusters ~32k tags at once (DOC-V2 had 31k level-3 tags on 2026-10-06) and
/// keeps the daemon well under its 8 GiB RSS brake (`WQM_MAX_RSS_MB`).
pub(super) const MAX_CLUSTER_MATRIX_BYTES: usize = 2 << 30;

/// Merge near-duplicate tags using single-linkage clustering.
///
/// Tags with cosine similarity > threshold are merged. The label closest
/// to the centroid is chosen as the canonical name.
pub(super) fn merge_duplicates(tags: &[TagWithVector], threshold: f64) -> Vec<CanonicalTag> {
    let n = tags.len();
    let mut cluster_id: Vec<Option<usize>> = vec![None; n];
    let mut clusters: Vec<Vec<usize>> = Vec::new();

    // Greedy merging: for each unassigned tag, find all similar tags
    for i in 0..n {
        if cluster_id[i].is_some() {
            continue;
        }

        let cid = clusters.len();
        let mut members = vec![i];
        cluster_id[i] = Some(cid);

        for j in (i + 1)..n {
            if cluster_id[j].is_some() {
                continue;
            }
            let sim = cosine_similarity(&tags[i].vector, &tags[j].vector);
            if sim > threshold {
                members.push(j);
                cluster_id[j] = Some(cid);
            }
        }

        clusters.push(members);
    }

    // Build canonical tags from clusters
    clusters
        .iter()
        .map(|members| {
            let centroid = compute_centroid(members.iter().map(|&i| &tags[i].vector));
            let total_docs: u32 = members.iter().map(|&i| tags[i].doc_count).sum();

            // Choose label: phrase closest to centroid
            let label_idx = members
                .iter()
                .max_by(|&&a, &&b| {
                    let sim_a = cosine_similarity(&tags[a].vector, &centroid);
                    let sim_b = cosine_similarity(&tags[b].vector, &centroid);
                    sim_a
                        .partial_cmp(&sim_b)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .copied()
                .unwrap_or(members[0]);

            let label = tags[label_idx].phrase.clone();
            let aliases: Vec<String> = members
                .iter()
                .filter(|&&i| i != label_idx)
                .map(|&i| tags[i].phrase.clone())
                .collect();

            CanonicalTag {
                label,
                aliases,
                centroid,
                doc_count: total_docs,
                level: 3,
                parent_index: None,
                parent_similarity: None,
            }
        })
        .collect()
}

/// Cluster canonical tags into higher-level groups using average-linkage.
///
/// Sets `parent_index` on each input tag to point to the index of its
/// parent cluster in the returned output vector.
pub(super) fn cluster_tags(
    tags: &mut [CanonicalTag],
    threshold: f64,
    level: u8,
) -> Vec<CanonicalTag> {
    cluster_tags_within(tags, threshold, level, MAX_CLUSTER_MATRIX_BYTES)
}

/// [`cluster_tags`] with an explicit memory budget for the similarity matrix.
pub(super) fn cluster_tags_within(
    tags: &mut [CanonicalTag],
    threshold: f64,
    level: u8,
    budget_bytes: usize,
) -> Vec<CanonicalTag> {
    if tags.is_empty() {
        return Vec::new();
    }
    let clustered = clustered_subset(tags, budget_bytes, level);
    let vectors: Vec<&[f32]> = clustered
        .iter()
        .map(|&i| tags[i].centroid.as_slice())
        .collect();
    let mut groups: Vec<Vec<usize>> = average_linkage_clusters(&vectors, threshold)
        .into_iter()
        .map(|group| group.into_iter().map(|k| clustered[k]).collect())
        .collect();
    if clustered.len() < tags.len() {
        let mut in_matrix = vec![false; tags.len()];
        for &i in &clustered {
            in_matrix[i] = true;
        }
        groups.extend((0..tags.len()).filter(|&i| !in_matrix[i]).map(|i| vec![i]));
        groups.sort_unstable_by_key(|group| group[0]);
    }
    build_cluster_output(tags, &groups, level)
}

/// The tags clustered at this level, ascending: all of them when their matrix
/// fits `budget_bytes`, else the most documented that fit.
fn clustered_subset(tags: &[CanonicalTag], budget_bytes: usize, level: u8) -> Vec<usize> {
    let cap = largest_fitting(budget_bytes);
    if tags.len() <= cap {
        return (0..tags.len()).collect();
    }
    warn!(
        hierarchy_level = level,
        tags = tags.len(),
        clustered = cap,
        "Tag hierarchy: the similarity matrix would exceed its memory budget; clustering the {} most-documented tags, the rest stay singleton clusters",
        cap
    );
    let mut by_docs: Vec<usize> = (0..tags.len()).collect();
    by_docs.sort_by(|&a, &b| tags[b].doc_count.cmp(&tags[a].doc_count).then(a.cmp(&b)));
    by_docs.truncate(cap);
    by_docs.sort_unstable();
    by_docs
}

/// The largest item count whose similarity matrix fits `budget_bytes`.
pub(super) fn largest_fitting(budget_bytes: usize) -> usize {
    let (mut lo, mut hi) = (1usize, 1usize << 20);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if matrix_bytes(mid) <= budget_bytes {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

/// Build canonical-tag output from cluster groups (member indices into `tags`),
/// setting parent links on the input tags.
fn build_cluster_output(
    tags: &mut [CanonicalTag],
    groups: &[Vec<usize>],
    level: u8,
) -> Vec<CanonicalTag> {
    let mut result = Vec::new();

    for members in groups {
        if members.is_empty() {
            continue;
        }

        let centroid = compute_centroid(members.iter().map(|&m| &tags[m].centroid));
        let total_docs: u32 = members.iter().map(|&m| tags[m].doc_count).sum();

        let label_idx = closest_to_centroid(members, tags, &centroid);

        let label = tags[label_idx].label.clone();
        let aliases: Vec<String> = members
            .iter()
            .filter(|&&m| m != label_idx)
            .map(|&m| tags[m].label.clone())
            .collect();

        let parent_idx = result.len();
        for &m in members {
            tags[m].parent_index = Some(parent_idx);
            tags[m].parent_similarity = Some(cosine_similarity(&tags[m].centroid, &centroid));
        }

        result.push(CanonicalTag {
            label,
            aliases,
            centroid,
            doc_count: total_docs,
            level,
            parent_index: None,
            parent_similarity: None,
        });
    }

    result
}

/// Find the member whose centroid is closest to the given centroid.
fn closest_to_centroid(members: &[usize], tags: &[CanonicalTag], centroid: &[f32]) -> usize {
    members
        .iter()
        .max_by(|&&a, &&b| {
            let sim_a = cosine_similarity(&tags[a].centroid, centroid);
            let sim_b = cosine_similarity(&tags[b].centroid, centroid);
            sim_a
                .partial_cmp(&sim_b)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .copied()
        .unwrap_or(members[0])
}

/// Compute centroid (mean) of a set of vectors.
pub(super) fn compute_centroid<'a>(vectors: impl Iterator<Item = &'a Vec<f32>>) -> Vec<f32> {
    let vecs: Vec<&Vec<f32>> = vectors.collect();
    if vecs.is_empty() {
        return Vec::new();
    }

    let dim = vecs[0].len();
    let mut centroid = vec![0.0f32; dim];
    let n = vecs.len() as f32;

    for v in &vecs {
        if v.len() != dim {
            continue;
        }
        for (i, val) in v.iter().enumerate() {
            centroid[i] += val;
        }
    }

    for val in &mut centroid {
        *val /= n;
    }

    centroid
}
