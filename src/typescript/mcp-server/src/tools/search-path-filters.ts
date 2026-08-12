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
 *
 * ## The worktree-origin shape (single home for this rationale)
 *
 * A worktree-origin point is indexed with the worktree prefix STRIPPED: its
 * `relative_path` is byte-identical to the main tree's file at that path, and
 * only the absolute `file_path` carries `.claude/worktrees/<name>/`. Anything
 * that tests the relative path alone therefore checks the one field that can
 * never distinguish a worktree copy — which is how
 * `pathExclude: ".claude/worktrees/**"` returned foreign-session hits and the
 * default worktree drop shipped as a silent no-op on the semantic lane (it
 * worked in `grep`, whose matches carry only the absolute path). Measured
 * 2026-08-12, DOC-V2.
 *
 * But a caller GLOB must never be tested against the raw absolute path either:
 * globs float (`docs/**` → `**​/docs/**`), so a raw-absolute test matches host
 * ANCESTORS of the repo — a checkout under `~/docs/` would have every hit
 * dropped by `pathExclude: "docs/**"`. Hence {@link globCandidates}: the
 * repo-relative path, plus the repo-relative WORKTREE SUBPATH sliced from the
 * absolute, plus the raw absolute only when no relative exists at all (the
 * exact/grep result shape).
 *
 * `list` is structurally exempt from all of this: `tracked_files` has no
 * absolute-path column, so a listing can never emit a worktree path.
 *
 * Known limit, root-fixed by the planned payload re-anchor (PR-7 in the
 * 2026-08-12 plan): content byte-identical in main and a worktree shares ONE
 * point whose stored absolute path belongs to whichever ingest wrote it last,
 * so the default drop can hide a file that genuinely exists on main. `grep`
 * has accepted that trade since #337; the semantic lane matching it keeps the
 * two surfaces consistent until the daemon stops persisting worktree paths.
 */

import type { SearchResult, DerankConfig } from './search-types.js';
import {
  matchesPathExclude,
  isWorktreeSubtreePath,
  worktreeSubpathOf,
  worktreeNameOf,
} from '../utils/path-glob.js';
import { getWorktreeContext } from '../utils/request-context.js';

function metadataPath(result: SearchResult, key: string): string | undefined {
  const value = result.metadata[key];
  return typeof value === 'string' && value.length > 0 ? value : undefined;
}

/** Does a caller glob explicitly target the worktree subtree? Consent marker
 *  shared by the include-suppression and the subpath glob candidate. */
export function globTargetsWorktrees(glob: string | undefined): boolean {
  return glob !== undefined && glob.replace(/\\/g, '/').includes('.claude/worktrees');
}

/**
 * The paths a caller-supplied GLOB is tested against — see the module header.
 * Repo-relative coordinates only, unless the result has no relative path at
 * all (then the raw absolute is the only name it has, and pre-existing globs
 * were written against it).
 *
 * The worktree SUBPATH candidate is added only when the exclude itself targets
 * worktrees. An ordinary scoping exclude (`tests/**`, a slug-colliding
 * literal) must not be evaluated against `.claude/worktrees/<slug>/…` — a
 * namespace the caller never sees in displayed results and cannot reproduce a
 * drop from.
 */
function globCandidates(result: SearchResult, excludeTargetsWorktrees: boolean): string[] {
  const rel = metadataPath(result, 'relative_path');
  const abs = metadataPath(result, 'file_path');
  const candidates: string[] = [];
  if (rel !== undefined) candidates.push(rel);
  if (abs !== undefined) {
    const worktreeSubpath = excludeTargetsWorktrees ? worktreeSubpathOf(abs) : undefined;
    if (worktreeSubpath !== undefined) candidates.push(worktreeSubpath);
    else if (rel === undefined) candidates.push(abs);
  }
  return candidates;
}

/**
 * True when a result should be dropped by the caller-aware worktree gate:
 * a MAIN caller sees no worktree paths at all; a caller inside worktree A
 * keeps A's own paths but still must not see worktree B's (the primary /batch
 * population — sibling agents' trees; a presence-only gate let every worktree
 * caller see every other worktree).
 */
