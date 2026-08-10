/**
 * Shared branch scoping helpers for project-aware tools.
 *
 * Default project-scoped reads to the caller's current Git branch so indexed
 * feature/worktree branches do not bleed into ordinary results. `branch: "*"`
 * remains the explicit opt-out for cross-branch reads.
 */

import type { ProjectDetector, ProjectInfo } from '../utils/project-detector.js';
import type { SqliteStateManager } from '../clients/sqlite-state-manager.js';
import { getCurrentBranch } from '../utils/git-utils.js';
import { getEffectiveCwd } from '../utils/request-context.js';

export interface ProjectIdentity {
  projectId: string | undefined;
  projectPath: string | undefined;
}

export async function resolveProjectIdentity(
  projectDetector: ProjectDetector,
  explicitProjectId: string | undefined,
  fallbackToSoleProject = true,
  stateManager?: SqliteStateManager
): Promise<ProjectIdentity> {
  if (explicitProjectId) {
    // Complete the project path from the registry: with `projectPath`
    // undefined, resolveEffectiveBranch cannot read the checked-out branch
    // and the read silently loses branch scoping — a projectId-only caller
    // then gets cross-branch results, including stale per-branch content
    // generations. (The exact/semantic paths did this lookup inline before;
    // it lives here now so every resolveProjectIdentity caller shares it.)
    return {
      projectId: explicitProjectId,
      projectPath: stateManager?.getProjectById(explicitProjectId).data?.project_path,
    };
  }
  const projectInfo: ProjectInfo | null = await projectDetector.getProjectInfo(
    getEffectiveCwd(),
    false,
    { fallbackToSoleProject }
  );
  return {
    projectId: projectInfo?.projectId,
    projectPath: projectInfo?.projectPath,
  };
}

export function resolveEffectiveBranch(params: {
  explicitBranch: string | undefined;
  scope: string;
  projectId: string | undefined;
  projectPath: string | undefined;
}): string | undefined {
  if (params.explicitBranch !== undefined) return params.explicitBranch;
  if (params.scope !== 'project' || !params.projectId) return undefined;
  if (!params.projectPath) return undefined;
  const branch = getCurrentBranch(params.projectPath);
  return branch && branch !== 'HEAD' ? branch : undefined;
}

export function applyEffectiveBranch<T extends { branch?: string }>(
  options: T,
  effectiveBranch: string | undefined
): T {
  if (effectiveBranch === undefined || effectiveBranch === options.branch) return options;
  return { ...options, branch: effectiveBranch };
}

export function concreteBranchFilter(branch: string | undefined): string | undefined {
  return branch && branch !== '*' ? branch : undefined;
}

/**
 * At or below this many branches the concrete list is shown verbatim (it IS the
 * disambiguation payload of a `branch:"*"` sweep — which paths are branch-
 * exclusive). Above it the field collapses to `"*"` + a count.
 */
export const BRANCH_SMALL_SET_MAX = 3;

export interface CollapsedBranch {
  /** The value to show, or `undefined` to OMIT the field (redundant with the
   *  queried branch). `"*"` means "wide fan-out — see branch_count". */
  branch?: string;
  /** Present only when `branch === "*"`: the real number of branches. */
  branch_count?: number;
}

/**
 * Normalize the daemon's branch representation to a clean list. The FTS surfaces
 * (grep, exact) hand back a comma-joined STRING; the Qdrant payload (semantic,
 * retrieve) hands back an ARRAY. Accept both, plus a bare scalar.
 */
export function normalizeBranchList(value: unknown): string[] {
  const raw = Array.isArray(value)
    ? value.map((v) => String(v))
    : typeof value === 'string'
      ? value.split(',')
      : [];
  return raw.map((b) => b.trim()).filter((b) => b.length > 0);
}

/**
 * Collapse a branch set into a compact, high-signal form — the SINGLE source of
 * truth shared by grep, exact search, semantic search and retrieve.
 *
 * The daemon returns the FULL `file_metadata.branches` mirror (every branch the
 * content is byte-identical on). Most files are identical across every branch,
 * so in a many-branch repo that is a ~60-name, ~1.5 KB list on EVERY hit — noise
 * that buried the content and drove agents off the tools (field feedback
 * 2026-08-10). Rules:
 *   - `queriedBranch` set (a concrete-branch read — the default) AND present in
 *     the set → the field just repeats the query → OMIT it.
 *   - Otherwise it carries signal: `<= BRANCH_SMALL_SET_MAX` names show verbatim
 *     (branch-exclusive hits — the point of a `branch:"*"` sweep); a wider set
 *     collapses to `"*"` + `branch_count`.
 * `queriedBranch` is `concreteBranchFilter(effectiveBranch)`, i.e. `undefined`
 * for a `branch:"*"` sweep — where nothing is redundant, so nothing is omitted.
 */
export function collapseBranchSet(
  branches: string[],
  queriedBranch: string | undefined
): CollapsedBranch {
  if (branches.length === 0) return {};
  if (queriedBranch !== undefined && branches.includes(queriedBranch)) return {};
  if (branches.length <= BRANCH_SMALL_SET_MAX) return { branch: branches.join(',') };
  return { branch: '*', branch_count: branches.length };
}

/**
 * Apply {@link collapseBranchSet} to a result metadata record in place. Handles
 * the `branch` field whether the daemon handed it back as a payload array
 * (semantic, retrieve) or an FTS comma-string (exact). No-op when absent.
 */
export function collapseMetadataBranchField(
  metadata: Record<string, unknown>,
  queriedBranch: string | undefined
): void {
  if (!('branch' in metadata)) return;
  const c = collapseBranchSet(normalizeBranchList(metadata['branch']), queriedBranch);
  if (c.branch === undefined) delete metadata['branch'];
  else metadata['branch'] = c.branch;
  if (c.branch_count !== undefined) metadata['branch_count'] = c.branch_count;
  else delete metadata['branch_count'];
}

/**
 * Collapse the branch fields of every result's metadata in place. Structural
 * over the result shape so branch-scope.ts stays free of a SearchResult import.
 */
export function collapseResultBranchFields(
  results: Array<{ metadata?: Record<string, unknown> | null }>,
  queriedBranch: string | undefined
): void {
  for (const r of results) {
    if (r.metadata) collapseMetadataBranchField(r.metadata, queriedBranch);
  }
}

/**
 * Decide the base branch to fall back to for files unchanged on the caller's
 * feature branch. The daemon only tags CHANGED files under a feature branch
 * (unchanged files stay under the project's base branch), so a branch-scoped
 * read on a feature branch would otherwise miss most of the project.
 *
 * `baseBranch` must be resolved from the indexed DATA (the branch the daemon
 * actually tagged the bulk under — see `getBaseBranch`), NOT from git's local
 * default, because the daemon's base tag can differ from the repo's git default
 * (e.g. files tagged "main" while git's default is "master"). Returns undefined
 * when no fallback should apply: no concrete effective branch (e.g. "*" or
 * unset), no base branch, or the effective branch already IS the base branch.
 */
export function resolveFallbackBranch(params: {
  effectiveBranch: string | undefined;
  baseBranch: string | null | undefined;
}): string | undefined {
  const eff = concreteBranchFilter(params.effectiveBranch);
  if (!eff || !params.baseBranch) return undefined;
  return params.baseBranch !== eff ? params.baseBranch : undefined;
}
