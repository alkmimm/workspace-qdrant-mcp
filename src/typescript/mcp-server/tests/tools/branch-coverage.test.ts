/**
 * Per-branch coverage in project_status / list_branches.
 *
 * Regression (emnify-sms-sender, 2026-10-03): status read "325 files, 100%"
 * while the worktree branch held 102 of its 400 files in the index — the queue
 * metric cannot see content removed after it drained. The ratio is judged
 * against the trunk's own ratio, because a tip counts files the daemon never
 * indexes (images, lockfiles).
 */

import { describe, it, expect } from 'vitest';
import {
  computeBranchCoverage,
  coverageResponseFields,
  withIndexCoverage,
  type CoverageGit,
} from '../../src/tools/branch-coverage.js';

const emnifyGit: CoverageGit = {
  checkouts: async () => [
    { path: '/repo', branch: 'develop' },
    { path: '/repo/.claude/worktrees/fase-3-comandos', branch: 'feat/fase-5' },
    { path: '/repo/.claude/worktrees/fase-1', branch: 'chore/fase-1' },
    { path: '/repo/.claude/worktrees/detached', branch: undefined },
  ],
  tipFiles: async (b) => ({ develop: 145, 'feat/fase-5': 400, 'chore/fase-1': 150 })[b] ?? null,
};

const emnifyCounts = [
  { branch: 'claude/repository-analysis', files: 134 },
  { branch: 'chore/fase-1', files: 127 },
  { branch: 'develop', files: 126 },
  { branch: 'feat/fase-5', files: 102 },
];

describe('computeBranchCoverage', () => {
  it('flags a checked-out branch far below the trunk ratio (the emnify case)', async () => {
    const report = await computeBranchCoverage({ counts: emnifyCounts, trunk: 'develop', git: emnifyGit });

    const fase5 = report.branches.find((b) => b.branch === 'feat/fase-5');
    expect(fase5).toMatchObject({
      indexed_files: 102,
      tip_files: 400,
      ratio: 0.26,
      checkout: '/repo/.claude/worktrees/fase-3-comandos',
    });
    expect(report.warnings).toHaveLength(1);
    expect(report.warnings[0]).toContain("'feat/fase-5'");
    expect(report.warnings[0]).toContain('102 of the 400 files');
    expect(report.warnings[0]).toContain("87% on the trunk 'develop'");
  });

  it('does not flag a healthy branch (chore/fase-1: 85% vs 87%)', async () => {
    const report = await computeBranchCoverage({ counts: emnifyCounts, trunk: 'develop', git: emnifyGit });
    expect(report.warnings.join('\n')).not.toContain('chore/fase-1');
  });

  it('only asks git for tips of checked-out branches and the trunk', async () => {
    const asked: string[] = [];
    await computeBranchCoverage({
      counts: emnifyCounts,
      trunk: 'develop',
      git: { ...emnifyGit, tipFiles: async (b) => (asked.push(b), 100) },
    });
    expect(asked.sort()).toEqual(['chore/fase-1', 'develop', 'feat/fase-5']);
  });

  it('lists a checked-out branch the index holds nothing for, at 0%', async () => {
    const report = await computeBranchCoverage({
      counts: [{ branch: 'develop', files: 126 }],
      trunk: 'develop',
      git: {
        checkouts: async () => [
          { path: '/repo', branch: 'develop' },
          { path: '/repo/wt', branch: 'feat/new' },
        ],
        tipFiles: async (b) => (b === 'develop' ? 145 : 120),
      },
    });
    expect(report.branches.find((b) => b.branch === 'feat/new')).toMatchObject({
      indexed_files: 0,
      ratio: 0,
    });
    expect(report.warnings).toHaveLength(1);
  });

  it('falls back to an absolute floor without a trunk ratio, and ignores tiny tips', async () => {
    const report = await computeBranchCoverage({
      counts: [
        { branch: 'a', files: 10 },
        { branch: 'tiny', files: 1 },
      ],
      trunk: null,
      git: {
        checkouts: async () => [
          { path: '/r/a', branch: 'a' },
          { path: '/r/t', branch: 'tiny' },
        ],
        tipFiles: async (b) => (b === 'a' ? 100 : 10),
      },
    });
    expect(report.warnings).toHaveLength(1);
    expect(report.warnings[0]).toContain("'a'");
  });

  it('degrades to index counts only when git cannot answer', async () => {
    const report = await computeBranchCoverage({
      counts: emnifyCounts,
      trunk: 'develop',
      git: { checkouts: async () => null, tipFiles: async () => null },
    });
    expect(report.warnings).toEqual([]);
    expect(report.branches.map((b) => b.indexed_files)).toEqual([134, 127, 126, 102]);
  });
});

describe('response shaping', () => {
  it('project_status keeps live branches in full and counts the rest', async () => {
    const report = await computeBranchCoverage({ counts: emnifyCounts, trunk: 'develop', git: emnifyGit });
    const fields = coverageResponseFields(report);
    expect(fields.index_coverage?.branches.map((b) => b.branch).sort()).toEqual([
      'chore/fase-1',
      'develop',
      'feat/fase-5',
    ]);
    expect(fields.index_coverage?.other_indexed_branches).toBe(1);
    expect(fields.coverage_warnings).toHaveLength(1);
    expect(coverageResponseFields(null)).toEqual({});
  });

  it('list_branches gains the index view, keyed by the tenant', async () => {
    const report = await computeBranchCoverage({ counts: emnifyCounts, trunk: 'develop', git: emnifyGit });
    const out = (await withIndexCoverage(
      Promise.resolve({ success: true, projectId: 'dd8113bcff19', branches: [{ name: 'develop' }] }),
      async (tenant) => (tenant === 'dd8113bcff19' ? report : null)
    )) as Record<string, unknown>;
    expect(out['branches']).toEqual([{ name: 'develop' }]);
    expect((out['index_coverage'] as { branches: unknown[] }).branches).toHaveLength(4);
    expect(out['coverage_warnings']).toHaveLength(1);
  });

  it('list_branches is unchanged without a tenant or when the probe throws', async () => {
    const base = { success: true, branches: [] };
    expect(await withIndexCoverage(base, async () => null)).toEqual(base);
    expect(
      await withIndexCoverage({ ...base, projectId: 't' }, async () => {
        throw new Error('db');
      })
    ).toEqual({ ...base, projectId: 't' });
  });
});
