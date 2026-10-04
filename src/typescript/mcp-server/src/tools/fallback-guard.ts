/**
 * Which trunk entries may fill in a branch-scoped read — one rule shared by
 * grep, exact search, the semantic lane, retrieve and list.
 *
 * A read on a feature branch falls back to the trunk for files the branch has
 * not been indexed under. That fill-in is only sound for a path whose trunk
 * copy IS the branch's copy. #408 admitted a trunk entry whenever the scoped
 * result had no HIT for its path, which is a different question: a file the
 * branch deleted has no hit (so the trunk resurrected it), and a file the
 * branch changed has no hit whenever its new content stops matching (so the
 * trunk's old line surfaced as a match on the branch). Observed on
 * emnify-sms-sender (2026-10-03): `app/lib/actions/endpoint.ts` and
 * `Message.ts`, deleted on the branch, and the trunk's `message/page.tsx`
 * starting with `'use server'` where the branch's starts with an import.
 *
 * A trunk entry is admitted only when its path is
 *   1. not changed between the trunk and branch tips (git) — modified, added or
 *      deleted on either side means the trunk copy is not the branch's; and
 *   2. not held by the branch in the index at all — the branch's own
 *      generation owns the path even when it did not match this query (this
 *      also covers uncommitted edits the tips do not show).
 * When git cannot answer, rule 2 still applies. An entry whose path cannot be
 * read is kept: a metadata gap must not become silent data loss.
 */

import { getPathsChangedBetween } from '../utils/git-branch-diff.js';
import { normalizeBranchList } from './branch-scope.js';

/** What the guard needs from the outside world (injectable for tests). */
export interface FallbackGuardSources {
  /** Which of these repo-relative paths the index holds under the branch. */
  trackedOnBranch(relativePaths: readonly string[]): ReadonlySet<string>;
  /** Paths that differ between the trunk and branch tips; `null` if unknown. */
  changedBetweenTips(): Promise<ReadonlySet<string> | null>;
}

export class FallbackGuard {
  private changed: Promise<ReadonlySet<string> | null> | undefined;

  constructor(
    readonly branch: string,
    readonly fallbackBranch: string,
    private readonly projectRoot: string | undefined,
    private readonly sources: FallbackGuardSources
  ) {}

  /** Repo-relative form of a path: a main-anchored absolute path loses the root. */
  relative(path: string): string {
    const root = this.projectRoot?.replace(/\/+$/, '');
    if (root && path.startsWith(`${root}/`)) return path.slice(root.length + 1);
    return path;
  }

  /** Paths changed between the tips (memoized per guard); `null` if git could not say. */
  changedPaths(): Promise<ReadonlySet<string> | null> {
    if (this.changed === undefined) this.changed = this.sources.changedBetweenTips();
    return this.changed;
  }

  /** The repo-relative paths the trunk must never fill, as a list (for SQL). */
  async refusedPaths(): Promise<string[]> {
    return [...((await this.changedPaths()) ?? [])];
  }

  /** Keep the fallback entries the rule admits, in their original order. */
  async admit<T>(entries: readonly T[], pathOf: (entry: T) => string | undefined): Promise<T[]> {
    if (entries.length === 0) return [];
    const changed = await this.changedPaths();
    const rels = entries.map((entry) => {
      const path = pathOf(entry);
      return path ? this.relative(path) : undefined;
    });
    const candidates = rels.filter(
      (rel): rel is string => rel !== undefined && !(changed?.has(rel) ?? false)
    );
    const owned =
      candidates.length > 0 ? this.sources.trackedOnBranch(candidates) : new Set<string>();
    return entries.filter((_, i) => {
      const rel = rels[i];
      if (rel === undefined) return true;
      if (changed?.has(rel)) return false;
      return !owned.has(rel);
    });
  }
}

/**
 * Apply the guard to the entries the caller's branch does NOT carry — the
 * trunk's fill-in in a lane that fetched both branches at once (the vector
 * lane's `should: [branch, fallbackBranch]`, retrieve). Entries carrying the
 * caller's branch, or with no readable branch, pass untouched.
 */
export async function admitFallbackEntries<T>(
  entries: readonly T[],
  guard: FallbackGuard | undefined,
  pathOf: (entry: T) => string | undefined,
  branchOf: (entry: T) => unknown
): Promise<T[]> {
  if (!guard) return [...entries];
  const fallbackOnly = new Set<T>();
  for (const entry of entries) {
    const branches = normalizeBranchList(branchOf(entry));
    if (branches.length > 0 && !branches.includes(guard.branch)) fallbackOnly.add(entry);
  }
  if (fallbackOnly.size === 0) return [...entries];
  const admitted = new Set(await guard.admit([...fallbackOnly], pathOf));
  return entries.filter((entry) => !fallbackOnly.has(entry) || admitted.has(entry));
}

/**
 * Filter an auto-widened (`branch:"*"`) result through the guard. The widen
 * runs when the caller's branch answered nothing, so every widened entry is
 * another branch's copy; one whose path the caller's branch deleted or changed
 * is that other branch's content, not the caller's, and must not come back as
 * if it were (emnify: a symbol only in a file the branch had deleted). Returns
 * the admitted entries and how many were withheld.
 */
export async function guardWidened<T>(
  entries: readonly T[],
  guard: FallbackGuard | undefined,
  pathOf: (entry: T) => string | undefined
): Promise<{ kept: T[]; withheld: number }> {
  if (!guard) return { kept: [...entries], withheld: 0 };
  const kept = await guard.admit(entries, pathOf);
  return { kept, withheld: entries.length - kept.length };
}

/** The sentence a widened response appends when the guard withheld matches. */
export function withheldNote(withheld: number, branch: string): string {
  return (
    `${withheld} match(es) were withheld: they are in files that branch "${branch}" ` +
    `deleted or changed, so another branch's copy is not yours. Pass branch:"*" to see them.`
  );
}

/** The state-manager slice the guard reads. */
interface TrackedOnBranchSource {
  getPathsTrackedOnBranch(
    watchFolderId: string,
    branch: string,
    relativePaths: readonly string[]
  ): ReadonlySet<string>;
}

/**
 * Build the guard for a read that falls back from `branch` to `fallbackBranch`,
 * or `undefined` when there is no fallback in play.
 */
export function createFallbackGuard(params: {
  stateManager: TrackedOnBranchSource | null | undefined;
  watchFolderId: string | null | undefined;
  projectRoot: string | undefined;
  branch: string | undefined;
  fallbackBranch: string | undefined;
}): FallbackGuard | undefined {
  const { stateManager, watchFolderId, projectRoot, branch, fallbackBranch } = params;
  if (!branch || branch === '*' || !fallbackBranch || fallbackBranch === branch) return undefined;
  return new FallbackGuard(branch, fallbackBranch, projectRoot, {
    // Best-effort like every annotation lookup on the read path: an index that
    // cannot answer leaves the git rule in charge rather than failing the read.
    trackedOnBranch: (paths) => {
      if (!watchFolderId || typeof stateManager?.getPathsTrackedOnBranch !== 'function') {
        return new Set<string>();
      }
      try {
        return stateManager.getPathsTrackedOnBranch(watchFolderId, branch, paths);
      } catch {
        return new Set<string>();
      }
    },
    changedBetweenTips: () =>
      projectRoot
        ? getPathsChangedBetween(projectRoot, fallbackBranch, branch)
        : Promise.resolve(null),
  });
}
