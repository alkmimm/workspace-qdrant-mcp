/**
 * Shared empty-result diagnosis for the project-scoped FTS read tools (`grep`
 * and exact `search`).
 *
 * A bare "No matches" conflates several very different situations: the pattern
 * is genuinely absent, a `pathGlob`/`pathExclude` whose SHAPE excluded every hit,
 * a well-formed path filter over files that merely don't contain the pattern, or
 * a requested branch with no indexed content yet. Distinguishing them turns a
 * dead-end 0 into an actionable message — and, critically, stops a valid glob
 * from being falsely accused of being malformed. All three read tools funnel
 * through here so the behavior can't drift (CLAUDE.md shared-behavior rule) —
 * grep, search-exact, and semantic `search` each supply a
 * `countWithoutPathFilter` closure appropriate to their own request shape.
 *
 * The two FTS tools (grep, exact) match a literal `pattern`; semantic `search`
 * has none — an empty result under a well-formed glob means "nothing was
 * relevant enough", not "pattern absent". `mode` selects the wording for that
 * one sub-verdict (1b); the shape (1a) and branch (2) probes are mode-agnostic.
 */

import { concreteBranchFilter } from './branch-scope.js';
import type { SearchDbReader } from '../clients/search-db-reader.js';

/**
 * Cap for the "without path filter" probe query. The message shows `N+` when the
 * probe hits this cap so a large unfiltered result isn't understated.
 */
export const EMPTY_DIAGNOSIS_PROBE_LIMIT = 50;

/** Render "pathGlob \"x\" + pathExclude \"y\"" for whichever filters are set. */
function describePathFilters(
  pathGlob: string | undefined,
  pathExclude: string | undefined
): string {
  return [
    pathGlob ? `pathGlob "${pathGlob}"` : undefined,
    pathExclude ? `pathExclude "${pathExclude}"` : undefined,
  ]
    .filter(Boolean)
    .join(' + ');
}

/** Format a probe count, appending "+" when it hit the probe cap. */
function withCapMarker(n: number): string {
  return `${n}${n >= EMPTY_DIAGNOSIS_PROBE_LIMIT ? '+' : ''}`;
}

/**
 * A path filter (`pathGlob`/`pathExclude`) selected NO indexed file, yet the
 * same query without it has hits — so the filter shape itself is the problem
 * (malformed / too restrictive), not the pattern. Only emitted after confirming
 * the filter matches zero indexed files (see {@link patternAbsentUnderPathFilterMessage}
 * for the well-formed-but-empty case).
 */
export function pathFilterExcludedAllMessage(
  nWithout: number,
  pathGlob: string | undefined,
  pathExclude: string | undefined
): string {
  return (
    `No matches under ${describePathFilters(pathGlob, pathExclude)}, but the same query WITHOUT the path filter has ` +
    `${withCapMarker(nWithout)} — and the filter matches NO indexed file, so its shape excluded everything. ` +
    'pathGlob matches the ABSOLUTE file path and multi-segment literal folders must be ADJACENT ' +
    '(e.g. "a/b/**" needs a and b consecutive); a name-only filter should float as "**/Name.ext". ' +
    'Adjust or drop the filter, then retry.'
  );
}

/**
 * The path filter is WELL-FORMED — it selects real indexed files — but none of
 * them contain the pattern, while the same query without the filter has hits in
 * OTHER files. The emptiness is the pattern being absent from the filtered
 * files, NOT a broken glob. Guards against the false-blame where a valid filter
 * like `*.proto` is accused of being malformed only because the literal
 * (e.g. snake_case `reference_schedule`) lives elsewhere under a different
 * casing (`ReferenceSchedule`).
 */
export function patternAbsentUnderPathFilterMessage(
  nWithout: number,
  nFilesMatched: number,
  pathGlob: string | undefined,
  pathExclude: string | undefined
): string {
  return (
    `No matches under ${describePathFilters(pathGlob, pathExclude)}. The path filter is well-formed — it selects ` +
    `${withCapMarker(nFilesMatched)} indexed file(s) — but none of them contain the pattern. The same query WITHOUT ` +
    `the path filter has ${withCapMarker(nWithout)}, so those matches live in OTHER files; this is NOT a filter-shape ` +
    'problem. The pattern is most likely just absent from the filtered files — check naming/casing ' +
    '(e.g. snake_case "my_symbol" vs PascalCase "MySymbol") or whether the symbol really lives in that ' +
    'filetype. Widen or drop the filter if you expected a match here.'
  );
}