export function isForeignWorktreePath(
  path: string,
  callerWorktree: string | undefined
): boolean {
  if (!isWorktreeSubtreePath(path)) return false;
  if (callerWorktree === undefined) return true;
  return worktreeNameOf(path) !== callerWorktree;
}

/**
 * Drop every result whose path matches the `pathExclude` glob, PLUS — for a
 * main-tenant caller — any result whose ABSOLUTE path lies under another
 * worktree's `.claude/worktrees/` subtree: a leaked/stale path that points into
 * the wrong checkout. The worktree drop is skipped when the caller itself works
 * inside a worktree (those paths are legitimately its own) and when the
 * caller's INCLUDE glob explicitly targets the worktree subtree — an explicit
 * `pathGlob: ".claude/worktrees/…"` is consent to see those paths, and letting
 * the default drop cancel it made the include deterministically return zero.
 *
 * A result with no resolvable path is KEPT (it cannot match, so dropping it
 * would be a silent false-drop). Explicit-exclude semantics for shared points
 * (identical content in main and a worktree, one point): a match on the
 * worktree subpath drops the result — the caller asked not to be shown
 * worktree paths, and the measured alternative was presenting another
 * session's checkout as if it were the main tree.
 */
export function filterResultsByPathExclude(
  results: SearchResult[],
  pathExclude: string | undefined,
  includeGlob?: string
): SearchResult[] {
  const dropForeignWorktrees = !globTargetsWorktrees(includeGlob);
  const callerWorktree = getWorktreeContext()?.name;
  if (!pathExclude && !dropForeignWorktrees) return results;
  const excludeTargetsWorktrees = globTargetsWorktrees(pathExclude);
  return results.filter((result) => {
    // The default drop tests BOTH coordinates: the absolute path (worktree-
    // origin points, prefix stripped from the relative) AND the relative path
    // (legacy/stale generations indexed before the strip, which carry the
    // worktree prefix there — the shape #337 was originally written for).
    // The `.claude/worktrees/` segment is unambiguous in either coordinate;
    // only caller GLOBS are unsafe against raw absolutes (see module header).
    if (dropForeignWorktrees) {
      const rel = metadataPath(result, 'relative_path');
      const abs = metadataPath(result, 'file_path');
      if (
        (abs !== undefined && isForeignWorktreePath(abs, callerWorktree)) ||
        (rel !== undefined && isForeignWorktreePath(rel, callerWorktree))
      ) {
        return false;
      }
    }
    return pathExclude
      ? !globCandidates(result, excludeTargetsWorktrees).some((p) =>
          matchesPathExclude(p, pathExclude)
        )
      : true;
  });
}

/**
 * Soft de-rank: multiply the RANKING score of any result whose path contains a
 * configured substring by the penalty (<1), sinking legacy dirs (e.g.
 * `old_project/`) below live code WITHOUT removing them. Mutates `results` in
 * place and is ordering-only — the caller restores the pre-boost display score
 * downstream, exactly as it does for the path-relevance boost. No-op when there
 * are no substrings or the penalty is not a genuine down-weight.
 *
 * Tests the relative AND the absolute path, so a deployment substring that is
 * only expressible against the absolute shape (`.claude/worktrees/`, a shared
 * vendor root) works — the hard exclude above and this soft sibling must agree
 * on what "the path of a result" means. Substring semantics don't float, and
 * the penalty is uniform, so an ancestor-segment collision merely rescales
 * every hit equally (ordering unchanged) instead of dropping anything.
 */
export function applyPathDerank(results: SearchResult[], config: DerankConfig): void {
  if (config.substrings.length === 0 || config.penalty >= 1) return;
  for (const result of results) {
    const rel = metadataPath(result, 'relative_path');
    const abs = metadataPath(result, 'file_path');
    const haystacks: string[] = [];
    if (rel !== undefined) haystacks.push(rel.replace(/\\/g, '/'));
    if (abs !== undefined && abs !== rel) haystacks.push(abs.replace(/\\/g, '/'));
    if (haystacks.length === 0) continue;
    if (
      config.substrings.some((needle) => haystacks.some((h) => h.includes(needle)))
    ) {
      result.score *= config.penalty;
    }
  }
}
