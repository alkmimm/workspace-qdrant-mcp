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
