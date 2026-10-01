/**
 * Git utility functions for repository detection and remote URL parsing.
 *
 * Behavior:
 * - `.git` is accepted both as a directory (main repo) and as a file
 *   (linked worktree — contains `gitdir: <path>` pointing to the
 *   worktree's git dir under the main repo's `.git/worktrees/<n>/`).
 * - Remote URL resolution shells out to `git config --get` so worktrees
 *   resolve via the shared common config without us having to follow
 *   the `gitdir:` indirection manually.
 */

import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync, statSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';

/**
 * Check if a path is a git repository.
 *
 * A repo root is identified by the presence of a `.git` entry, which may
 * be a directory (main worktree) or a file (linked worktree). Both forms
 * are valid — we do not require it to be a directory.
 */
export function isGitRepository(path: string): boolean {
  try {
    return existsSync(join(path, '.git'));
  } catch {
    return false;
  }
}

/**
 * Find git repository root by walking up from a path.
 * Returns the path containing .git (file or directory), or null if not found.
 */
export function findGitRoot(startPath: string): string | null {
  let currentPath = resolve(startPath);
  const MAX_DEPTH = 20;

  for (let i = 0; i < MAX_DEPTH; i++) {
    if (existsSync(join(currentPath, '.git'))) {
      return currentPath;
    }
    const parent = dirname(currentPath);
    if (parent === currentPath) break;
    currentPath = parent;
  }
  return null;
}

/**
 * Get the git remote URL for a repository.
 *
 * Uses `git config --get remote.origin.url` so linked worktrees resolve
 * via the shared `.git` indirection automatically. Returns `null` when:
 * - `git` is not on PATH;
 * - the path is not a repo;
 * - no `remote.origin` is configured;
 * - the git command times out or fails for any other reason.
 *
 * This is for informational purposes only — the daemon computes the
 * authoritative project_id.
 */
export function getGitRemoteUrl(repoPath: string): string | null {
  return runGit(repoPath, ['config', '--get', 'remote.origin.url']);
}

/**
 * Detect whether a repo root is a linked worktree.
 *
 * In a linked worktree, `.git` is a regular file whose contents are
 * `gitdir: <path>` pointing to the worktree's git dir under the main
 * repo's `.git/worktrees/<n>/`. In a main worktree (or a non-worktree
 * clone), `.git` is a directory.
 *
 * Returns `false` when `.git` is missing.
 */
export function isWorktree(repoRoot: string): boolean {
  try {
    return statSync(join(repoRoot, '.git')).isFile();
  } catch {
    return false;
  }
}

/**
 * Resolve the shared git directory (the `--git-common-dir`).
 *
 * For a main worktree, this is `<repo>/.git`. For a linked worktree,
 * it's the main repo's `.git`, reached by following the `gitdir:`
 * pointer in the worktree's `.git` file and then walking up.
 *
 * Returns `null` when the path is not a repo or `git` is unavailable.
 */
export function getGitCommonDir(repoRoot: string): string | null {
  const out = runGit(repoRoot, ['rev-parse', '--git-common-dir']);
  if (out === null) return null;
  // `git --git-common-dir` may return a relative path; resolve against repoRoot.
  return resolve(repoRoot, out);
}

/**
 * Current branch name reported by `git rev-parse --abbrev-ref HEAD`.
 *
 * Returns `"HEAD"` in detached-HEAD state (mirroring git's own behavior).
 * Returns `null` when `git` is unavailable or the path is not a repo.
 */
export function getCurrentBranch(repoRoot: string): string | null {
  return runGit(repoRoot, ['rev-parse', '--abbrev-ref', 'HEAD']);
}

/**
 * SHA of the current HEAD commit.
 *
 * Returns `null` on empty repos (no commits yet) or when `git` is
 * unavailable / the path is not a repo.
 */
export function getHeadCommit(repoRoot: string): string | null {
  return runGit(repoRoot, ['rev-parse', 'HEAD']);
}

