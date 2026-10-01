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
import {
  getEffectiveCwd,
  getRequestContext,
  type ProjectResolutionSource,
} from '../utils/request-context.js';

export interface ProjectIdentity {
  projectId: string | undefined;
  projectPath: string | undefined;
  /** Which rung resolved it (explicit id, the cwd, or the sole registered
   *  project when the cwd matched none). Absent when nothing resolved. */
  source?: ProjectResolutionSource;
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
    return record({
      projectId: explicitProjectId,
      projectPath: stateManager?.getProjectById(explicitProjectId).data?.project_path,
      source: 'projectId',
    });
  }
  const projectInfo: ProjectInfo | null = await projectDetector.getProjectInfo(
    getEffectiveCwd(),
    false,
    { fallbackToSoleProject }
  );
  if (!projectInfo) return record({ projectId: undefined, projectPath: undefined });
  return record({
    projectId: projectInfo.projectId,
    projectPath: projectInfo.projectPath,
    // The detector marks the sole-project convenience fallback: the cwd matched
    // NO registered project. Read echoes must not call that `cwd`.
    source: projectInfo.resolvedBy === 'sole-project' ? 'sole-project' : 'cwd',
  });
}

/** Record the resolution on the request so the response echo can reuse it. */
function record(identity: ProjectIdentity): ProjectIdentity {
  const ctx = getRequestContext();
  if (ctx) ctx.resolvedIdentity = identity;
  return identity;
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
 * `baseBranch` must come from `getBaseBranch`, which reconciles git's default
 * branch with the branches the index actually holds — git alone can name a
 * branch the index never tagged (the write path defaults a tag to "main", so a
 * repo whose git default is "master" can hold its files under "main"), and the
 * index alone cannot tell a trunk from a long-lived feature branch. That
 * function already returns `null` when the caller is on the trunk, so the guard
 * below is a second line of defence rather than the only one: when it WAS the
 * only one it never fired, because the resolver was asked for "the best branch
 * that isn't you" and so could never answer "you".
 *
 * Returns undefined when no fallback should apply: no concrete effective branch
 * (e.g. "*" or unset), no base branch, or the effective branch already IS it.
 */
export function resolveFallbackBranch(params: {
  effectiveBranch: string | undefined;
  baseBranch: string | null | undefined;
}): string | undefined {
  const eff = concreteBranchFilter(params.effectiveBranch);
  if (!eff || !params.baseBranch) return undefined;
  return params.baseBranch !== eff ? params.baseBranch : undefined;
}

/**
 * Drop base-branch entries for paths the caller's OWN branch already answered.
 *
 * The vector lane cannot express "fill only what's missing" in its Qdrant filter
 * — {@link branchFilterClause} widens with a plain `should: [branch,
 * fallbackBranch]`, so both generations of a file changed on the caller's branch
 * come back. The per-file collapse downstream then keeps whichever RANKED
 * higher, which can be the base branch's older content: the stale copy wins on
 * score and the caller never learns there was a newer one. This restores the
 * same precedence the FTS and `list` surfaces use — the scoped branch owns any
 * path it carries, and the fallback only fills gaps.
 *
 * A no-op when there is no fallback in play. Entries whose path or branch cannot
 * be read are kept: a metadata gap must not become silent data loss.
 */
export function dropFallbackDuplicatesByPath<T>(
  entries: readonly T[],
  effectiveBranch: string | undefined,
  fallbackBranch: string | undefined,
  pathOf: (entry: T) => string | undefined,
  branchOf: (entry: T) => unknown
): T[] {
  const eff = concreteBranchFilter(effectiveBranch);
  if (!eff || !fallbackBranch || fallbackBranch === eff) return [...entries];
  const branchesOf = (entry: T): string[] => normalizeBranchList(branchOf(entry));
  const answeredByScope = new Set<string>();
  for (const entry of entries) {
    const path = pathOf(entry);
    if (path && branchesOf(entry).includes(eff)) answeredByScope.add(path);
  }
  if (answeredByScope.size === 0) return [...entries];
  return entries.filter((entry) => {
    const path = pathOf(entry);
    if (!path || !answeredByScope.has(path)) return true;
    const branches = branchesOf(entry);
    // No readable branch → we cannot tell this is the fallback's copy, and
    // guessing would delete a real hit. Keep it; only a hit we can positively
    // identify as fallback-only is dropped.
    if (branches.length === 0) return true;
    return branches.includes(eff);
  });
}

/**
 * Merge a base-branch fallback result into the branch-scoped one, keeping only
 * the fallback entries whose FILE PATH the scoped result does not already carry.
 *
 * This is the rule that makes a fallback safe, and it is the rule the `list`
 * surface has always applied in SQL ("rows on `branch` PLUS rows on
 * `fallbackBranch` whose `relative_path` is not already present"). The FTS and
 * vector surfaces instead concatenated the two result sets, so a path changed on
 * the caller's branch was returned TWICE — once current, once as the base
 * branch's older content generation — and an agent could read the pre-edit
 * version of a line it had just fixed. Collapsing by path keeps the fallback
 * doing its only job: filling in files the scoped view is missing.
 *
 * Entries whose path cannot be read are kept: dropping a hit because its shape
 * was unexpected would turn a metadata gap into silent data loss.
 */
export function mergeFallbackByMissingPath<T>(
  scoped: readonly T[],
  fallback: readonly T[],
  pathOf: (entry: T) => string | undefined
): T[] {
  if (fallback.length === 0) return [...scoped];
  const covered = new Set<string>();
  for (const entry of scoped) {
    const path = pathOf(entry);
    if (path) covered.add(path);
  }
  const merged = [...scoped];
  for (const entry of fallback) {
    const path = pathOf(entry);
    if (path && covered.has(path)) continue;
    merged.push(entry);
  }
  return merged;
}
