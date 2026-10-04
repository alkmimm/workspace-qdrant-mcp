/**
 * Per-branch index coverage: how many files the index holds for each branch,
 * against the files in that branch's tip for every branch with a live
 * checkout (the main folder or a linked worktree).
 *
 * `project_status` used to report only the queue ("325 files, 100%"), which
 * stays at 100% when content is later removed from the index. On
 * emnify-sms-sender (2026-10-03) a reconcile defect had emptied a worktree
 * branch to 102 of its 400 files while the status still read "complete", and
 * an agent could only find out by grepping for files it knew existed.
 *
 * The tip count includes files the daemon never indexes (images, lockfiles),
 * so the ratio is compared with the TRUNK's own ratio, not with 100%.
 */

import type { BranchFileCount } from '../clients/tracked-files-queries/index.js';
import type { SqliteStateManager } from '../clients/sqlite-state-manager.js';
import {
  countFilesAtBranchTip,
  listBranchCheckouts,
  type BranchCheckout,
} from '../utils/git-branch-diff.js';
import { getDefaultBranch } from '../utils/git-utils.js';

export interface BranchCoverage {
  branch: string;
  /** Distinct paths the index holds under this branch. */
  indexed_files: number;
  /** Where the branch is checked out, when it is. */
  checkout?: string;
  /** Files in the branch tip (git), for checked-out branches and the trunk. */
  tip_files?: number;
  /** indexed_files / tip_files, rounded to 2 decimals. */
  ratio?: number;
}

export interface BranchCoverageReport {
  trunk?: string;
  branches: BranchCoverage[];
  /** One line per checked-out branch whose coverage is far below the trunk's. */
  warnings: string[];
}

/** What the report needs from git (injectable for tests). */
export interface CoverageGit {
  checkouts(): Promise<BranchCheckout[] | null>;
  tipFiles(branch: string): Promise<number | null>;
}

/** A branch is flagged below this fraction of the trunk's own ratio. */
const PARTIAL_FRACTION_OF_TRUNK = 0.5;
/** Without a trunk ratio to compare with, flag below this absolute ratio. */
const PARTIAL_ABSOLUTE_RATIO = 0.4;
/** Tips smaller than this are too small for a ratio to mean anything. */
const MIN_TIP_FILES = 20;

export async function computeBranchCoverage(params: {
  counts: readonly BranchFileCount[];
  trunk: string | null;
  git: CoverageGit;
}): Promise<BranchCoverageReport> {
  const { counts, trunk, git } = params;
  const checkoutOf = new Map<string, string>();
  for (const c of (await git.checkouts()) ?? []) {
    if (c.branch && !checkoutOf.has(c.branch)) checkoutOf.set(c.branch, c.path);
  }

  const indexed = new Map(counts.map((c) => [c.branch, c.files]));
  // Checked-out branches the index has nothing for are the worst case — list them too.
  const names = new Set<string>([...indexed.keys(), ...checkoutOf.keys()]);
  if (trunk) names.add(trunk);

  // Tips only for the trunk and checked-out branches, counted concurrently.
  const branches: BranchCoverage[] = await Promise.all(
    [...names].map(async (branch) => {
      const entry: BranchCoverage = { branch, indexed_files: indexed.get(branch) ?? 0 };
      const checkout = checkoutOf.get(branch);
      if (checkout !== undefined) entry.checkout = checkout;
      if (checkout !== undefined || branch === trunk) {
        const tip = await git.tipFiles(branch);
        if (tip !== null && tip > 0) {
          entry.tip_files = tip;
          entry.ratio = Math.round((entry.indexed_files / tip) * 100) / 100;
        }
      }
      return entry;
    })
  );
  branches.sort((a, b) => b.indexed_files - a.indexed_files || a.branch.localeCompare(b.branch));

  const trunkRatio = branches.find((b) => b.branch === trunk)?.ratio;
  const warnings: string[] = [];
  for (const b of branches) {
    if (b.branch === trunk || b.checkout === undefined) continue;
    if (b.tip_files === undefined || b.ratio === undefined || b.tip_files < MIN_TIP_FILES) continue;
    const floor =
      trunkRatio !== undefined ? trunkRatio * PARTIAL_FRACTION_OF_TRUNK : PARTIAL_ABSOLUTE_RATIO;
    if (b.ratio >= floor) continue;
    const pct = (r: number) => `${Math.round(r * 100)}%`;
    warnings.push(
      `Branch '${b.branch}' (checked out at ${b.checkout}): the index holds ${b.indexed_files} of ` +
        `the ${b.tip_files} files in its tip (${pct(b.ratio)})` +
        (trunkRatio !== undefined ? `, against ${pct(trunkRatio)} on the trunk '${trunk}'` : '') +
        ` — reads scoped to it can miss files; verify absence on disk.`
    );
  }

  const report: BranchCoverageReport = { branches, warnings };
  if (trunk) report.trunk = trunk;
  return report;
}

