//! The reliability caveat a finished test-gaps report carries when its
//! numbers measure the extractor rather than the tests.

use super::LanguageCoverage;

/// Coverage ratio below which a report that HAS test nodes is treated as a
/// failed measurement rather than as a finding.
///
/// Field feedback (v0-bws-training audit, 2026-08): a Next.js/TS repo with 183
/// Jest test files reported 21 of 3554 symbols covered (0.6%) and ranked
/// demonstrably-tested functions as the top gaps. The tests resolve their
/// subjects through a DI container and import via an `@/` path alias, so no
/// `CALLS` edge test→production was ever extracted. At that ratio the ranking
/// is noise, and presenting it as a finding is worse than presenting nothing.
///
/// 5% is deliberately far below any real-world floor: this repo's own graph
/// measures ~28%, and even a lightly-tested codebase clears 5% once its test
/// edges resolve at all. Anything under it indicates the extractor, not the
/// test suite.
pub(super) const IMPLAUSIBLE_COVERAGE_RATIO: f64 = 0.05;

/// Build the reliability caveat for a finished report, or `None` when the
/// numbers are trustworthy.
///
/// Deliberately silent when `test_nodes == 0`: a project with no test code
/// genuinely has 0% coverage, and that is a finding, not a malfunction. The
/// warning fires only on the contradiction — tests are present in the graph,
/// yet they reach almost no production symbol.
pub(super) fn build_reliability_warning(
    total_production: u32,
    covered: u32,
    test_nodes: u32,
    by_language: &[LanguageCoverage],
) -> Option<String> {
    if test_nodes == 0 || total_production == 0 {
        return None;
    }
    let ratio = f64::from(covered) / f64::from(total_production);
    if ratio >= IMPLAUSIBLE_COVERAGE_RATIO {
        // The GLOBAL figure is plausible — but an average hides a language whose
        // edges did not resolve at all. Apply the SAME already-calibrated floor
        // per language rather than inventing a second threshold: "a real test
        // suite does not measure this low" is exactly as true of one language as
        // of a whole repo, and a polyglot repo can look healthy overall while one
        // stack is entirely unmeasured. Requires the language to actually have
        // test symbols, so an untested module stays an honest finding.
        return language_reliability_warning(by_language, ratio);
    }
    Some(format!(
        "UNRELIABLE: {test_nodes} test symbols are indexed, yet only {covered} of \
         {total_production} production symbols ({:.1}%) are reachable from them. A real test \
         suite does not measure this low — the test->production edges almost certainly failed \
         to resolve. Common causes: subjects wired through a DI container, imports via a path \
         alias the extractor does not follow, dynamic dispatch, or mocking that replaces the \
         call entirely. Treat the gap ranking below as noise, not as a list of untested code, \
         and confirm with a real coverage tool.",
        ratio * 100.0
    ))
}

/// Minimum production symbols before a language's ratio is worth judging — below
/// this a handful of unresolved edges swings the percentage wildly.
const LANGUAGE_MIN_PRODUCTION: u32 = 50;

/// Fraction of the repo's best-measuring language below which another language is
/// treated as under-EXTRACTED rather than under-tested.
///
/// The absolute floor above only catches TOTAL failure. A PARTIAL one — a language
/// whose edges resolve for some idioms and not others — sits comfortably above 5%
/// and passes in silence, while its unresolved symbols still dominate the ranking
/// an agent reads. Both known cases were silent under the absolute floor alone
/// (measured 2026-09-08):
///
/// - DOC-V2: Dart 20.5% against Java 52.2%, overall 30.9% — no warning.
/// - this repo: TypeScript 13.0% against Rust 44.7%, overall 37.7% — no warning.
///
/// 0.5 rather than 0.4: at 0.4 the DOC-V2 threshold lands on 20.9% against Dart's
/// 20.5%, far too thin to hang a calibration on. At 0.5 neither repo gains a flag
/// it did not deserve — every other language in both is either already caught by
/// the absolute floor or below [`LANGUAGE_MIN_PRODUCTION`].
const RELATIVE_COVERAGE_FLOOR: f64 = 0.5;

