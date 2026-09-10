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
 *   - `unresolved`     NO project was resolved. The result is not a statement
 *                      about any repository.
 *
 * That last rung exists because the original design assumed the opposite. This
 * file used to return an EMPTY echo when nothing resolved, on the reasoning
 * that "the tool already reports that failure in its own words". Measured, the
 * tools did not: `scratchpad list` without `cwd` answered `total: 0` with no
 * echo and no hint, byte-identical to a genuinely empty project — and it was
 * reported from the field as "the scratchpad is empty despite dozens of
 * sessions". The scratchpad held 16 notes for one project and 52 for another.
 * An absent field cannot carry that distinction; a named one can.
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
  'unresolved',
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
 * The echo for a call where no project resolved. Says so in the field an agent
 * already reads to learn WHICH project answered, rather than leaving that field
 * out and letting the emptiness pass for a fact about the code.
 */
export const UNRESOLVED_ECHO: ProjectEcho = { project_source: 'unresolved' };

/**
 * A caveat for an empty result that no project backs. Phrased as what the
 * caller must do, because the previous behaviour left them nothing to act on.
 */
export const UNRESOLVED_PROJECT_HINT =
  'No project was resolved for this call, so this result says nothing about any ' +
  'repository — it is not evidence that the project is empty. Pass `cwd` (an ' +
  'absolute path inside the repo) or an explicit `projectId`, then re-run.';

/**
 * Build the echo for a resolved identity. Reports `unresolved` when nothing
 * resolved. Fields the server does not know are omitted, never fabricated (no
 * `project_path` without a registry entry).
 */
export function projectEcho(
  identity: IdentityLike | undefined,
  explicitProjectId?: string
): ProjectEcho {
  if (identity === undefined) return { ...UNRESOLVED_ECHO };
  const projectId = identity.projectId;
  if (projectId === undefined || projectId === '') return { ...UNRESOLVED_ECHO };
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
  // `fallback` is the scratchpad's unresolved rung — the exact path that
  // produced the reported "the scratchpad is empty" (#384).
  if (scoped.source === 'fallback') return { ...UNRESOLVED_ECHO };
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
