/**
 * Shared empty-result diagnosis for the project-scoped FTS read tools (`grep`
 * and exact `search`).
 *
 * A bare "No matches" conflates three very different situations: the pattern is
 * genuinely absent, a `pathGlob`/`pathExclude` excluded every hit, or the
 * requested branch simply has no indexed content yet. Distinguishing them turns
 * a dead-end 0 into an actionable message. Both FTS tools funnel through here so
 * the behavior can't drift (CLAUDE.md shared-behavior rule) — grep and
 * search-exact each supply a `countWithoutPathFilter` closure appropriate to
 * their own request shape.
 */

import { concreteBranchFilter } from './branch-scope.js';
import type { SearchDbReader } from '../clients/search-db-reader.js';

/**
 * Cap for the "without path filter" probe query. The message shows `N+` when the
 * probe hits this cap so a large unfiltered result isn't understated.
 */
export const EMPTY_DIAGNOSIS_PROBE_LIMIT = 50;

/**
 * A path filter (`pathGlob`/`pathExclude`) removed every match that the same
 * query would otherwise return — the emptiness is the filter, not the pattern.
 */
export function pathFilterExcludedAllMessage(
  nWithout: number,
  pathGlob: string | undefined,
  pathExclude: string | undefined
): string {
  const filters = [
    pathGlob ? `pathGlob "${pathGlob}"` : undefined,
    pathExclude ? `pathExclude "${pathExclude}"` : undefined,
  ]
    .filter(Boolean)
    .join(' + ');
  return (
    `No matches under ${filters}, but the same query WITHOUT the path filter has ` +
    `${nWithout}${nWithout >= EMPTY_DIAGNOSIS_PROBE_LIMIT ? '+' : ''} — the path filter excluded everything. ` +
    'pathGlob matches the ABSOLUTE file path and multi-segment literal folders must be ADJACENT ' +
    '(e.g. "a/b/**" needs a and b consecutive); a name-only filter should float as "**/Name.ext". ' +
    'Adjust or drop the filter, then retry.'
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
}

/**
 * Diagnose a still-empty project-scoped FTS result. Two cheap probes, most
 * specific first:
 *   1. **Path filter dropped everything** — only when `pathGlob`/`pathExclude`
 *      is set and the unfiltered scope has hits.
 *   2. **Branch has no indexed content** — only for a concrete branch with a
 *      reader available.
 * Returns `undefined` when neither applies. Best-effort: any probe error is
 * swallowed so diagnosis never breaks the response.
 */
export async function diagnoseEmptyResult(input: EmptyDiagnosisInput): Promise<string | undefined> {
  const { tenantId, branch, pathGlob, pathExclude, searchDbReader, countWithoutPathFilter } = input;

  // (1) Did a path filter exclude everything?
  if (pathGlob || pathExclude) {
    try {
      const nWithout = await countWithoutPathFilter();
      if (nWithout > 0) {
        return pathFilterExcludedAllMessage(nWithout, pathGlob, pathExclude);
      }
    } catch {
      // fall through to the branch probe
    }
  }

  // (2) Does the requested branch have any indexed content?
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
