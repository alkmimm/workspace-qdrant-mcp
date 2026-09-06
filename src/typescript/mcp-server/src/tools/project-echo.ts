/**
 * Resolved-project echo for project-scoped READ responses.
 *
 * Field feedback (2026-08-19, bws-engineer session): a `search` issued without
 * `cwd` answered from ANOTHER project — the session's sticky cwd still pointed
 * at the previous repo — and nothing in the envelope said which project had
 * been resolved. `scope:"project"` + `status:"ok"` read as "correct scope", the
 * relative paths were plausible for a generic query, the rerank scores were
 * high. The misroute was invisible until the agent recognised the files.
 *
 * `store` already echoes `project_id` / `project_path` on the WRITE side for
 * the same reason (PR #362). This is the read-side counterpart, shared by
 * search / grep / list / retrieve / graph / scratchpad list / rules list, so
 * every tenant-addressed surface names the project it answered from — and HOW
 * it got there:
 *
 *   - `projectId`      an explicit tenant id
 *   - `cwd`            the cwd bound to THIS call (header or tool-body `cwd`)
 *                      matched a registered project
 *   - `sticky-cwd`     a cwd remembered from an EARLIER call in the session —
 *                      the one case where a stale project can answer silently
 *   - `sole-project`   the cwd matched NO registered project and the only
 *                      registered one answered (the detector's convenience
 *                      fallback) — another silent-misroute shape, now labelled
 *   - `server-default` an HTTP call bound no cwd at all (no header, no body
 *                      cwd, no sticky value): the server's own default cwd
 *                      resolved the project, not the caller's
 *   - `session`        the session's activated project (a write-side rung the
 *                      scratchpad tool's list can land on)
 *
 * Intentionally WITHOUT the echo: `search_eval` (a benchmark harness that
 * already returns its `projectId`), `workspace_index` (registry mutations that
 * echo `project_id`/`project_root`), and reads of the `libraries`/`rules`
 * collections, which are not tenant-addressed.
 *
 * Deliberately tiny (~60 bytes): the value is in the comparison the agent can
 * make against the repo it meant, not in prose.
 */
import { getRequestContext, type ResolvedProjectIdentity } from '../utils/request-context.js';
import type { ScopedTenant } from './tenant-scope.js';

/**
 * Every rung the project of a read can be resolved by. Runtime array, with the
 * type derived from it, so `help("http")` can be asserted to document all of
 * them — a new rung an agent sees in `project_source` but cannot look up is
 * worse than no echo at all.
 */
export const PROJECT_SOURCES = [
  'projectId',
  'cwd',
  'sticky-cwd',
  'sole-project',
  'server-default',
  'session',
] as const;

/** How the project of a read was resolved. */
export type ProjectSource = (typeof PROJECT_SOURCES)[number];

export interface ProjectEcho {
  project_id?: string;
  project_path?: string;
  project_source?: ProjectSource;
}

interface IdentityLike {
  projectId?: string | undefined;
  projectPath?: string | undefined;
  /** The rung `resolveProjectIdentity` reported, when known. */
  source?: ResolvedProjectIdentity['source'];
}

/**
 * The identity the shared resolver recorded on this request, if any. A tool
 * that already scoped its read through `resolveProjectIdentity` echoes THAT
 * resolution instead of resolving a second time (which is not a cache hit and
 * could, in principle, name a different project).
 */
export function recordedIdentity(): ResolvedProjectIdentity | undefined {
  return getRequestContext()?.resolvedIdentity;
}

/** Source label for a cwd-rung resolution, from the resolver's rung and the request's cwd provenance. */
function cwdSource(identity: IdentityLike): ProjectSource {
  if (identity.source === 'sole-project') return 'sole-project';
  const ctx = getRequestContext();
  if (ctx === undefined) return 'cwd'; // stdio: the process cwd IS the client's
  if (ctx.cwdSource === 'sticky') return 'sticky-cwd';
  if (ctx.cwdSource === undefined) return 'server-default';
  return 'cwd';
}

/**
 * Build the echo for a resolved identity. Empty when nothing resolved — the
 * tool already reports that failure in its own words. Fields the server does
 * not know are omitted, never fabricated (no `project_path` without a registry
 * entry).
 */
export function projectEcho(
  identity: IdentityLike | undefined,
  explicitProjectId?: string
): ProjectEcho {
  if (identity === undefined) return {};
  const projectId = identity.projectId;
  if (projectId === undefined || projectId === '') return {};
  const echo: ProjectEcho = { project_id: projectId };
  const projectPath = identity.projectPath;
  if (projectPath !== undefined && projectPath !== '') echo.project_path = projectPath;
  const explicit =
    (explicitProjectId !== undefined && explicitProjectId !== '') ||
    identity.source === 'projectId';
  echo.project_source = explicit ? 'projectId' : cwdSource(identity);
  return echo;
}

/**
 * Echo for a write-side tenant resolution (`resolveScopedTenant`), used by the
 * scratchpad tool's list. The cwd rung there went through the shared resolver,
 * so its recorded rung (sole-project) and the request's cwd provenance apply.
 */
export function scopedTenantEcho(scoped: ScopedTenant): ProjectEcho {
  if (scoped.source === 'fallback') return {};
  const echo: ProjectEcho = { project_id: scoped.tenantId };
  if (scoped.projectPath !== undefined && scoped.projectPath !== '') {
    echo.project_path = scoped.projectPath;
  }
  echo.project_source =
    scoped.source === 'projectId'
      ? 'projectId'
      : scoped.source === 'session'
        ? 'session'
        : cwdSource(recordedIdentity() ?? {});
  return echo;
}
