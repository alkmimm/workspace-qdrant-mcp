/**
 * Tenant resolution for PROJECT-SCOPED WRITES — the single source of truth
 * shared by `store(type:"scratchpad")`, `store(type:"url")`,
 * `store(type:"library", forProject:true)` and the `scratchpad` tool's
 * list/update/delete.
 *
 * Why it exists: the READ surfaces (search / grep / list / retrieve / rules)
 * all funnel through {@link resolveProjectIdentity}, whose precedence is
 *
 *     explicit `projectId`  ->  the effective cwd  ->  nothing
 *
 * where "effective cwd" is itself `X-MCP-Host-Cwd` header > body `cwd` >
 * session sticky cwd (see `resolveStickyCwd` / `getEffectiveCwd`). The session's
 * ACTIVATED project (`sessionState.projectId`) is deliberately not part of that
 * chain.
 *
 * The write paths used to consult `sessionState.projectId` FIRST. That project
 * is set asynchronously and fire-and-forget by `ensureClientProjectActive`, so
 * it lags the caller: it still names the previous repo right after a cwd switch,
 * and while an activation is in flight. A `store` carrying an explicit `cwd` for
 * project A therefore landed in project B, reported success, and became
 * invisible to A — every project-scoped read surface (the recall lane,
 * `scratchpad list`) is tenant-strict. Observed 2026-09-02: a note written with
 * `cwd` = workspace-qdrant-mcp was queued under the DOC-V2 tenant while `rules`,
 * `search` and `list` in the SAME session resolved workspace-qdrant-mcp from the
 * same cwd. Provenance stamping (`origin_cwd`) had it right, which is exactly
 * what made the mismatch legible after the fact.
 *
 * The fix is to give writes the read precedence, keeping the session project
 * only as a LAST resort before the global tenant — it is still the one thing
 * that resolves for a caller that never passes a cwd (a stdio client launched
 * outside any repo), which is why it is not simply dropped.
 */

import type { ProjectDetector } from '../utils/project-detector.js';
import type { SqliteStateManager } from '../clients/sqlite-state-manager.js';
import { TENANT_GLOBAL } from '../constants/tenants.js';
import { resolveProjectIdentity } from './branch-scope.js';

/** Which rung of the precedence chain produced the tenant. */
export type TenantSource = 'projectId' | 'cwd' | 'session' | 'fallback';

export interface ScopedTenant {
  /** The tenant_id to write under. */
  tenantId: string;
  /**
   * Registered path of the resolved project, when known. Echoed back to the
   * caller so a misroute is visible in the response instead of silent: an agent
   * that passed `cwd` for repo A sees immediately whether A is what the server
   * resolved.
   */
  projectPath?: string;
  /** Which rung resolved it — `'fallback'` means no project was resolved. */
  source: TenantSource;
}

export interface ResolveScopedTenantParams {
  /** Explicit `projectId` tool argument, if any. Wins outright. */
  explicitProjectId?: unknown;
  projectDetector: ProjectDetector;
  /**
   * The session's activated project. LAST resort before the fallback tenant —
   * see the module docs for why it must not outrank the cwd.
   */
  sessionProjectId?: string | null | undefined;
  /** Used to complete `projectPath` for an id that did not come from the cwd. */
  stateManager?: SqliteStateManager | undefined;
  /** Tenant to use when nothing resolves (default: the global sentinel). */
  fallbackTenant?: string;
}

/** Best-effort registered path for a tenant_id. Never throws. */
function lookupProjectPath(
  projectId: string,
  stateManager: SqliteStateManager | undefined
): string | undefined {
  if (!stateManager) return undefined;
  try {
    return stateManager.getProjectById(projectId).data?.project_path ?? undefined;
  } catch {
    return undefined;
  }
}

/**
 * Resolve the tenant a project-scoped write belongs to.
 *
 * Precedence (identical to the read surfaces, plus a session last resort):
 *   1. explicit `projectId` argument
 *   2. the project detected from the effective cwd (header > body > sticky cwd)
 *   3. the session's activated project
 *   4. `fallbackTenant` (the global sentinel by default)
 */
export async function resolveScopedTenant(
  params: ResolveScopedTenantParams
): Promise<ScopedTenant> {
  const explicit =
    typeof params.explicitProjectId === 'string' ? params.explicitProjectId.trim() : '';
  if (explicit) {
    const path = lookupProjectPath(explicit, params.stateManager);
    return { tenantId: explicit, ...(path ? { projectPath: path } : {}), source: 'projectId' };
  }

  try {
    // Same call the read surfaces make: a longest-prefix match of the effective
    // cwd against daemon-registered project paths, with the sole-project
    // fallback. `undefined` for the explicit id — it was handled above.
    const identity = await resolveProjectIdentity(
      params.projectDetector,
      undefined,
      true,
      params.stateManager
    );
    if (identity.projectId) {
      return {
        tenantId: identity.projectId,
        ...(identity.projectPath ? { projectPath: identity.projectPath } : {}),
        source: 'cwd',
      };
    }
  } catch {
    // Detection failed (no project at cwd / ambiguous) — try the session next.
  }

  const sessionProjectId = params.sessionProjectId?.trim();
  if (sessionProjectId) {
    const path = lookupProjectPath(sessionProjectId, params.stateManager);
    return {
      tenantId: sessionProjectId,
      ...(path ? { projectPath: path } : {}),
      source: 'session',
    };
  }

  return { tenantId: params.fallbackTenant ?? TENANT_GLOBAL, source: 'fallback' };
}

/**
 * Render the resolved scope for a write response message.
 *
 * Always names the tenant; adds the project path when known, so a caller that
 * passed a `cwd` can confirm at a glance that the server resolved the repo it
 * meant. Field feedback 2026-09-02: the old message carried the tenant id alone,
 * so a misroute to another project's opaque 12-hex id read as success.
 */
export function describeScope(scoped: ScopedTenant): string {
  return scoped.projectPath ? `${scoped.tenantId} — ${scoped.projectPath}` : scoped.tenantId;
}
