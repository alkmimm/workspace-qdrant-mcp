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
 *
 * Each branch resolves to the tip the daemon indexes it at (its
 * `git::followed_trunk`): the trunk, unless the main folder has it checked
 * out, at `origin/<trunk>` when that fast-forwards the local trunk — tecsul's
 * local `release/platform` sat 1,406 commits behind origin while every agent
 * branch came off origin's tip (2026-11-11), so diffing against the stale
 * local tip reported thousands of "changed" paths the index held right.
 */

import { execFile } from 'node:child_process';
import { recordCacheLookup, type CacheName } from '../telemetry/metrics.js';
import { getDefaultBranch } from './git-utils.js';

/** How long an answer (or a failure) is reused: one agent turn, not a session. */
const BRANCH_DIFF_TTL_MS = 30_000;

/** Runs git with argv in `repoRoot`; resolves stdout, or `null` when git failed. */
export type GitRunner = (repoRoot: string, args: readonly string[]) => Promise<string | null>;

/** Names the repository's trunk (`getDefaultBranch`), or `null`. */
export type TrunkLookup = (repoRoot: string) => string | null;

const noTrunk: TrunkLookup = () => null;

/**
 * The repository's trunk, looked up only when a name has both a local and an
 * origin ref to choose between — never for reads that do not need it.
 */
const repoTrunk: TrunkLookup = (repoRoot) => getDefaultBranch(repoRoot);

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
  name: CacheName,
  cache: Map<string, Cached<T>>,
  key: string,
  now: () => number,
  compute: () => Promise<T>
): Promise<T> {
  const hit = cache.get(key);
  const fresh = hit !== undefined && hit.expiresAt > now();
  recordCacheLookup(name, fresh);
  if (fresh) return hit.value;
  const value = compute();
  cache.set(key, { value, expiresAt: now() + BRANCH_DIFF_TTL_MS });
  return value;
}

/**
 * Paths (relative to `repoRoot`) that differ between the tips of `base` and
 * `branch`, or `null` when git cannot answer (not a repo, a ref missing,
 * timeout). Each side resolves as {@link tipRef} says.
 */
export function getPathsChangedBetween(
  repoRoot: string,
  base: string,
  branch: string
): Promise<ReadonlySet<string> | null> {
  return getPathsChangedBetweenWith(repoRoot, base, branch, runGitRaw, Date.now, repoTrunk);
}

/** {@link getPathsChangedBetween} with an injectable git runner, clock and trunk (tests). */
export function getPathsChangedBetweenWith(
  repoRoot: string,
  base: string,
  branch: string,
  run: GitRunner,
  now: () => number,
  trunkOf: TrunkLookup = noTrunk
): Promise<ReadonlySet<string> | null> {
  return cached('branch_diff', diffCache, `${repoRoot}\0${base}\0${branch}`, now, () =>
    computeChangedPaths(repoRoot, base, branch, run, trunkOf)
  );
}

async function computeChangedPaths(
  repoRoot: string,
  base: string,
  branch: string,
  run: GitRunner,
  trunkOf: TrunkLookup
): Promise<ReadonlySet<string> | null> {
  const present = await existingRefs(repoRoot, [base, branch], run);
  if (present === null) return null;
  const baseRef = await tipRef(repoRoot, base, present, run, trunkOf);
  const branchRef = await tipRef(repoRoot, branch, present, run, trunkOf);
  if (baseRef === undefined || branchRef === undefined) return null;
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
  const candidates = names.flatMap((name) => [`refs/heads/${name}`, `refs/remotes/origin/${name}`]);
  const out = await run(repoRoot, ['for-each-ref', '--format=%(refname)', ...candidates]);
  if (out === null) return null;
  return new Set(
    out
      .split('\n')
      .map((l) => l.trim())
      .filter((l) => l.length > 0)
  );
}

/**
 * The ref the daemon indexes `name` at (`git::followed_trunk`). The trunk,
 * unless the main folder has it checked out, is held at `origin/<trunk>` when
 * that fast-forwards the local trunk (what `git pull --ff-only` would check
 * out); a local trunk with commits of its own stays local. Every other branch:
 * its local ref, else `origin/<name>`. `undefined` when neither exists.
 */
async function tipRef(
  repoRoot: string,
  name: string,
  present: ReadonlySet<string>,
  run: GitRunner,
  trunkOf: TrunkLookup
): Promise<string | undefined> {
  const local = `refs/heads/${name}`;
  const origin = `refs/remotes/origin/${name}`;
  if (!present.has(local)) return present.has(origin) ? origin : undefined;
  if (!present.has(origin) || trunkOf(repoRoot) !== name) return local;
  const head = await run(repoRoot, ['symbolic-ref', '-q', 'HEAD']);
  if (head?.trim() === local) return local; // the watcher follows that disk
  // Exit 0 (empty stdout) when local is an ancestor of origin, or equal to it.
  const fastForward = await run(repoRoot, ['merge-base', '--is-ancestor', local, origin]);
  return fastForward === null ? local : origin;
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
 * Number of files under `repoRoot` in the tip of `branch` (as {@link tipRef}
 * resolves it), or `null` when git cannot say. Cached like the diff.
 */
export function countFilesAtBranchTipWith(
  repoRoot: string,
  branch: string,
  run: GitRunner,
  now: () => number,
  trunkOf: TrunkLookup = noTrunk
): Promise<number | null> {
  return cached('branch_tip_count', tipCountCache, `${repoRoot}\0${branch}`, now, async () => {
    const present = await existingRefs(repoRoot, [branch], run);
    const ref = present ? await tipRef(repoRoot, branch, present, run, trunkOf) : undefined;
    if (ref === undefined) return null;
    const listing = await run(repoRoot, ['ls-tree', '-r', '--name-only', '-z', ref, '--', '.']);
    return listing === null ? null : listing.split('\0').filter((p) => p.length > 0).length;
  });
}

/** {@link countFilesAtBranchTipWith} with the real git, clock and trunk. */
export function countFilesAtBranchTip(repoRoot: string, branch: string): Promise<number | null> {
  return countFilesAtBranchTipWith(repoRoot, branch, runGitRaw, Date.now, repoTrunk);
}
