//! The nearest-neighbour chain against the greedy loop it replaced, the
//! memory budget, and the scale the nightly rebuild needs.

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use super::super::semantic_rerank::cosine_similarity;
use super::clustering::{cluster_tags_within, largest_fitting, MAX_CLUSTER_MATRIX_BYTES};
use super::linkage::{average_linkage_clusters, matrix_bytes};
use super::types::CanonicalTag;

/// Average pairwise similarity between two clusters: the greedy's linkage.
fn average_linkage_sim(cluster_a: &[usize], cluster_b: &[usize], sim: &[Vec<f64>]) -> f64 {
    let total: f64 = cluster_a
        .iter()
        .flat_map(|&a| cluster_b.iter().map(move |&b| sim[a][b]))
        .sum();
    total / (cluster_a.len() * cluster_b.len()) as f64
}

/// The clustering the chain replaced, kept as the reference: merge the most
/// similar pair of clusters while its average linkage is at least the
/// threshold. O(n²) memory and O(n³) or worse time — test sizes only.
fn greedy_reference(vectors: &[Vec<f32>], threshold: f64) -> Vec<Vec<usize>> {
    let n = vectors.len();
    // The same f32 rounding the chain's matrix applies, so both see the same numbers.
    let sim: Vec<Vec<f64>> = (0..n)
        .map(|i| {
            (0..n)
                .map(|j| f64::from(cosine_similarity(&vectors[i], &vectors[j]) as f32))
                .collect()
        })
        .collect();
    let mut active = vec![true; n];
    let mut members: Vec<Vec<usize>> = (0..n).map(|i| vec![i]).collect();
    loop {
        let mut best: Option<(usize, usize, f64)> = None;
        for i in (0..n).filter(|&i| active[i]) {
            for j in ((i + 1)..n).filter(|&j| active[j]) {
                let linkage = average_linkage_sim(&members[i], &members[j], &sim);
                if best.is_none_or(|(_, _, top)| linkage > top) {
                    best = Some((i, j, linkage));
                }
            }
        }
        match best {
            Some((i, j, linkage)) if linkage >= threshold => {
                let absorbed = std::mem::take(&mut members[j]);
                members[i].extend(absorbed);
                active[j] = false;
            }
            _ => break,
        }
    }
    let mut clusters: Vec<Vec<usize>> = (0..n)
        .filter(|&i| active[i])
        .map(|i| {
            let mut group = members[i].clone();
            group.sort_unstable();
            group
        })
        .collect();
    clusters.sort_unstable_by_key(|group| group[0]);
    clusters
}

/// `n` vectors scattered around `centers` random centres.
fn clustered_vectors(
    rng: &mut StdRng,
    n: usize,
    dim: usize,
    centers: usize,
    noise: f32,
) -> Vec<Vec<f32>> {
    let centres: Vec<Vec<f32>> = (0..centers)
        .map(|_| (0..dim).map(|_| rng.gen_range(-1.0..1.0)).collect())
        .collect();
    (0..n)
        .map(|_| {
            let centre = &centres[rng.gen_range(0..centers)];
            centre
                .iter()
                .map(|x| x + rng.gen_range(-noise..noise))
                .collect()
        })
        .collect()
}

fn as_slices(vectors: &[Vec<f32>]) -> Vec<&[f32]> {
    vectors.iter().map(Vec::as_slice).collect()
}

fn canonical(label: &str, centroid: Vec<f32>, doc_count: u32) -> CanonicalTag {
    CanonicalTag {
        label: label.to_string(),
        aliases: Vec::new(),
        centroid,
        doc_count,
        level: 3,
        parent_index: None,
        parent_similarity: None,
    }
}

#[test]
fn the_reference_linkage_averages_every_member_pair() {
    let sim = vec![
        vec![1.0, 0.8, 0.1],
        vec![0.8, 1.0, 0.2],
        vec![0.1, 0.2, 1.0],
    ];
    // (0.1 + 0.2) / 2
    assert!((average_linkage_sim(&[0, 1], &[2], &sim) - 0.15).abs() < 1e-9);
}

