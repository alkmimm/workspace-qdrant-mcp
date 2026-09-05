/**
 * Write-time provenance for scratchpad notes.
 *
 * Scratchpad reads are deliberately branch-agnostic (a note must survive a
 * checkout), and the queue item's `branch` stays "main" because the point id
 * derives from `(tenant, branch, document_id)` — changing it would fork a
 * note's identity across branches. Provenance therefore travels in dedicated
 * payload fields stamped at write time:
 *
 *   - `origin_branch`:   the branch checked out where the note was written
 *   - `origin_cwd`:      the client working directory the write came from
 *                        (a worktree path identifies the worktree)
 *   - `origin_worktree`: whether that checkout is a linked git worktree
 *
 * All fields are best-effort ATTRIBUTION, never scoping filters: a field the
 * server cannot determine is omitted, not fabricated.
 *
 * Origin means "where the write came from": when a note targets another
 * project via an explicit `projectId` (or the session rung), the writer's own
 * location is still the correct attribution — callers pass the project the CWD
 * resolves to (`cwdProjectPath`), never the target tenant's path.
 *
 * Worktree cwd (2026-09-05): a client working in `.claude/worktrees/<name>`
 * usually resolves to the MAIN project (worktree content is indexed under the
 * main folder), so the session branch and `getCurrentBranch(projectPath)` both
 * name the main checkout's branch — and `isWorktree(projectPath)` is false. A
 * note written from a worktree was therefore stamped with the wrong branch and
 * `origin_worktree:false`, while `search`/`grep` on the same cwd emitted their
 * worktree note. The shape of the cwd being stamped as `origin_cwd` now
 * decides: inside a worktree the branch is read from the main checkout's
 * `.git/worktrees/<id>/HEAD` (the worktree path itself is a HOST path the
 * server, in a container, cannot open). A daemon-REGISTERED worktree resolves
 * to its own path; that path is worktree-shaped too, so the main checkout is
 * derived from it. When the HEAD is unreadable the branch is omitted rather
 * than borrowed.
 */
import type { SessionState } from '../server-types.js';
import type { ProjectDetector } from '../utils/project-detector.js';
import { getRequestContext, getEffectiveCwd, worktreeContextOf } from '../utils/request-context.js';
import { getCurrentBranch, isWorktree, readLinkedWorktreeBranch } from '../utils/git-utils.js';
import { resolveProjectIdentity } from './branch-scope.js';

/** Provenance fields, named exactly as persisted in the Qdrant payload. */
export interface ScratchpadOrigin {
  origin_branch?: string;
  origin_cwd?: string;
  origin_worktree?: boolean;
}

type OriginSessionState = Pick<SessionState, 'currentBranch' | 'isWorktree'>;

/**
 * The client cwd to attribute a write to, or `undefined` when unknowable.
 *
 * On HTTP a request context is bound: use its `hostCwd` when present, and
 * refuse to stamp anything when it is absent — falling through to
 * `process.cwd()` would attribute the note to the container WORKDIR.
 * Without a request context (stdio) `getEffectiveCwd()` is client-side
 * (spawn cwd or `WQM_DEFAULT_HOST_CWD`) and safe to stamp.
 */
function resolveOriginCwd(): string | undefined {
  const ctx = getRequestContext();
  if (ctx?.hostCwd !== undefined && ctx.hostCwd.length > 0) return ctx.hostCwd;
  if (!ctx) return getEffectiveCwd();
  return undefined;
}

/** Registered path of the project the CWD resolves to, or `undefined`. */
async function detectCwdProjectPath(
  projectDetector: ProjectDetector | undefined
): Promise<string | undefined> {
  if (!projectDetector) return undefined;
  try {
    return (await resolveProjectIdentity(projectDetector, undefined)).projectPath;
  } catch {
    return undefined;
  }
}

function nonEmpty(value: string | null | undefined): value is string {
  return value !== null && value !== undefined && value !== '';
}

/**
 * The main checkout for a registered path that may itself be a linked worktree
 * root (`<main>/.claude/worktrees/<name>`, either separator): the daemon
 * registers worktrees as their own rows, so the cwd's project can BE the
 * worktree. The `.git/worktrees/<id>/HEAD` metadata lives in the main checkout.
 */
function mainCheckoutOf(path: string): string {
  const wt = worktreeContextOf(path);
  if (!wt) return path;
  return wt.root.slice(0, wt.root.length - ('/.claude/worktrees/'.length + wt.name.length));
}

/**
 * Resolve the provenance to stamp on a scratchpad write.
 *
 * Precedence for `origin_branch`: explicit `branch` argument > the linked
 * worktree's HEAD when the stamped cwd is inside `.claude/worktrees/<name>` >
 * the session's current branch > the cwd project's checked-out branch.
 * `origin_worktree` is true whenever the stamped cwd has the worktree path
 * shape, regardless of whether the branch could be read.
 */
export async function resolveScratchpadOrigin(params: {
  explicitBranch?: string | undefined;
  sessionState?: OriginSessionState | undefined;
  projectDetector?: ProjectDetector | undefined;
  /**
   * Registered path of the project the CWD resolves to (a `resolveScopedTenant`
   * result whose `source` is 'cwd'). NOT the write target: a note that targets
   * another project via projectId is still attributed to the writer's repo.
   * When absent, `projectDetector` resolves the cwd's project.
   */
  cwdProjectPath?: string | undefined;
}): Promise<ScratchpadOrigin> {
  const origin: ScratchpadOrigin = {};

  const cwd = resolveOriginCwd();
  if (nonEmpty(cwd)) origin.origin_cwd = cwd;

  const explicit = params.explicitBranch?.trim();
  if (nonEmpty(explicit) && explicit !== '*') origin.origin_branch = explicit;

  const cwdProject = async (): Promise<string | undefined> =>
    params.cwdProjectPath ?? (await detectCwdProjectPath(params.projectDetector));

  // Path shape of the cwd being stamped identifies a linked worktree (the same
  // detection the read tools use for their worktree note). Its branch is NOT
  // the session's nor the main checkout's: read the worktree's HEAD via the
  // main repo of the CWD's project.
  const wt = nonEmpty(cwd) ? worktreeContextOf(cwd) : undefined;
  if (wt) {
    origin.origin_worktree = true;
    if (origin.origin_branch === undefined) {
      const projectPath = await cwdProject();
      const branch = nonEmpty(projectPath)
        ? readLinkedWorktreeBranch(mainCheckoutOf(projectPath), wt.name)
        : null;
      if (nonEmpty(branch)) origin.origin_branch = branch;
      // else: unknown — omitted, never borrowed from the main checkout.
    }
    return origin;
  }

  const session = params.sessionState;
  if (session) {
    if (origin.origin_branch === undefined && nonEmpty(session.currentBranch)) {
      origin.origin_branch = session.currentBranch;
    }
    origin.origin_worktree = session.isWorktree;
    return origin;
  }

  const projectPath = await cwdProject();
  if (nonEmpty(projectPath)) {
    if (origin.origin_branch === undefined) {
      const branch = getCurrentBranch(projectPath);
      if (nonEmpty(branch) && branch !== 'HEAD') origin.origin_branch = branch;
    }
    origin.origin_worktree = isWorktree(projectPath);
  }
  return origin;
}
