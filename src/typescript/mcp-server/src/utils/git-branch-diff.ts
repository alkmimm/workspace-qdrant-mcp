/**
 * Paths whose content differs between two branch tips, from git.
 *
 * A branch-scoped read fills in from the trunk only for files the branch has
 * NOT changed — the trunk's copy is the branch's copy only when git says the
 * two tips agree on that path. A path git reports between the tips (modified,
 * added or deleted on either side) has no valid trunk copy for the branch:
 * filling it served files the branch had deleted and the trunk's version of
 * files it had changed (emnify-sms-sender, 2026-10-03).
 *
 * Every git call here is ASYNC: these run on the MCP request path, and a
 * synchronous spawn blocks the event loop — every concurrent request — for as
 * long as git takes. Answers are cached as PROMISES, so concurrent misses for
 * the same key share one spawn. Paths are relative to `repoRoot`
 * (`git diff --relative`, `ls-tree … -- .`), matching `tracked_files`, even
 * when the project is registered at a subdirectory of its git repository.
 */

import { execFile } from 'node:child_process';

/** How long an answer (or a failure) is reused: one agent turn, not a session. */
const BRANCH_DIFF_TTL_MS = 30_000;

/** Runs git with argv in `repoRoot`; resolves stdout, or `null` when git failed. */
export type GitRunner = (repoRoot: string, args: readonly string[]) => Promise<string | null>;

const runGitRaw: GitRunner = (repoRoot, args) =>
  new Promise((resolve) => {
    execFile(
      'git',
      ['-C', repoRoot, ...args],
      {
        encoding: 'utf-8',
        timeout: 5000,
        maxBuffer: 32 * 1024 * 1024,
        windowsHide: true,
      },
      (error, stdout) => resolve(error ? null : stdout)
    );
  });

interface Cached<T> {
  value: Promise<T>;
  expiresAt: number;
}

const diffCache = new Map<string, Cached<ReadonlySet<string> | null>>();
const tipCountCache = new Map<string, Cached<number | null>>();

/** Drop the caches (tests). */
export function clearBranchDiffCache(): void {
  diffCache.clear();
  tipCountCache.clear();
}

/** Reuse a live cached promise for `key`, else start `compute` and cache it. */
function cached<T>(
  cache: Map<string, Cached<T>>,
  key: string,
  now: () => number,
  compute: () => Promise<T>
): Promise<T> {
  const hit = cache.get(key);
  if (hit && hit.expiresAt > now()) return hit.value;
  const value = compute();
  cache.set(key, { value, expiresAt: now() + BRANCH_DIFF_TTL_MS });
  return value;
}

/**
 * Paths (relative to `repoRoot`) that differ between the tips of `base` and
 * `branch`, or `null` when git cannot answer (not a repo, a ref missing,
 * timeout). A local branch is preferred over `origin/<name>` for either side.
 */
export function getPathsChangedBetween(
  repoRoot: string,
  base: string,
  branch: string
): Promise<ReadonlySet<string> | null> {
  return getPathsChangedBetweenWith(repoRoot, base, branch, runGitRaw, Date.now);
}

/** {@link getPathsChangedBetween} with an injectable git runner and clock (tests). */
export function getPathsChangedBetweenWith(
  repoRoot: string,
  base: string,
  branch: string,
  run: GitRunner,
  now: () => number
): Promise<ReadonlySet<string> | null> {
  return cached(diffCache, `${repoRoot}\0${base}\0${branch}`, now, () =>
    computeChangedPaths(repoRoot, base, branch, run)
  );
}

async function computeChangedPaths(
  repoRoot: string,
  base: string,
  branch: string,
  run: GitRunner
): Promise<ReadonlySet<string> | null> {
  const present = await existingRefs(repoRoot, [base, branch], run);
  if (present === null) return null;
  const baseRef = pickRef(present, base);
  const branchRef = pickRef(present, branch);
  if (!baseRef || !branchRef) return null;
  const diffOut = await run(repoRoot, [
    'diff',
    '--relative',
    '--no-renames',
    '--name-only',
    '-z',
    baseRef,
    branchRef,
    '--',
  ]);
  if (diffOut === null) return null;
  return new Set(diffOut.split('\0').filter((p) => p.length > 0));
}

/**
 * Which of `refs/heads/<n>` / `refs/remotes/origin/<n>` exist for `names`.
 * for-each-ref patterns are prefixes (`refs/heads/main` also lists
 * `refs/heads/main/x`), so the returned names are matched exactly.
 */
async function existingRefs(
  repoRoot: string,
  names: readonly string[],
  run: GitRunner
): Promise<ReadonlySet<string> | null> {
  const candidates = names.flatMap((name) => [
    `refs/heads/${name}`,
    `refs/remotes/origin/${name}`,
  ]);
  const out = await run(repoRoot, ['for-each-ref', '--format=%(refname)', ...candidates]);
  if (out === null) return null;
  return new Set(
    out
      .split('\n')
      .map((l) => l.trim())
      .filter((l) => l.length > 0)
  );
}

function pickRef(present: ReadonlySet<string>, name: string): string | undefined {
  for (const ref of [`refs/heads/${name}`, `refs/remotes/origin/${name}`]) {
    if (present.has(ref)) return ref;
  }
  return undefined;
}

/** A checkout of the repository: the main folder or a linked worktree. */
export interface BranchCheckout {
  path: string;
  /** `undefined` for a detached HEAD. */
  branch: string | undefined;
}

/** Every checkout of `repoRoot` (main first), from `git worktree list --porcelain`. */
export async function listBranchCheckoutsWith(
  repoRoot: string,
  run: GitRunner
): Promise<BranchCheckout[] | null> {
  const out = await run(repoRoot, ['worktree', 'list', '--porcelain']);
  if (out === null) return null;
  const checkouts: BranchCheckout[] = [];
  let current: BranchCheckout | undefined;
  for (const line of out.split('\n')) {
    if (line.startsWith('worktree ')) {
      current = { path: line.slice('worktree '.length).trim(), branch: undefined };
      checkouts.push(current);
    } else if (current && line.startsWith('branch refs/heads/')) {
      current.branch = line.slice('branch refs/heads/'.length).trim();
    }
  }
  return checkouts;
}

/** {@link listBranchCheckoutsWith} with the real git. */
export function listBranchCheckouts(repoRoot: string): Promise<BranchCheckout[] | null> {
  return listBranchCheckoutsWith(repoRoot, runGitRaw);
}

/**
 * Number of files under `repoRoot` in the tip of `branch` (local ref, else
 * `origin/<name>`), or `null` when git cannot say. Cached like the diff.
 */
export function countFilesAtBranchTipWith(
  repoRoot: string,
  branch: string,
  run: GitRunner,
  now: () => number
): Promise<number | null> {
  return cached(tipCountCache, `${repoRoot}\0${branch}`, now, async () => {
    const present = await existingRefs(repoRoot, [branch], run);
    const ref = present ? pickRef(present, branch) : undefined;
    if (!ref) return null;
    const listing = await run(repoRoot, ['ls-tree', '-r', '--name-only', '-z', ref, '--', '.']);
    return listing === null ? null : listing.split('\0').filter((p) => p.length > 0).length;
  });
}

/** {@link countFilesAtBranchTipWith} with the real git and clock. */
export function countFilesAtBranchTip(repoRoot: string, branch: string): Promise<number | null> {
  return countFilesAtBranchTipWith(repoRoot, branch, runGitRaw, Date.now);
}