/**
 * How long a FAILED default-branch lookup is remembered. The lookup runs git
 * synchronously on the request path, so not remembering a failure would stall
 * every read while git is unhealthy — and remembering it forever (the first
 * version did) turned one slow filesystem moment into a permanent downgrade to
 * the index heuristic for that repo, until the server restarted.
 */
const DEFAULT_BRANCH_FAILURE_TTL_MS = 60_000;

/** The refs that decide the trunk, in one `git for-each-ref` call. */
const DEFAULT_BRANCH_REFS = ['refs/remotes/origin/HEAD', 'refs/heads/main', 'refs/heads/master'];

interface CachedDefaultBranch {
  value: string | null;
  /** `Infinity` for a definitive answer; a short TTL for a failed lookup. */
  expiresAt: number;
}

const defaultBranchCache = new Map<string, CachedDefaultBranch>();

/** Drop the {@link getDefaultBranch} cache (tests, and after a remote re-point). */
export function clearDefaultBranchCache(): void {
  defaultBranchCache.clear();
}

/**
 * Run `git for-each-ref` for {@link DEFAULT_BRANCH_REFS}. Returns its output, or
 * `null` when git could not answer (not a repo, git missing, timeout) — which
 * is different from git answering "none of those refs exist" (empty output).
 */
export type ForEachRefRunner = (repoRoot: string) => string | null;

const runForEachRef: ForEachRefRunner = (repoRoot) => {
  try {
    return execFileSync(
      'git',
      ['-C', repoRoot, 'for-each-ref', '--format=%(refname)%09%(symref)', ...DEFAULT_BRANCH_REFS],
      { encoding: 'utf-8', stdio: ['ignore', 'pipe', 'ignore'], timeout: 2000, windowsHide: true }
    );
  } catch {
    return null;
  }
};

/**
 * The repository's DEFAULT branch — the trunk, not the checked-out branch.
 *
 * This is the authority for "am I on the project's main line?", which decides
 * whether a read should widen to a base branch at all. Deriving it from the
 * INDEX instead (e.g. "the branch most files are tracked under, excluding mine")
 * is what let a read on the trunk widen into an abandoned sibling branch and
 * serve its stale content generations — the branch with the most tracked files
 * is not reliably the trunk (measured: a feature branch held 98.7% of one
 * project's files against the trunk's 97.3%).
 *
 * Precedence: `refs/remotes/origin/HEAD` (what the remote itself calls
 * default), then a local `main`, then a local `master`. Deliberately NOT
 * `init.defaultBranch`: that config names the branch git creates in NEW
 * repositories — a user-wide preference, not this repository's trunk — and
 * `remote.origin.defaultBranch` is not a git setting at all. One subprocess
 * answers all three questions; the first version spawned up to five, each with
 * a 2 s timeout, synchronously on the request path.
 *
 * Returns `null` when none resolve — callers then fall back to their own
 * heuristic rather than guessing a name that does not exist in the repo.
 */
export function getDefaultBranch(repoRoot: string): string | null {
  return getDefaultBranchWith(repoRoot, runForEachRef, Date.now);
}

/** {@link getDefaultBranch} with an injectable git runner and clock (tests). */
export function getDefaultBranchWith(
  repoRoot: string,
  run: ForEachRefRunner,
  now: () => number
): string | null {
  const cached = defaultBranchCache.get(repoRoot);
  if (cached && cached.expiresAt > now()) return cached.value;
  const output = run(repoRoot);
  const value = output === null ? null : parseDefaultBranch(output);
  defaultBranchCache.set(repoRoot, {
    value,
    expiresAt: output === null ? now() + DEFAULT_BRANCH_FAILURE_TTL_MS : Number.POSITIVE_INFINITY,
  });
  return value;
}

/**
 * Pick the trunk out of `for-each-ref --format=%(refname)%09%(symref)` output.
 * Ref names are compared exactly: `for-each-ref` treats a pattern as a prefix,
 * so `refs/heads/main` also lists a branch called `main/experiment`.
 */
