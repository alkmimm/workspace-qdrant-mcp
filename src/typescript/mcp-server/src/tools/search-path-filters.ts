/**
 * Path-based result shaping shared by the semantic and exact search paths:
 *
 *  - {@link filterResultsByPathExclude} — a HARD, per-call filter (`pathExclude`
 *    glob) that drops any hit under a path, complementing the `pathGlob` include.
 *  - {@link applyPathDerank} — a SOFT, deployment-default penalty
 *    (`WQM_SEARCH_DERANK`) that only reorders: it multiplies the RANKING score of
 *    a legacy path so it sinks below live code but stays findable.
 *
 * Both live here (not in search-helpers) so search-exact can reuse the exclude
 * without importing the whole finalize pipeline (avoids an import cycle).
 */

import type { SearchResult, DerankConfig } from './search-types.js';
import { matchesPathExclude, isWorktreeSubtreePath } from '../utils/path-glob.js';
import { getWorktreeContext } from '../utils/request-context.js';

/** Prefer the repo-relative path (stable, what the caller reasons about); fall
 *  back to the absolute `file_path` when a result carries no relative path. */
function resultPath(result: SearchResult): string | undefined {
  const rel = result.metadata['relative_path'];
  if (typeof rel === 'string' && rel.length > 0) return rel;
  const abs = result.metadata['file_path'];
  if (typeof abs === 'string' && abs.length > 0) return abs;
  return undefined;
}

/**
 * EVERY path a result is known by — relative first, then absolute.
 *
 * The exclude has to test both, because a worktree-origin point is indexed with
 * the worktree prefix STRIPPED: its `relative_path` is byte-identical to the
 * main tree's file at that path, and only `file_path`/`absolute_path` carries
 * the `.claude/worktrees/<name>/` segment. Testing `resultPath()` alone
 * therefore checked the one field that can never distinguish them, so
 * `pathExclude: ".claude/worktrees/**"` returned foreign-session hits — and the
 * default worktree drop below was a silent no-op on the semantic lane for the
 * same reason (it works in `grep`, whose matches carry only the absolute path).
 *
 * This mirrors the include filter, which already ORs the two
 * (`filterResultsByPathGlob`).
 */
function resultPaths(result: SearchResult): string[] {
  const paths: string[] = [];
  for (const key of ['relative_path', 'file_path', 'absolute_path'] as const) {
    const value = result.metadata[key];
    if (typeof value === 'string' && value.length > 0 && !paths.includes(value)) {
      paths.push(value);
    }
  }
  return paths;
}

/**
 * Drop every result whose path matches the `pathExclude` glob, PLUS — for a
 * main-tenant caller — any result under another worktree's `.claude/worktrees/`
 * subtree (see {@link isWorktreeSubtreePath}): a leaked/stale path that points
 * into the wrong checkout. The worktree drop is skipped when the caller itself
 * works inside a worktree (then those paths are legitimately its own). A result
 * with no resolvable path is KEPT (it cannot match, so dropping it would be a
 * silent false-drop). Floats like the include filter — see
 * {@link matchesPathExclude}.
 *
 * A match on ANY known path drops the result (see {@link resultPaths}). Note the
 * consequence for content that is byte-identical in the main tree and a
 * worktree: cross-branch dedup collapses it onto ONE point whose stored absolute
 * path belongs to whichever ingest ran first, so such a point can be excluded
 * via its worktree path even though the same content also lives in main. That is
 * the correct trade — the caller asked not to be shown worktree paths, and the
 * alternative (the measured behaviour) is presenting another session's checkout
 * as if it were the main tree, which is the failure this filter exists to stop.
 */
export function filterResultsByPathExclude(
  results: SearchResult[],
  pathExclude: string | undefined
): SearchResult[] {
  const dropOtherWorktrees = getWorktreeContext() === undefined;
  if (!pathExclude && !dropOtherWorktrees) return results;
  return results.filter((result) => {
    const paths = resultPaths(result);
    if (paths.length === 0) return true;
    if (dropOtherWorktrees && paths.some(isWorktreeSubtreePath)) return false;
    return pathExclude ? !paths.some((p) => matchesPathExclude(p, pathExclude)) : true;
  });
}

/**
 * Soft de-rank: multiply the RANKING score of any result whose path contains a
 * configured substring by the penalty (<1), sinking legacy dirs (e.g.
 * `old_project/`) below live code WITHOUT removing them. Mutates `results` in
 * place and is ordering-only — the caller restores the pre-boost display score
 * downstream, exactly as it does for the path-relevance boost. No-op when there
 * are no substrings or the penalty is not a genuine down-weight.
 */
export function applyPathDerank(results: SearchResult[], config: DerankConfig): void {
  if (config.substrings.length === 0 || config.penalty >= 1) return;
  for (const result of results) {
    const path = resultPath(result);
    if (path === undefined) continue;
    const normalized = path.replace(/\\/g, '/');
    if (config.substrings.some((needle) => normalized.includes(needle))) {
      result.score *= config.penalty;
    }
  }
}