/**
 * Semantic-search analogue of {@link patternAbsentUnderPathFilterMessage}: the
 * path filter is WELL-FORMED (selects real indexed files), the query has hits
 * elsewhere, but nothing under the filter cleared the score threshold. There is
 * no literal "pattern" to be miscased here — the emptiness is a relevance/scope
 * miss, so the message points at the threshold and phrasing instead of casing.
 */
export function noSemanticMatchUnderPathFilterMessage(
  nWithout: number,
  nFilesMatched: number,
  pathGlob: string | undefined,
  pathExclude: string | undefined
): string {
  return (
    `No semantic matches under ${describePathFilters(pathGlob, pathExclude)}. The path filter is well-formed — it ` +
    `selects ${withCapMarker(nFilesMatched)} indexed file(s) — but none of their content cleared the score threshold ` +
    `under it, while the same query WITHOUT the path filter has ${withCapMarker(nWithout)}. This is NOT a filter-shape ` +
    'problem: the relevant content lives in OTHER files. Widen or drop the filter, lower scoreThreshold, or rephrase ' +
    'the query with vocabulary closer to the target code (identifiers/comments are mostly English).'
  );
}

/** Result of the case-insensitivity probe (see {@link EmptyDiagnosisInput}). */
export interface CaseProbeResult {
  /** Case-insensitive match count, capped at {@link EMPTY_DIAGNOSIS_PROBE_LIMIT}. */
  count: number;
  /** One example hit, so the message can SHOW the casing that was missed. */
  sample?: { file: string; content: string } | undefined;
}

/** Cap for the sample line embedded in the case-probe message. */
const CASE_SAMPLE_MAX_CHARS = 120;

/**
 * The pattern is absent case-SENSITIVELY but present case-INSENSITIVELY — the
 * classic camelCase trap: `declineReason` cannot match `getDeclineReason`, so
 * a case-sensitive zero must not be read as "the symbol does not exist".
 * Emitted only when the original query ran case-sensitively; shows a sample
 * hit so the agent sees the actual casing, plus the tool-specific retry knob.
 */
export function caseSensitivityMessage(probe: CaseProbeResult, retryHint: string): string {
  const sampleLine = probe.sample?.content.trim() ?? '';
  const truncatedSample =
    sampleLine.length > CASE_SAMPLE_MAX_CHARS
      ? `${sampleLine.slice(0, CASE_SAMPLE_MAX_CHARS)}…`
      : sampleLine;
  const example = probe.sample ? ` — e.g. \`${truncatedSample}\` in ${probe.sample.file}` : '';
  return (
    `No case-sensitive matches, but ${withCapMarker(probe.count)} case-INSENSITIVE match(es) ` +
    `exist (any branch)${example}. CamelCase hides identifiers inside wrappers ` +
    `(a case-sensitive "declineReason" cannot see "getDeclineReason"), so this zero does NOT ` +
    `prove the symbol is absent. ${retryHint}`
  );
}

/**
 * The requested branch has 0 files listed under its own name in the index. This
 * is a soft signal, not a verdict: it usually means a freshly created /
 * not-yet-indexed branch, but a file UNCHANGED across a checkout can stay tagged
 * under the branch it was last modified on (branch-membership drift), so a
 * branch that IS effectively indexed can still show 0 under its own name. The
 * message hedges accordingly and does NOT suggest `branch:"*"` as a next step —
 * the callers (grep / exact) already auto-widened across all branches before
 * reaching this probe, so the cross-branch content (if any) was already checked.
 */
export function branchNotIndexedMessage(branch: string, indexedBranches: string[]): string {
  const list = indexedBranches.length > 0 ? indexedBranches.slice(0, 6).join(', ') : '(none yet)';
  return (
    `Branch "${branch}" has 0 files indexed under its own name (likely a freshly created or ` +
    'not-yet-indexed branch — or its files are still tagged under the branch they were last ' +
    'modified on). The pattern was also not found on any other indexed branch. Indexed branches ' +
    `for this project: ${list}. If this branch was just created, let the daemon index it and retry.`
  );
}