export function parseDefaultBranch(forEachRefOutput: string): string | null {
  const symrefByRef = new Map<string, string>();
  for (const line of forEachRefOutput.split('\n')) {
    const [refname, symref = ''] = line.split('\t');
    if (refname && refname.trim().length > 0) symrefByRef.set(refname.trim(), symref.trim());
  }
  const remotePrefix = 'refs/remotes/origin/';
  const remoteHead = symrefByRef.get('refs/remotes/origin/HEAD');
  // `refs/remotes/origin/release/2.0` → `release/2.0`: only the remote prefix
  // is stripped, so a trunk whose own name contains a slash survives intact.
  if (remoteHead?.startsWith(remotePrefix) && remoteHead.length > remotePrefix.length) {
    return remoteHead.slice(remotePrefix.length);
  }
  if (symrefByRef.has('refs/heads/main')) return 'main';
  if (symrefByRef.has('refs/heads/master')) return 'master';
  return null;
}

/**
 * Aggregate git state for a repo root.
 *
 * Combines the primitives above into a single object suitable for passing
 * over the wire (gRPC, MCP). All fields are best-effort: any individual
 * field may be `null` if its underlying git command fails, without
 * affecting the others. Returns `null` only when `repoRoot` is not a git
 * repo at all.
 */
export interface GitState {
  readonly repoRoot: string;
  readonly branch: string | null;
  readonly commit: string | null;
  readonly remoteUrl: string | null;
  readonly isWorktree: boolean;
  readonly worktreePath: string | null;
  readonly commonDir: string | null;
}

export function getGitState(repoRoot: string): GitState | null {
  if (!isGitRepository(repoRoot)) return null;
  const worktree = isWorktree(repoRoot);
  return {
    repoRoot,
    branch: getCurrentBranch(repoRoot),
    commit: getHeadCommit(repoRoot),
    remoteUrl: getGitRemoteUrl(repoRoot),
    isWorktree: worktree,
    worktreePath: worktree ? repoRoot : null,
    commonDir: getGitCommonDir(repoRoot),
  };
}

/**
 * Run a `git` command in the given repo, returning trimmed stdout or
 * `null` on any failure. Internal helper used by the typed accessors
 * above.
 */
function runGit(repoRoot: string, args: ReadonlyArray<string>): string | null {
  try {
    const out = execFileSync('git', ['-C', repoRoot, ...args], {
      encoding: 'utf-8',
      stdio: ['ignore', 'pipe', 'ignore'],
      timeout: 2000,
      windowsHide: true,
    });
    const trimmed = out.trim();
    return trimmed.length > 0 ? trimmed : null;
  } catch {
    return null;
  }
}

/**
 * Branch checked out in the linked worktree `<mainRepoRoot>/.claude/worktrees/<name>`,
 * read from the MAIN checkout's `.git/worktrees/<id>/HEAD`. No git spawn, and
 * nothing is read from the worktree's own path beyond its `.git` gitlink under
 * the main folder — the worktree path a client reports is a HOST path the
 * server (in a container) cannot open. `<id>` is normally the directory name;
 * git disambiguates collisions with a suffix, so the gitlink's recorded id is
 * preferred when readable. Returns null when detached or unreadable.
 */
export function readLinkedWorktreeBranch(
  mainRepoRoot: string,
  worktreeName: string
): string | null {
  try {
    let id = worktreeName;
    try {
      const gitlink = readFileSync(
        join(mainRepoRoot, '.claude', 'worktrees', worktreeName, '.git'),
        'utf-8'
      );
      const m = /^gitdir:\s*(.+?)\s*$/m.exec(gitlink);
      const last = m?.[1]
        ?.replace(/[\\/]+$/, '')
        .split(/[\\/]/)
        .pop();
      if (last) id = last;
    } catch {
      // No gitlink readable here — fall back to the directory name.
    }
    const head = readFileSync(join(mainRepoRoot, '.git', 'worktrees', id, 'HEAD'), 'utf-8').trim();
    const ref = /^ref:\s*refs\/heads\/(.+)$/.exec(head);
    return ref?.[1] ?? null;
  } catch {
    return null;
  }
}