/** Computes the coverage report for a tenant, or `null` when it cannot. */
export type BranchCoverageProbe = (tenantId: string) => Promise<BranchCoverageReport | null>;

/** The production probe: index counts from state.db, tips and checkouts from git. */
export async function probeBranchCoverage(
  stateManager: Pick<
    SqliteStateManager,
    'getWatchFolderIdByTenantId' | 'getProjectById' | 'getTrackedFileCountsByBranch'
  >,
  tenantId: string
): Promise<BranchCoverageReport | null> {
  const watchFolderId = stateManager.getWatchFolderIdByTenantId(tenantId);
  if (!watchFolderId) return null;
  const root = stateManager.getProjectById(tenantId).data?.project_path;
  const counts = stateManager.getTrackedFileCountsByBranch(watchFolderId);
  if (!root) {
    return computeBranchCoverage({
      counts,
      trunk: null,
      git: { checkouts: async () => null, tipFiles: async () => null },
    });
  }
  return computeBranchCoverage({
    counts,
    trunk: getDefaultBranch(root),
    git: {
      checkouts: () => listBranchCheckouts(root),
      tipFiles: (branch) => countFilesAtBranchTip(root, branch),
    },
  });
}

/**
 * Attach the index's per-branch coverage to a `list_branches` response. The
 * registry lists the branches someone registered; a branch the daemon indexed
 * through a worktree was invisible there (it showed only the primary).
 * Best-effort: without a tenant or a probe the response is returned as is.
 */
export async function withIndexCoverage(
  result: unknown,
  probe: BranchCoverageProbe | undefined,
  tenantHint?: string
): Promise<unknown> {
  const value: unknown = await result;
  if (!probe || !value || typeof value !== 'object') return value;
  const record = value as Record<string, unknown>;
  const responseId = record['projectId'];
  const tenantId = tenantHint ?? (typeof responseId === 'string' ? responseId : undefined);
  if (!tenantId) return value;
  let report: BranchCoverageReport | null = null;
  try {
    report = await probe(tenantId);
  } catch {
    report = null;
  }
  if (!report) return value;
  return {
    ...record,
    index_coverage: { trunk: report.trunk, branches: report.branches },
    ...(report.warnings.length > 0 ? { coverage_warnings: report.warnings } : {}),
  };
}

/**
 * The response shape: the trunk and every checked-out branch in full, the
 * branches the index holds without a checkout reduced to a count (a
 * many-branch repo would otherwise ship dozens of entries on every status).
 */
export function coverageResponseFields(report: BranchCoverageReport | null | undefined): {
  index_coverage?: { trunk?: string; branches: BranchCoverage[]; other_indexed_branches: number };
  coverage_warnings?: string[];
} {
  if (!report) return {};
  const live = report.branches.filter((b) => b.checkout !== undefined || b.branch === report.trunk);
  const coverage: { trunk?: string; branches: BranchCoverage[]; other_indexed_branches: number } = {
    branches: live,
    other_indexed_branches: report.branches.length - live.length,
  };
  if (report.trunk) coverage.trunk = report.trunk;
  return {
    index_coverage: coverage,
    ...(report.warnings.length > 0 ? { coverage_warnings: report.warnings } : {}),
  };
}
