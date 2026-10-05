//! Branch-prune policy: which tracked branch is the project's corpus (guard 3),
//! and how often the prune runs.

use std::time::Duration;

/// Elect the project's corpus branch from per-branch distinct-file counts: the
/// elected branch is never pruned (guard 3).
///
/// Guard 3 exists for a corpus indexed under a label git does not have (the
/// example-service / example-tool incident: everything tagged with a fallback
/// "main" while git had master/dev-clean). There HEAD's own branch holds
/// almost nothing. When HEAD holds at least half as many files as the largest
/// branch, HEAD IS the corpus — and a larger branch is an offshoot, not a
/// mislabel: a linked worktree carries every file of the trunk plus its own new
/// ones, so a deleted worktree branch out-counts HEAD by exactly those. Measured
/// 2026-10-05 on DOC-V2: `feat/time-clock-mobile-only`, deleted in git, held
/// 5,280 files to `main`'s 5,260 and was protected forever. HEAD is a live
/// branch (guard 2 checked), so electing it protects no dead branch.
///
/// Otherwise the largest branch wins, with ties decided deliberately, never by
/// `max_by_key` over an unordered `GROUP BY` — which branch is PROTECTED must
/// not depend on SQLite's row order between boots:
/// 1. HEAD wins a tie — a checked-out branch is the corpus by definition.
/// 2. Otherwise the lexicographically first name wins: an arbitrary but STABLE
///    choice, so the same project protects the same branch on every boot.
pub(crate) fn elect_primary<'a>(counts: &'a [(String, i64)], head: &str) -> Option<&'a str> {
    let max = counts.iter().map(|(_, n)| *n).max()?;
    let head_entry = counts.iter().find(|(b, _)| b == head);
    if let Some((b, n)) = head_entry {
        if n.saturating_mul(2) >= max {
            return Some(b.as_str());
        }
    }
    let mut tied: Vec<&'a str> = counts
        .iter()
        .filter(|(_, n)| *n == max)
        .map(|(b, _)| b.as_str())
        .collect();
    tied.sort_unstable();
    tied.into_iter().next()
}

/// Time between prune cycles after the start-up one; `None` = start-up only.
///
/// Each cycle enqueues at most `WQM_BRANCH_PRUNE_COVERED_CAP` covered deletes,
/// and until 2026-10-05 the only cycle ran at daemon start — so the backlog
/// grew with every deleted worktree branch: 74,846 deferred candidates and
/// ~2.3k graphless versions held only by deleted branches, of which each
/// restart drained ~40. Repeating the cycle keeps the per-cycle blast radius
/// the cap exists for and drains at the cap's rate instead.
///
/// `WQM_BRANCH_PRUNE_INTERVAL_SECS`: default 900, `0` = start-up only, floor 60.
pub fn prune_interval() -> Option<Duration> {
    prune_interval_from(
        std::env::var("WQM_BRANCH_PRUNE_INTERVAL_SECS")
            .ok()
            .as_deref(),
    )
}

fn prune_interval_from(raw: Option<&str>) -> Option<Duration> {
    const DEFAULT_SECS: u64 = 900;
    const FLOOR_SECS: u64 = 60;
    let secs = match raw.map(str::trim).filter(|v| !v.is_empty()) {
        None => DEFAULT_SECS,
        // An unparsable value keeps the default: a typo must not silently
        // switch the drain off.
        Some(v) => match v.parse::<u64>() {
            Ok(0) => return None,
            Ok(n) => n.max(FLOOR_SECS),
            Err(_) => DEFAULT_SECS,
        },
    };
    Some(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(pairs: &[(&str, i64)]) -> Vec<(String, i64)> {
        pairs.iter().map(|(b, n)| (b.to_string(), *n)).collect()
    }

    // ── guard 3: corpus election (#224) ───────────────────────────────────

    #[test]
    fn corpus_election_ignores_generation_debris() {
        // The live deadlock this fix exists for: a DEAD branch accumulated stale
        // generations and out-counted the real corpus in ROWS, protecting itself
        // forever. Counting distinct FILES elects `main` — the counts here are
        // what the fixed query returns (paths, not rows).
        let c = counts(&[
            ("fix/is-test-lookup-relative-path", 1859),
            ("main", 1869),
            ("codex/linux-codex-register", 2),
        ]);
        assert_eq!(elect_primary(&c, "main"), Some("main"));
    }

    #[test]
    fn corpus_election_still_protects_a_mislabeled_corpus() {
        // The failure mode guard 3 was BORN for (example-service / example-tool,
        // whose whole corpus was indexed under a bogus label): the mislabeled
        // branch holds ALL of the project's files, so it must stay the elected
        // corpus and thus stay unprunable. Counting files instead of rows must
        // not weaken this.
        let c = counts(&[("bogus-main", 3000), ("dev-clean", 12), ("feat/x", 3)]);
        assert_eq!(elect_primary(&c, "dev-clean"), Some("bogus-main"));
    }

    #[test]
    fn corpus_election_breaks_ties_with_head_then_deterministically() {
        // HEAD wins a tie: a checked-out branch IS the corpus.
        let c = counts(&[("feat/b", 100), ("main", 100), ("feat/a", 100)]);
        assert_eq!(elect_primary(&c, "main"), Some("main"));

        // No HEAD among the tied → stable, order-independent choice. The input
        // order comes from an unordered GROUP BY, so the same tie must elect the
        // same branch on every boot (which branch is PROTECTED cannot flap).
        let c1 = counts(&[("feat/b", 100), ("feat/a", 100)]);
        let c2 = counts(&[("feat/a", 100), ("feat/b", 100)]);
        assert_eq!(elect_primary(&c1, "main"), Some("feat/a"));
        assert_eq!(
            elect_primary(&c1, "main"),
            elect_primary(&c2, "main"),
            "row order must not decide which branch is protected"
        );
    }

    #[test]
    fn corpus_election_handles_empty_and_single() {
        assert_eq!(elect_primary(&[], "main"), None);
        let c = counts(&[("only", 7)]);
        assert_eq!(elect_primary(&c, "main"), Some("only"));
    }

    #[test]
    fn corpus_election_does_not_protect_a_worktree_offshoot() {
        // Live 2026-10-05: a deleted worktree branch = the trunk's files + its
        // own new ones, so it out-counts HEAD — and was protected forever.
        let c = counts(&[("feat/time-clock-mobile-only", 5280), ("main", 5260)]);
        assert_eq!(elect_primary(&c, "main"), Some("main"));
        // At exactly half, HEAD is still the corpus; below half it is not.
        let half = counts(&[("big", 200), ("main", 100)]);
        assert_eq!(elect_primary(&half, "main"), Some("main"));
        let below = counts(&[("big", 201), ("main", 100)]);
        assert_eq!(elect_primary(&below, "main"), Some("big"));
    }

    // ── cadence ───────────────────────────────────────────────────────────

    #[test]
    fn prune_interval_defaults_floors_and_can_be_switched_off() {
        let secs = |raw| prune_interval_from(raw).map(|d| d.as_secs());
        assert_eq!(secs(None), Some(900));
        assert_eq!(secs(Some("")), Some(900));
        assert_eq!(secs(Some("1800")), Some(1800));
        assert_eq!(secs(Some("5")), Some(60), "floored");
        assert_eq!(secs(Some("0")), None, "start-up only");
        assert_eq!(secs(Some("15m")), Some(900), "a typo keeps the default");
    }
}
