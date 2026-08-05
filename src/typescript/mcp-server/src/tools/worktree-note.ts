/**
 * Shared worktree-read note for the project-scoped read tools (`grep`, `search`).
 *
 * The worktree-membership model indexes a linked worktree's content under the
 * MAIN repo's folder, so result `file` paths come back MAIN-anchored. A caller
 * whose cwd is inside a worktree must `Read` that worktree's OWN copy — which,
 * for a file that diverges on the branch, differs from main. Rather than have the
 * caller translate every path by hand (field feedback from worktree sessions),
 * the response carries this note once. The canonical main-anchored path stays on
 * each match so tool round-trips keep working (`retrieve`/`grep` resolve by that
 * path); the note only explains how to reach the worktree copy for a `Read`.
 */

import { getWorktreeContext } from '../utils/request-context.js';

export interface WorktreeReadNote {
  /** The worktree directory name (`.claude/worktrees/<name>`). */
  name: string;
  /** The worktree root — the prefix to swap in for the repo root when Reading. */
  root: string;
  /** One-line, actionable translation guidance. */
  note: string;
}

/**
 * The worktree note when the caller's cwd is inside a linked worktree, else
 * `undefined`. Path-shape only (via {@link getWorktreeContext}) — no disk/DB.
 */
export function worktreeReadNote(): WorktreeReadNote | undefined {
  const wt = getWorktreeContext();
  if (!wt) return undefined;
  return {
    name: wt.name,
    root: wt.root,
    note:
      `cwd is in worktree '${wt.name}'. Result 'file' paths are MAIN-anchored (worktree content ` +
      `is indexed under the main folder). To Read this worktree's own copy, swap the repo root — ` +
      `the path up to '/.claude/worktrees/' — for '${wt.root}'. A file that diverges on the branch ` +
      `differs from main; an identical file is the same content either way.`,
  };
}
