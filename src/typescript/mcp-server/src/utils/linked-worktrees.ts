/**
 * Linked git worktrees of a registered project, read from the MAIN checkout.
 *
 * A caller working in a linked worktree reports a cwd the project registry
 * does not know: the daemon indexes worktrees under the main folder (no
 * watch folder of their own), and agent tooling often puts them OUTSIDE the
 * repository (`~/.codex/worktrees/<n>/<repo>`, `<repo>-worktrees/<name>`). The
 * longest-prefix match against registered paths then missed the project
 * entirely, and even a cwd under `<repo>/.claude/worktrees/<name>` read the
 * MAIN checkout's branch instead of the worktree's — so agents had to pass
 * `projectId` and `branch` on every call (field feedback 2026-11-11, tecsul:
 * 12 worktrees outside the repo).
 *
 * The main checkout's `.git/worktrees/<id>/` already records every linked
 * worktree: `gitdir` holds `<worktree root>/.git` and `HEAD` its branch. Both
 * live under the main repository, which the server can read, so nothing is
 * read from the worktree's own path — a host path the server (in a container)
 * may not be able to open.
 */

import { readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';

import { canonicalizeHostPath } from '../clients/project-queries.js';

export interface LinkedWorktree {
  /** The worktree root as git recorded it (canonicalized for comparison). */
  root: string;
  /** The checked-out branch, or `null` for a detached HEAD. */
  branch: string | null;
}

/** How long one main checkout's worktree list is reused. */
const CACHE_TTL_MS = 15_000;

const cache = new Map<string, { at: number; worktrees: LinkedWorktree[] }>();

/** Drop the cache (tests). */
export function clearLinkedWorktreeCache(): void {
  cache.clear();
}

/**
 * Every linked worktree of the repository whose main checkout is `mainRoot`.
 * Empty when it has none or its `.git/worktrees` is unreadable.
 */
export function listLinkedWorktrees(
  mainRoot: string,
  now: () => number = Date.now
): LinkedWorktree[] {
  const cached = cache.get(mainRoot);
  if (cached && now() - cached.at < CACHE_TTL_MS) return cached.worktrees;
  const worktrees = readLinkedWorktrees(mainRoot);
  cache.set(mainRoot, { at: now(), worktrees });
  return worktrees;
}

function readLinkedWorktrees(mainRoot: string): LinkedWorktree[] {
  const adminDir = join(mainRoot, '.git', 'worktrees');
  let ids: string[];
  try {
    ids = readdirSync(adminDir);
  } catch {
    return [];
  }
  const out: LinkedWorktree[] = [];
  for (const id of ids) {
    try {
      const gitdir = readFileSync(join(adminDir, id, 'gitdir'), 'utf-8').trim();
      // `<root>/.git` (either separator) → `<root>`.
      const root = canonicalizeHostPath(gitdir.replace(/[\\/]\.git[\\/]?$/, ''));
      if (root.length === 0) continue;
      const head = readFileSync(join(adminDir, id, 'HEAD'), 'utf-8').trim();
      const ref = /^ref:\s*refs\/heads\/(.+)$/.exec(head);
      out.push({ root, branch: ref?.[1] ?? null });
    } catch {
      // A half-created or pruned admin entry: skip it.
    }
  }
  return out;
}

/**
 * The linked worktree of `mainRoot` that contains `cwd` (the deepest one, for
 * worktrees nested in one another), or `undefined` when `cwd` lies in none.
 * Paths are compared canonicalized and case-insensitively, like the project
 * registry's own prefix match, so a Windows-host cwd over UNC matches too.
 */
export function linkedWorktreeOf(
  cwd: string,
  mainRoot: string,
  now: () => number = Date.now
): LinkedWorktree | undefined {
  const target = canonicalizeHostPath(cwd).toLowerCase();
  let best: LinkedWorktree | undefined;
  for (const wt of listLinkedWorktrees(mainRoot, now)) {
    const root = wt.root.toLowerCase();
    if (
      (target === root || target.startsWith(`${root}/`)) &&
      root.length > (best?.root.length ?? -1)
    ) {
      best = wt;
    }
  }
  return best;
}
