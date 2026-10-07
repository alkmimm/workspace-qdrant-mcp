//! Which content generations a graph read may see.
//!
//! The graph keeps one set of rows per content GENERATION of a file (its
//! `base_point`), so a path changed on a branch has one generation per
//! version. Which generations a branch holds lives in `tracked_files` (the
//! authority — see `branch_scope`); this module holds the in-memory forms the
//! graph store applies, so the store never needs `state.db` itself.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// The generations a read admits. `GraphScope::all()` admits every row and is
/// for maintenance passes that work on the whole tenant; every answer given
/// to a caller is scoped to one branch.
#[derive(Debug, Clone, Default)]
pub struct GraphScope {
    generations: Option<Arc<HashSet<String>>>,
}

impl GraphScope {
    /// Admit every generation (no branch scoping).
    pub fn all() -> Self {
        Self { generations: None }
    }

    /// Admit exactly these generations (plus the generation-less stub rows).
    pub fn generations(generations: HashSet<String>) -> Self {
        Self {
            generations: Some(Arc::new(generations)),
        }
    }

    /// Whether a row of `generation` is visible. The empty generation marks a
    /// row no file version owns (a file-less stub, or a node another file only
    /// referred to); it is visible everywhere, as before generations existed.
    pub fn admits(&self, generation: &str) -> bool {
        generation.is_empty()
            || self
                .generations
                .as_ref()
                .map_or(true, |set| set.contains(generation))
    }

    /// Whether this scope restricts anything.
    pub fn is_scoped(&self) -> bool {
        self.generations.is_some()
    }
}

/// One extracted generation and when it was extracted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedGeneration {
    pub generation: String,
    pub file_path: String,
    /// ISO-8601 UTC, as every other graph timestamp.
    pub extracted_at: String,
    /// Written by the current extractor (`GRAPH_EXTRACTOR_VERSION`). A stale
    /// extraction still answers queries and still belongs to the orphan
    /// sweep, but the backfill rebuilds it.
    pub current: bool,
}

/// The branches that hold each generation of one tenant.
///
/// Used by the stub resolver: a candidate definition only counts for an edge
/// when some branch holds both the edge's generation and the candidate's.
/// The trunk's generations are `universal`: a feature branch is tagged only on
/// the files it changed and sees the trunk's copy of the rest, so a trunk
/// generation is co-visible with every branch's.
#[derive(Debug, Clone, Default)]
pub struct GenerationBranches {
    by_generation: HashMap<String, Vec<String>>,
    universal: HashSet<String>,
}

impl GenerationBranches {
    /// No membership known: every pair of generations counts as co-visible,
    /// which is the resolver's behaviour before generations existed.
    pub fn unknown() -> Self {
        Self::default()
    }

    /// Build from `(generation, branches)` rows. A row with no branches is a
    /// legacy row visible on every branch and is left out, which makes it
    /// co-visible with everything.
    pub fn from_rows(rows: impl IntoIterator<Item = (String, Vec<String>)>) -> Self {
        let mut by_generation: HashMap<String, Vec<String>> = HashMap::new();
        for (generation, branches) in rows {
            if branches.is_empty() {
                continue;
            }
            let entry = by_generation.entry(generation).or_default();
            for b in branches {
                if !entry.contains(&b) {
                    entry.push(b);
                }
            }
        }
        Self {
            by_generation,
            universal: HashSet::new(),
        }
    }

    /// Mark generations visible on every branch (the trunk's).
    pub fn with_universal(mut self, universal: HashSet<String>) -> Self {
        self.universal = universal;
        self
    }

    /// Whether some branch holds both generations. Unknown membership on
    /// either side (a stub's empty generation, a legacy row, a generation the
    /// rows did not mention) answers `true` — never narrower than before.
    pub fn co_visible(&self, a: &str, b: &str) -> bool {
        if a == b || self.universal.contains(a) || self.universal.contains(b) {
            return true;
        }
        match (self.by_generation.get(a), self.by_generation.get(b)) {
            (Some(x), Some(y)) => x.iter().any(|branch| y.contains(branch)),
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unscoped_admits_everything_and_scoped_admits_its_set_plus_stubs() {
        let all = GraphScope::all();
        assert!(all.admits("g1") && all.admits(""));
        assert!(!all.is_scoped());

        let scoped = GraphScope::generations(["g1".to_string()].into_iter().collect());
        assert!(scoped.is_scoped());
        assert!(scoped.admits("g1"));
        assert!(
            !scoped.admits("g2"),
            "a generation the branch does not hold"
        );
        assert!(scoped.admits(""), "stub rows belong to no version");
    }

    #[test]
    fn an_empty_scope_admits_only_stub_rows() {
        let empty = GraphScope::generations(HashSet::new());
        assert!(!empty.admits("g1"));
        assert!(empty.admits(""));
    }

    #[test]
    fn co_visible_requires_a_shared_branch() {
        let m = GenerationBranches::from_rows([
            ("dev".to_string(), vec!["develop".to_string()]),
            ("f5".to_string(), vec!["fase-5".to_string()]),
            (
                "both".to_string(),
                vec!["develop".to_string(), "fase-5".to_string()],
            ),
        ]);
        assert!(!m.co_visible("dev", "f5"), "no branch holds both versions");
        assert!(m.co_visible("both", "dev"));
        assert!(m.co_visible("both", "f5"));
        assert!(m.co_visible("dev", "dev"));
    }

    #[test]
    fn a_trunk_generation_is_co_visible_with_every_branch() {
        let m = GenerationBranches::from_rows([
            ("dev".to_string(), vec!["develop".to_string()]),
            ("f5".to_string(), vec!["fase-5".to_string()]),
        ])
        .with_universal(["dev".to_string()].into_iter().collect());
        assert!(m.co_visible("f5", "dev"));
    }

    #[test]
    fn unknown_membership_never_narrows() {
        let m = GenerationBranches::from_rows([
            ("dev".to_string(), vec!["develop".to_string()]),
            ("legacy".to_string(), vec![]),
        ]);
        assert!(m.co_visible("dev", ""), "stub side");
        assert!(m.co_visible("dev", "legacy"), "a row with no branch set");
        assert!(m.co_visible("dev", "never-seen"));
        assert!(GenerationBranches::unknown().co_visible("a", "b"));
    }
}
