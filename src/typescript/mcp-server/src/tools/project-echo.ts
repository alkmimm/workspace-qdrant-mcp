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
 * search / grep / list / retrieve / graph so every project-scoped surface names
 * the tenant it answered from — and HOW it got there:
 *
 *   - `cwd`        the cwd bound to THIS call (header or tool-body `cwd`)
 *   - `sticky-cwd` a cwd remembered from an EARLIER call in the session — the
 *                  one case where a stale project can answer silently
 *   - `projectId`  an explicit tenant id
 *
 * Deliberately tiny (~60 bytes): the value is in the comparison the agent can
 * make against the repo it meant, not in prose.
 */
import { getRequestContext } from '../utils/request-context.js';

/** How the project of a read was resolved. */
export type ProjectSource = 'projectId' | 'cwd' | 'sticky-cwd';

export interface ProjectEcho {
  project_id?: string;
  project_path?: string;
  project_source?: ProjectSource;
}

/**
 * Build the echo for a resolved identity. Empty when nothing resolved — the
 * tool already reports that failure in its own words. Fields the server does
 * not know are omitted, never fabricated (no `project_path` without a registry
 * entry).
 */
export function projectEcho(
  identity: { projectId?: string | undefined; projectPath?: string | undefined } | undefined,
  explicitProjectId?: string
): ProjectEcho {
  if (identity === undefined) return {};
  const projectId = identity.projectId;
  if (projectId === undefined || projectId === '') return {};
  const echo: ProjectEcho = { project_id: projectId };
  const projectPath = identity.projectPath;
  if (projectPath !== undefined && projectPath !== '') echo.project_path = projectPath;
  echo.project_source =
    explicitProjectId !== undefined && explicitProjectId !== ''
      ? 'projectId'
      : getRequestContext()?.cwdSource === 'sticky'
        ? 'sticky-cwd'
        : 'cwd';
  return echo;
}
