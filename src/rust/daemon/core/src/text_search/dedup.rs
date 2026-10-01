//! One identity rule for a served text-search hit, shared by every engine.
//!
//! A path holds several *content generations* in `search.db`: Layer 2 keys
//! `code_lines` by `file_id`, one `file_id` per distinct content, and the
//! `file_metadata` rows of different generations can carry overlapping branch
//! sets. A scoped query therefore streams one row per *generation*, while the
//! caller only ever sees one hit per `(file_path, line_number, content)` — the
//! MCP read surfaces collapse the repeats again on arrival.
//!
//! Counting rows where the caller counts hits is what let `total_matches`
//! overstate the real surface (measured: 4033 reported for a pattern whose true
//! deduped total was 2033). So the engines dedupe HERE, before the
//! `max_results` cap, which keeps three promises at once:
//!
//! * [`SearchResults::matches`](super::types::SearchResults) holds up to
//!   `max_results` *real* hits instead of padding a page with repeats;
//! * `truncated` describes the distinct set, not the row stream;
//! * the gRPC `total_matches` (the pre-cap `matches.len()`) counts hits, so the
//!   MCP server and `wqm project search` can report an exact number.
//!
//! Deduping at the source rather than in one caller is deliberate: `grep`,
//! `search exact:true`, `list`, `retrieve` and the CLI all consume these
//! results, and a fix applied in only one of them is the drift this module
//! exists to prevent.

use std::collections::HashSet;

/// Collapses rows that describe the same served hit.
///
/// The identity is the `(file_path, line_number, content)` triple — the same key
/// the MCP read surfaces use. Two generations that agree on all three are the
/// same hit to a reader and collapse; generations whose content genuinely
/// diverges stay distinct, because they are different lines of code.
#[derive(Debug, Default)]
pub struct MatchDeduper {
    seen: HashSet<(String, i64, String)>,
}

impl MatchDeduper {
    /// A deduper with no hits recorded yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a hit, returning `true` the first time this identity is seen and
    /// `false` for every repeat (so callers can `continue` on `false`).
    pub fn accept(&mut self, file_path: &str, line_number: i64, content: &str) -> bool {
        self.seen
            .insert((file_path.to_string(), line_number, content.to_string()))
    }

    /// How many distinct hits have been accepted.
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// Whether no hit has been accepted yet.
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

/// Drop repeated paths from a scan list, keeping the first occurrence (and so
/// the caller's `ORDER BY file_path`).
///
/// The grep engine resolves a file list from `file_metadata` and then scans each
/// entry on disk. Several generations of one path are several rows there, so
/// without this the same file is scanned — and every match in it reported —
/// once per generation.
pub fn retain_first_by_path<T>(items: &mut Vec<T>, path_of: impl Fn(&T) -> &str) {
    let mut seen: HashSet<String> = HashSet::new();
    items.retain(|item| seen.insert(path_of(item).to_string()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_is_true_once_per_identity() {
        let mut d = MatchDeduper::new();
        assert!(d.accept("/a/b.rs", 10, "pub fn x()"));
        assert!(!d.accept("/a/b.rs", 10, "pub fn x()"));
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn divergent_content_on_same_line_stays_distinct() {
        // Two generations whose content really differs are two different lines
        // of code, not a repeat — the reader must see both.
        let mut d = MatchDeduper::new();
        assert!(d.accept("/a/b.rs", 10, "pub fn x()"));
        assert!(d.accept("/a/b.rs", 10, "pub fn x(y: u8)"));
        assert_eq!(d.len(), 2);
    }

    #[test]
    fn same_content_on_different_lines_stays_distinct() {
        let mut d = MatchDeduper::new();
        assert!(d.accept("/a/b.rs", 10, "pub fn x()"));
        assert!(d.accept("/a/b.rs", 11, "pub fn x()"));
        assert_eq!(d.len(), 2);
    }

    #[test]
    fn same_line_in_different_files_stays_distinct() {
        let mut d = MatchDeduper::new();
        assert!(d.accept("/a/b.rs", 10, "pub fn x()"));
        assert!(d.accept("/a/c.rs", 10, "pub fn x()"));
        assert_eq!(d.len(), 2);
    }

    #[test]
    fn new_deduper_is_empty() {
        let d = MatchDeduper::new();
        assert!(d.is_empty());
        assert_eq!(d.len(), 0);
    }

    #[test]
    fn three_generations_of_one_line_collapse_to_one() {
        // The shape measured in search.db: one path with three generations, each
        // carrying the same line. Counting rows said 3; the reader sees 1.
        let mut d = MatchDeduper::new();
        let rows = [
            ("/repo/mod.rs", 42, "pub fn redact_for_path()"),
            ("/repo/mod.rs", 42, "pub fn redact_for_path()"),
            ("/repo/mod.rs", 42, "pub fn redact_for_path()"),
        ];
        let kept = rows.iter().filter(|(p, l, c)| d.accept(p, *l, c)).count();
        assert_eq!(kept, 1, "three generations of one line are one hit");
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn retain_first_by_path_keeps_order_and_first_row() {
        let mut items = vec![
            ("/a.rs", "gen1"),
            ("/b.rs", "gen1"),
            ("/a.rs", "gen2"),
            ("/c.rs", "gen1"),
            ("/b.rs", "gen2"),
        ];
        retain_first_by_path(&mut items, |(p, _)| p);
        assert_eq!(
            items,
            vec![("/a.rs", "gen1"), ("/b.rs", "gen1"), ("/c.rs", "gen1")],
            "first occurrence of each path survives, order preserved"
        );
    }

    #[test]
    fn retain_first_by_path_is_a_noop_without_repeats() {
        let mut items = vec![("/a.rs", 1), ("/b.rs", 2)];
        retain_first_by_path(&mut items, |(p, _)| p);
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn retain_first_by_path_handles_empty() {
        let mut items: Vec<(&str, u8)> = vec![];
        retain_first_by_path(&mut items, |(p, _)| p);
        assert!(items.is_empty());
    }
}