/// Warn when a language measures far below what this repo demonstrably achieves,
/// while the repo as a whole looks fine. That is the shape a global ratio cannot
/// express: the gaps are concentrated in the blind language, so they dominate the
/// ranking, while the healthy languages keep the average up.
///
/// Two bars, because there are two failure shapes. TOTAL failure trips the
/// absolute floor. PARTIAL failure only shows as divergence from the repo's own
/// best language — which is the right yardstick, since it is measured under the
/// same extractor, the same edge types and the same conventions.
fn language_reliability_warning(
    by_language: &[LanguageCoverage],
    overall_ratio: f64,
) -> Option<String> {
    fn ratio_of(lang: &LanguageCoverage) -> f64 {
        f64::from(lang.covered) / f64::from(lang.production)
    }
    fn judgeable(lang: &&LanguageCoverage) -> bool {
        lang.production >= LANGUAGE_MIN_PRODUCTION && lang.test_nodes > 0
    }

    // Baseline is the best-measuring JUDGEABLE language. With only one such
    // language the relative rule is inert — nothing can sit below half of itself —
    // which is correct: a lone language has nothing to diverge from.
    let best = by_language
        .iter()
        .filter(judgeable)
        .map(ratio_of)
        .fold(0.0_f64, f64::max);
    let relative_floor = best * RELATIVE_COVERAGE_FLOOR;

    let blind: Vec<(&LanguageCoverage, f64)> = by_language
        .iter()
        .filter(judgeable)
        .filter_map(|lang| {
            let r = ratio_of(lang);
            (r < IMPLAUSIBLE_COVERAGE_RATIO || r < relative_floor).then_some((lang, r))
        })
        .collect();
    if blind.is_empty() {
        return None;
    }
    let detail = blind
        .iter()
        .map(|(lang, r)| {
            format!(
                "{} ({:.1}%: {} of {} production symbols covered, {} test symbols indexed)",
                lang.extension,
                r * 100.0,
                lang.covered,
                lang.production,
                lang.test_nodes
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    Some(format!(
        "PARTIALLY UNRELIABLE: overall coverage is {:.1}%, which looks plausible, but these \
         languages measure far lower while HAVING indexed tests: {detail}. The bar is {:.0}% \
         absolute, or {:.0}% of this repo's best-measuring language ({:.1}%) — a language does \
         not fall this far behind its siblings, measured by the same extractor under the same \
         conventions, because it is merely less tested. Its test->production edges most likely \
         failed to resolve for some idiom, so its symbols are over-represented in the ranking \
         below. Known blind spot: an idiom that REFERENCES a symbol without invoking it produces \
         no edge (e.g. Flutter's `find.byType(Widget)` asserts on a type without constructing \
         it), so the most-asserted primitives can rank as the most critical gaps. Judge each \
         language on its own row in coverage_by_language, and confirm with a real coverage tool.",
        overall_ratio * 100.0,
        IMPLAUSIBLE_COVERAGE_RATIO * 100.0,
        RELATIVE_COVERAGE_FLOOR * 100.0,
        best * 100.0
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The threshold is a floor on the RATIO, not on the absolute count: the
    /// boundary is inclusive, so exactly 5% is still trusted.
    #[test]
    fn threshold_boundary_is_inclusive() {
        assert!(
            build_reliability_warning(100, 5, 1, &[]).is_none(),
            "5% passes"
        );
        assert!(
            build_reliability_warning(100, 4, 1, &[]).is_some(),
            "4% is flagged"
        );
        assert!(
            build_reliability_warning(0, 0, 7, &[]).is_none(),
            "no production candidates → nothing to judge"
        );
    }

    fn lang(extension: &str, production: u32, covered: u32, test_nodes: u32) -> LanguageCoverage {
        LanguageCoverage {
            extension: extension.to_string(),
            production,
            covered,
            test_nodes,
        }
    }

    /// The defect this guard exists for: a polyglot repo whose GLOBAL ratio is
    /// perfectly normal while one stack resolved no test edges at all. Measured
    /// 2026-09-06 — DOC-V2 reported 27.7% overall with a top-25 full of
    /// demonstrably tested Flutter primitives, and this repo's healthy graph
    /// reports 28.3%. The global number cannot separate them; a per-language
    /// row can.
    #[test]
    fn a_blind_language_is_flagged_even_when_the_global_ratio_is_healthy() {
        let by_language = [
            lang(".java", 1000, 400, 500), // 40% — healthy, carries the average
            lang(".dart", 800, 8, 900),    // 1% with 900 test symbols — impossible
        ];
        let warning = build_reliability_warning(1800, 408, 1400, &by_language)
            .expect("a language below the floor must be flagged");
        assert!(warning.contains("PARTIALLY UNRELIABLE"));
        assert!(warning.contains(".dart"));
        assert!(
            !warning.contains(".java"),
            "the healthy language must not be named as suspect"
        );
    }

    // ── PARTIAL extraction failure: the shape the absolute floor cannot see ──
    //
    // Both fixtures are real measurements taken 2026-09-08, after the seed fix,
    // and BOTH were silent under the absolute floor alone. A language does not
    // fall this far behind its siblings — measured by the same extractor, the
    // same edge types, the same conventions — because it is merely less tested.

    #[test]
    fn a_language_far_below_its_siblings_is_flagged() {
        // DOC-V2 as measured: Dart 20.5% against Java 52.2%, overall 30.9%.
        // Well clear of the 5% floor, and the top-25 was full of demonstrably
        // tested Flutter primitives.
        let by_language = [
            lang(".java", 6509, 3397, 9966),
            lang(".dart", 9299, 1909, 4396),
        ];
        let warning = build_reliability_warning(17196, 5306, 14362, &by_language)
            .expect("a language at 39% of the repo's best must be flagged");
        assert!(warning.contains(".dart"), "the diverging language is named");
        assert!(
            !warning.contains(".java"),
            "the baseline language must not be named as suspect"
        );

        // This repo as measured: TypeScript 13.0% against Rust 44.7%.
        let by_language = [lang(".rs", 7057, 3156, 6646), lang(".ts", 1396, 181, 604)];
        let warning = build_reliability_warning(8842, 3337, 7658, &by_language)
            .expect("29% of the best language must be flagged");
        assert!(warning.contains(".ts"));
        assert!(!warning.contains(".rs"));
    }

    #[test]
    fn a_language_merely_lower_than_its_siblings_is_not_flagged() {
        // 30% against 50% is 0.6 of the best — lower, but not the cliff that
        // marks an extraction failure. Flagging this would train the reader to
        // ignore the warning, which costs more than the miss.
        let by_language = [lang(".java", 1000, 500, 800), lang(".dart", 1000, 300, 700)];
        assert!(
            build_reliability_warning(2000, 800, 1500, &by_language).is_none(),
            "a merely-less-tested language must not be called unreliable"
        );
    }

    #[test]
    fn the_relative_rule_is_inert_with_a_single_judgeable_language() {
        // Nothing can sit below half of itself. A lone language has no sibling to
        // diverge from, so only the absolute floor can speak — and 40% is fine.
        let by_language = [
            lang(".rs", 1000, 400, 500),
            lang(".sh", 10, 0, 3), // under LANGUAGE_MIN_PRODUCTION, not judgeable
        ];
        assert!(build_reliability_warning(1010, 400, 503, &by_language).is_none());
    }

    /// A language with no tests at all measures 0% honestly — that is a finding,
    /// not a malfunction, exactly as for a whole repo with no test code.
    #[test]
    fn a_language_without_tests_is_not_flagged() {
        let by_language = [
            lang(".java", 1000, 400, 500),
            lang(".sql", 200, 0, 0), // no tests → an honest 0%
        ];
        assert!(build_reliability_warning(1200, 400, 500, &by_language).is_none());
    }

    /// Small languages swing wildly on a handful of unresolved edges, so they
    /// are not judged.
    #[test]
    fn a_tiny_language_is_not_judged() {
        let by_language = [
            lang(".java", 1000, 400, 500),
            lang(".lua", 10, 0, 3), // under LANGUAGE_MIN_PRODUCTION
        ];
        assert!(build_reliability_warning(1010, 400, 503, &by_language).is_none());
    }
}