/**
 * Index-lag caveat for an empty project-scoped FTS result, shared by grep and
 * exact search. A literal miss is NOT proof of absence when the daemon's index
 * trails recent commits: a file created or modified moments ago has not been
 * ingested yet, so the FTS index legitimately cannot see it. Field feedback
 * (2026-07-28): a grep for a symbol added in same-day commits returned 0 and the
 * hint sent the caller cross-repository (`scope:"all"`) instead of naming the far
 * more likely local cause — the index being a few commits behind.
 *
 * When `progress` shows in-flight work we state the concrete backlog (a firm
 * signal the index is mid-catch-up); otherwise we give the generic "verify on
 * disk" caveat. `pending` can be 0 while a brand-new file is still invisible
 * because the watcher has not enqueued it yet, so a drained queue does NOT prove
 * the index is current for a just-touched file — the caveat holds either way,
 * only its confidence changes. Structurally typed so any `IndexingProgress`
 * (or `null` when the probe could not run) is accepted without a hard dependency.
 */
export function indexLagCaveat(
  progress: { pending: number; in_progress: number } | null | undefined
): string {
  const inFlight = progress ? progress.pending + progress.in_progress : 0;
  if (progress && inFlight > 0) {
    return (
      `The index is still catching up (${progress.pending} pending, ` +
      `${progress.in_progress} in progress) — a file created or changed moments ago may not ` +
      'be searchable yet, so this zero does NOT prove the pattern is absent; verify on disk, ' +
      'or retry once indexing settles.'
    );
  }
  return (
    'If a matching file was created or changed very recently, the index may not have caught ' +
    'up yet — this zero would then be index lag, not true absence, so verify on disk.'
  );
}

/** Inputs for {@link diagnoseEmptyResult}. */
export interface EmptyDiagnosisInput {
  tenantId: string;
  /** Effective branch on the query (may be `"*"`/undefined — those skip the branch probe). */
  branch: string | undefined;
  pathGlob: string | undefined;
  pathExclude: string | undefined;
  /** search.db reader for branch coverage, or undefined to skip the branch probe. */
  searchDbReader: SearchDbReader | undefined;
  /**
   * Re-run the SAME scope with NO path filter (no `pathGlob`, no `pathExclude`
   * post-filter) and return the match count, capped at
   * {@link EMPTY_DIAGNOSIS_PROBE_LIMIT}. Only called when a path filter is set.
   */
  countWithoutPathFilter: () => Promise<number>;
  /**
   * Which read tool is diagnosing. `'literal'` (default — grep/exact) blames an
   * absent literal pattern under a well-formed filter; `'semantic'` blames a
   * relevance/score-threshold miss instead (no literal to miscase). Only affects
   * probe (1b); (1a) shape and (2) branch are identical either way.
   */
  mode?: 'literal' | 'semantic';
  /**
   * Case probe: re-run the SAME pattern and path filters case-INSENSITIVELY
   * across ALL branches (the callers' auto-widen already proved the
   * case-sensitive form is absent everywhere, so flipping only case isolates
   * it as the cause) and return the count plus a sample hit. Supply it ONLY
   * when the original query ran case-sensitively (grep's default / exact
   * always); omit for `caseSensitive:false` calls and for semantic mode,
   * where there is no case dimension.
   */
  probeCaseInsensitive?: () => Promise<CaseProbeResult>;
  /**
   * Tool-specific remedy appended to the case-probe message (e.g. grep:
   * "Retry with caseSensitive:false."; exact search has no case knob and
   * points at grep instead).
   */
  caseRetryHint?: string;
}

