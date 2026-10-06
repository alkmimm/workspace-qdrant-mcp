//! Average-linkage agglomerative clustering, cut at a similarity threshold.
//!
//! The nightly tag hierarchy clusters every canonical tag of a tenant. The
//! first implementation kept an n×n `f64` matrix and, for every merge, scanned
//! all pairs of active clusters recomputing their average linkage from the
//! members: O(n²) memory and O(n³)–O(n⁴) time. Live 2026-10-06, DOC-V2 (31k
//! level-3 tags) took 7.7 GB and ~10.5 h every night, and while that memory
//! was held the queue processor's RSS brake paused all indexing.
//!
//! This is the nearest-neighbour-chain algorithm with Lance–Williams updates
//! over the upper triangle in `f32`: O(n²) time and n(n-1)/2 floats. Average
//! linkage is reducible, so the dendrogram — and therefore its cut at the
//! threshold — is the one the greedy "merge the most similar pair while it is
//! above the threshold" loop produced, up to exact ties.

use super::super::semantic_rerank::cosine_similarity;

/// Upper triangle of a symmetric similarity matrix, row-major, no diagonal.
struct Condensed {
    n: usize,
    sim: Vec<f32>,
}

impl Condensed {
    fn build(vectors: &[&[f32]]) -> Self {
        let n = vectors.len();
        let mut sim = Vec::with_capacity(n * n.saturating_sub(1) / 2);
        for i in 0..n {
            for j in (i + 1)..n {
                sim.push(cosine_similarity(vectors[i], vectors[j]) as f32);
            }
        }
        Self { n, sim }
    }

    fn index(&self, i: usize, j: usize) -> usize {
        let (i, j) = if i < j { (i, j) } else { (j, i) };
        i * (2 * self.n - i - 1) / 2 + (j - i - 1)
    }

    fn get(&self, i: usize, j: usize) -> f64 {
        f64::from(self.sim[self.index(i, j)])
    }

    fn set(&mut self, i: usize, j: usize, value: f64) {
        let k = self.index(i, j);
        self.sim[k] = value as f32;
    }
}

/// Bytes the similarity matrix of `n` items takes.
pub(super) fn matrix_bytes(n: usize) -> usize {
    n * n.saturating_sub(1) / 2 * std::mem::size_of::<f32>()
}

/// Clusters of `vectors` under average linkage, merging while two clusters'
/// average pairwise cosine similarity is at least `threshold`.
///
/// Each cluster lists its members ascending; clusters are ordered by their
/// smallest member (the order the greedy loop's surviving indices had).
pub(super) fn average_linkage_clusters(vectors: &[&[f32]], threshold: f64) -> Vec<Vec<usize>> {
    let n = vectors.len();
    let mut state = Linkage {
        matrix: Condensed::build(vectors),
        size: vec![1; n],
        members: (0..n).map(|i| vec![i]).collect(),
        open: vec![true; n],
    };
    let mut chain: Vec<usize> = Vec::new();
    let mut next_start = 0usize;
    let mut clusters: Vec<Vec<usize>> = Vec::new();

    loop {
        if chain.is_empty() {
            while next_start < n && !state.open[next_start] {
                next_start += 1;
            }
            if next_start == n {
                break;
            }
            chain.push(next_start);
        }
        let a = chain[chain.len() - 1];
        let prev = chain.len().checked_sub(2).map(|k| chain[k]);
        match state.nearest(a, prev) {
            Some((b, sim)) if sim >= threshold => {
                if Some(b) == prev {
                    chain.truncate(chain.len() - 2);
                    state.merge(a, b);
                } else {
                    chain.push(b);
                }
            }
            _ => {
                // Nothing open is within the threshold, and under average
                // linkage nothing ever will be (a merge averages its parts'
                // similarities), so this cluster is final.
                state.open[a] = false;
                chain.pop();
                clusters.push(std::mem::take(&mut state.members[a]));
            }
        }
    }
    for members in &mut clusters {
        members.sort_unstable();
    }
    clusters.sort_unstable_by_key(|members| members[0]);
    clusters
}

/// The clusters while they merge: a cluster is identified by the smallest
/// index it absorbed and stays `open` until it is merged away or final.
struct Linkage {
    matrix: Condensed,
    size: Vec<usize>,
    members: Vec<Vec<usize>>,
    open: Vec<bool>,
}

impl Linkage {
    /// The open cluster most similar to `a`. A tie prefers `prev`, the chain
    /// link before `a`: that is what makes the chain end in a reciprocal pair.
    fn nearest(&self, a: usize, prev: Option<usize>) -> Option<(usize, f64)> {
        let mut best = prev.map(|p| (p, self.matrix.get(a, p)));
        for c in 0..self.matrix.n {
            if c == a || !self.open[c] || Some(c) == prev {
                continue;
            }
            let sim = self.matrix.get(a, c);
            if best.is_none_or(|(_, top)| sim > top) {
                best = Some((c, sim));
            }
        }
        best
    }

    /// Merge `a` and `b` into the smaller index. Lance–Williams for average
    /// linkage: a union's similarity to any other cluster is the size-weighted
    /// mean of its parts' similarities to it.
    fn merge(&mut self, a: usize, b: usize) {
        let (keep, gone) = if a < b { (a, b) } else { (b, a) };
        let (keep_size, gone_size) = (self.size[keep] as f64, self.size[gone] as f64);
        for c in 0..self.matrix.n {
            if c == keep || c == gone || !self.open[c] {
                continue;
            }
            let sim = (keep_size * self.matrix.get(keep, c) + gone_size * self.matrix.get(gone, c))
                / (keep_size + gone_size);
            self.matrix.set(keep, c, sim);
        }
        self.size[keep] += self.size[gone];
        let absorbed = std::mem::take(&mut self.members[gone]);
        self.members[keep].extend(absorbed);
        self.open[gone] = false;
    }
}