/// Average linkage is reducible, so the chain's dendrogram is the greedy
/// loop's and both cut the same clusters at any threshold.
#[test]
fn the_chain_cuts_the_same_clusters_as_the_greedy_loop() {
    let mut rng = StdRng::seed_from_u64(20_261_006);
    let mut compared = 0;
    for case in 0..60 {
        let n = rng.gen_range(1..70);
        let dim = [4, 8, 16][case % 3];
        let centers = rng.gen_range(1..8);
        let noise = [0.2, 0.5, 1.0][case % 3];
        let vectors = clustered_vectors(&mut rng, n, dim, centers, noise);
        for threshold in [0.3, 0.5, 0.7, 0.9] {
            assert_eq!(
                average_linkage_clusters(&as_slices(&vectors), threshold),
                greedy_reference(&vectors, threshold),
                "case {case}: n={n}, dim={dim}, threshold={threshold}"
            );
            compared += 1;
        }
    }
    assert_eq!(compared, 240);
}

#[test]
fn past_the_budget_only_the_most_documented_tags_are_clustered() {
    let fresh = || {
        vec![
            canonical("a", vec![1.0, 0.0, 0.0], 9),
            canonical("b", vec![0.99, 0.1, 0.0], 8),
            canonical("c", vec![0.0, 1.0, 0.0], 1),
            canonical("d", vec![0.0, 0.99, 0.1], 2),
            canonical("e", vec![0.0, 0.0, 1.0], 7),
        ]
    };
    let room_for_three = matrix_bytes(3);
    assert_eq!(largest_fitting(room_for_three), 3);

    // a, b and e are the most documented; c and d are never compared, so they
    // stay apart although they are near-identical.
    let mut tags = fresh();
    let parents = cluster_tags_within(&mut tags, 0.7, 2, room_for_three);
    let parent = |tags: &[CanonicalTag], i: usize| tags[i].parent_index.unwrap();
    assert_eq!(parents.len(), 4);
    assert_eq!(parent(&tags, 0), parent(&tags, 1));
    assert_ne!(parent(&tags, 2), parent(&tags, 3));
    assert!(tags.iter().all(|t| t.parent_index.is_some()));

    // With room for everyone, c and d merge too.
    let mut tags = fresh();
    let parents = cluster_tags_within(&mut tags, 0.7, 2, MAX_CLUSTER_MATRIX_BYTES);
    assert_eq!(parents.len(), 3);
    assert_eq!(parent(&tags, 2), parent(&tags, 3));
}

/// DOC-V2 had 31,085 level-3 tags on 2026-10-06: they must fit the budget.
#[test]
fn the_budget_holds_a_tenant_the_size_of_doc_v2() {
    assert_eq!(largest_fitting(MAX_CLUSTER_MATRIX_BYTES), 32_768);
    assert!(matrix_bytes(31_085) <= MAX_CLUSTER_MATRIX_BYTES);
}

/// The greedy loop needed ~10.5 h for DOC-V2's 31k tags. The chain is
/// quadratic: a few thousand cluster in well under a minute even unoptimised.
#[test]
fn a_few_thousand_tags_cluster_quickly() {
    let mut rng = StdRng::seed_from_u64(7);
    let vectors = clustered_vectors(&mut rng, 3_000, 32, 40, 0.6);
    let started = std::time::Instant::now();
    let clusters = average_linkage_clusters(&as_slices(&vectors), 0.7);
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(60),
        "took {elapsed:?}"
    );
    assert_eq!(
        clusters.iter().map(Vec::len).sum::<usize>(),
        3_000,
        "every tag lands in exactly one cluster"
    );
    assert!(clusters.len() < 3_000, "something merged");
}
