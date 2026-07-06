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
import { matchesPathExclude } from '../utils/path-glob.js';

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
 * Drop every result whose path matches the `pathExclude` glob. A result with no
 * resolvable path is KEPT (it cannot match the exclude, so excluding it would be
 * a silent false-drop). Floats like the include filter — see
 * {@link matchesPathExclude}.
 */
export function filterResultsByPathExclude(
  results: SearchResult[],
  pathExclude: string | undefined
): SearchResult[] {
  if (!pathExclude) return results;
  return results.filter((result) => {
    const path = resultPath(result);
    if (path === undefined) return true;
    return !matchesPathExclude(path, pathExclude);
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