/**
 * Diagnose a still-empty project-scoped FTS result. Three cheap probes, most
 * specific first:
 *   1. **A path filter is responsible** — only when `pathGlob`/`pathExclude` is
 *      set and the unfiltered scope has hits. This splits into two verdicts by a
 *      second probe (does the filter select any indexed file?):
 *        1a. filter selects NO file → its shape excluded everything (malformed
 *            / too restrictive) → {@link pathFilterExcludedAllMessage}.
 *        1b. filter selects real files but the result isn't in them → the glob
 *            is fine. Literal mode → {@link patternAbsentUnderPathFilterMessage}
 *            (pattern absent, check casing); semantic mode →
 *            {@link noSemanticMatchUnderPathFilterMessage} (relevance/threshold
 *            miss). Avoids falsely blaming a valid glob (e.g. `*.proto`).
 *   2. **Case sensitivity is responsible** — only when the caller supplies the
 *      case probe (i.e. the query ran case-sensitively) and the same query
 *      matches case-insensitively → {@link caseSensitivityMessage}. Runs after
 *      the path probes (a filter that excluded everything is the more specific
 *      verdict) and before the branch probe (a concrete casing fix beats a
 *      coverage hint).
 *   3. **Branch has no indexed content** — only for a concrete branch with a
 *      reader available.
 * Returns `undefined` when none applies. Best-effort: any probe error is
 * swallowed so diagnosis never breaks the response.
 */
export async function diagnoseEmptyResult(input: EmptyDiagnosisInput): Promise<string | undefined> {
  const { tenantId, branch, pathGlob, pathExclude, searchDbReader, countWithoutPathFilter } = input;
  const mode = input.mode ?? 'literal';

  // (1) Is a path filter responsible for the empty result?
  if (pathGlob || pathExclude) {
    try {
      const nWithout = await countWithoutPathFilter();
      if (nWithout > 0) {
        // The query has hits in the unfiltered scope. Distinguish a filter that
        // selects nothing (shape problem) from one that selects real files the
        // result simply isn't in. For 1b the wording forks by mode: a literal
        // pattern absent from those files (naming/casing) vs a semantic relevance
        // miss (nothing cleared the score threshold there).
        const nFilesMatched = searchDbReader
          ? tryCountFilesMatchingPathFilters(searchDbReader, tenantId, pathGlob, pathExclude)
          : 0;
        if (nFilesMatched === 0) {
          return pathFilterExcludedAllMessage(nWithout, pathGlob, pathExclude);
        }
        return mode === 'semantic'
          ? noSemanticMatchUnderPathFilterMessage(nWithout, nFilesMatched, pathGlob, pathExclude)
          : patternAbsentUnderPathFilterMessage(nWithout, nFilesMatched, pathGlob, pathExclude);
      }
    } catch {
      // fall through to the branch probe
    }
  }

  // (2) Is case sensitivity responsible? Only probed when the caller ran
  // case-sensitively (the probe closure is absent otherwise).
  if (input.probeCaseInsensitive) {
    try {
      const probe = await input.probeCaseInsensitive();
      if (probe.count > 0) {
        return caseSensitivityMessage(probe, input.caseRetryHint ?? 'Retry case-insensitively.');
      }
    } catch {
      // fall through to the branch probe
    }
  }

  // (3) Does the requested branch have any indexed content?
  const concreteBranch = concreteBranchFilter(branch);
  if (concreteBranch && searchDbReader) {
    try {
      const counts = searchDbReader.listBranchCounts({ tenantId });
      if (counts.length > 0) {
        const requested = counts.find((c) => c.branch === concreteBranch);
        if (!requested || requested.files === 0) {
          const indexed = counts
            .filter((c) => c.files > 0 && c.branch !== '(none)')
            .map((c) => c.branch);
          return branchNotIndexedMessage(concreteBranch, indexed);
        }
      }
    } catch {
      // best-effort — ignore reader errors
    }
  }

  return undefined;
}

/**
 * Best-effort count of indexed files matching the path filter, capped at the
 * probe limit. Swallows any reader error (or a reader mock lacking the method)
 * and returns 0, so a failure degrades to the shape-oriented message rather than
 * breaking diagnosis.
 */
function tryCountFilesMatchingPathFilters(
  searchDbReader: SearchDbReader,
  tenantId: string,
  pathGlob: string | undefined,
  pathExclude: string | undefined
): number {
  try {
    return searchDbReader.countFilesMatchingPathFilters({
      tenantId,
      pathGlob,
      pathExclude,
      limit: EMPTY_DIAGNOSIS_PROBE_LIMIT,
    });
  } catch {
    return 0;
  }
}
