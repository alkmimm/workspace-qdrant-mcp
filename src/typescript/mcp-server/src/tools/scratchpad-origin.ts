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
 */

import type { SessionState } from '../server-types.js';
import type { ProjectDetector } from '../utils/project-detector.js';
import { getRequestContext, getEffectiveCwd } from '../utils/request-context.js';
import { getCurrentBranch, isWorktree } from '../utils/git-utils.js';
import { resolveProjectIdentity } from './branch-scope.js';

/** Provenance fields, named exactly as persisted in the Qdrant payload. */
export interface ScratchpadOrigin {
  origin_branch?: string;
  origin_cwd?: string;
  origin_worktree?: boolean;
}

/** The slice of session git state the origin resolver consumes. */
export type OriginSessionState = Pick<SessionState, 'currentBranch' | 'isWorktree'>;

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
  if (ctx?.hostCwd && ctx.hostCwd.length > 0) return ctx.hostCwd;
  if (!ctx) return getEffectiveCwd();
  return undefined;
}

/**
 * Resolve write-time provenance for a scratchpad create/update.
 *
 * Branch resolution order: explicit `branch` arg (the caller knows best —
 * e.g. a /batch worker in a worktree the server cannot see) → the session's
 * cached git state (refreshed ≤5s ago by `ensureProjectFresh`) → best-effort
 * detection from the effective cwd (the stateless `scratchpad` tool has no
 * session). Origin means "where the write came from": when a note targets
 * another project via explicit projectId, the writer's own location is still
 * the correct attribution.
 */
export async function resolveScratchpadOrigin(params: {
  explicitBranch?: string | undefined;
  sessionState?: OriginSessionState | undefined;
  projectDetector?: ProjectDetector | undefined;
}): Promise<ScratchpadOrigin> {
  const origin: ScratchpadOrigin = {};

  const cwd = resolveOriginCwd();
  if (cwd) origin.origin_cwd = cwd;

  const explicit = params.explicitBranch?.trim();
  if (explicit && explicit !== '*') origin.origin_branch = explicit;

  const session = params.sessionState;
  if (session) {
    if (origin.origin_branch === undefined && session.currentBranch) {
      origin.origin_branch = session.currentBranch;
    }
    origin.origin_worktree = session.isWorktree;
    return origin;
  }

  if (!params.projectDetector) return origin;
  try {
    const identity = await resolveProjectIdentity(params.projectDetector, undefined);
    if (identity.projectPath) {
      if (origin.origin_branch === undefined) {
        const branch = getCurrentBranch(identity.projectPath);
        if (branch && branch !== 'HEAD') origin.origin_branch = branch;
      }
      origin.origin_worktree = isWorktree(identity.projectPath);
    }
  } catch {
    // Best-effort enrichment: detection failure must never block the write.
  }
  return origin;
}
