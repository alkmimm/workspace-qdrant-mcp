/**
 * Tests for the `search_eval` tool handler — it shapes the benchmark harness
 * output and enforces tenant resolution. Uses a mock runner so no live index
 * is needed.
 */

import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';

import { describe, it, expect, vi } from 'vitest';
import { runSearchEval } from '../../src/tools/search-eval.js';
import type { ProjectDetector } from '../../src/utils/project-detector.js';
import type { SearchBenchmarkRunner } from '../../src/benchmarks/semantic-search.js';

/** Relative location of the bundled dataset under WQM_REPO_DIR (mirrors the
 *  constant in the tool). */
const BUNDLED_DATASET_REL =
  'src/typescript/mcp-server/scripts/benchmark-data/semantic-search-quality.yaml';

/** Runner that always returns `top` at rank 1 and one filler hit. */
function makeRunner(topRelPath: string): SearchBenchmarkRunner {
  return {
    collectionExists: vi.fn().mockResolvedValue(true),
    search: vi.fn(async (options: { query: string; mode?: string }) => ({
      results: [
        {
          id: '1',
          score: 0.9,
          collection: 'projects',
          content: '',
          metadata: { relative_path: topRelPath },
        },
        {
          id: '2',
          score: 0.8,
          collection: 'projects',
          content: '',
          metadata: { relative_path: 'src/unrelated/filler.ts' },
        },
      ],
      total: 2,
      query: options.query,
      mode: options.mode ?? 'semantic',
      scope: 'project',
      collections_searched: ['projects'],
      status: 'ok',
    })),
  } as unknown as SearchBenchmarkRunner;
}

function makeDetector(projectId?: string): ProjectDetector {
  return {
    getProjectInfo: vi.fn().mockResolvedValue(projectId ? { projectId } : null),
  } as unknown as ProjectDetector;
}

describe('runSearchEval', () => {
  it('evaluates inline cases and returns per-mode metrics + verdict', async () => {
    const res = await runSearchEval(makeRunner('src/tools/search.ts'), makeDetector('p1'), {
      cases: [{ query: 'where is the search tool', expectedFiles: ['src/tools/search.ts'] }],
    });

    expect(res.success).toBe(true);
    expect(res.datasetSource).toBe('inline');
    expect(res.queryCount).toBe(1);
    expect(res.projectId).toBe('p1');
    // Expected file is at rank 1 → 100% top1/top3.
    expect(res.modes?.semantic?.top1).toBe(100);
    expect(res.modes?.semantic?.top3).toBe(100);
    expect(res.modes?.semantic?.duplicateRate).toBe(0);
    expect(res.perQuery?.[0]?.semantic).toMatchObject({ top1: true, top3: true, top10: true });
    expect(res.verdict?.grade).toBeDefined();
  });

  it('reports a miss when the expected file is absent', async () => {
    const res = await runSearchEval(makeRunner('src/unrelated/other.ts'), makeDetector('p1'), {
      cases: [{ query: 'q', expectedFiles: ['src/tools/search.ts'] }],
    });
    expect(res.success).toBe(true);
    expect(res.modes?.semantic?.top10).toBe(0);
    expect(res.perQuery?.[0]?.semantic).toMatchObject({ top1: false, top10: false });
  });

  it('refuses project scope when no tenant can be resolved', async () => {
    const res = await runSearchEval(makeRunner('a.ts'), makeDetector(undefined), {
      cases: [{ query: 'x', expectedFiles: ['a.ts'] }],
    });
    expect(res.success).toBe(false);
    expect(res.error).toMatch(/project/i);
  });

  it('includes top paths when includeTopPaths=true', async () => {
    const res = await runSearchEval(makeRunner('src/tools/search.ts'), makeDetector('p1'), {
      cases: [{ query: 'q', expectedFiles: ['src/tools/search.ts'] }],
      includeTopPaths: true,
    });
    const sem = res.perQuery?.[0]?.semantic as Record<string, unknown>;
    expect(sem.topPaths).toEqual(['src/tools/search.ts', 'src/unrelated/filler.ts']);
  });

  it('forwards rerank/rerankWeight overrides to every search call', async () => {
    const runner = makeRunner('src/tools/search.ts');
    const res = await runSearchEval(runner, makeDetector('p1'), {
      cases: [{ query: 'q', expectedFiles: ['src/tools/search.ts'] }],
      rerank: true,
      rerankWeight: 0.5,
    });
    expect(res.success).toBe(true);
    const calls = (runner.search as ReturnType<typeof vi.fn>).mock.calls as Array<
      [Record<string, unknown>]
    >;
    expect(calls.length).toBeGreaterThan(0);
    for (const [options] of calls) {
      expect(options['rerank']).toBe(true);
      expect(options['rerankWeight']).toBe(0.5);
    }
  });

  it('mirrors hybrid hit flags per query alongside semantic', async () => {
    const res = await runSearchEval(makeRunner('src/tools/search.ts'), makeDetector('p1'), {
      cases: [{ query: 'q', expectedFiles: ['src/tools/search.ts'] }],
    });
    expect(res.perQuery?.[0]?.hybrid).toMatchObject({ top1: true, top3: true, top10: true });
  });

  it('aggregates hit rates per query-id category', async () => {
    // Runner hits only `src/tools/search.ts`, so the pt- case (different gold)
    // misses while the impl- and unprefixed cases hit — distinct rates per
    // category prove the bucketing keys off the id prefix.
    const res = await runSearchEval(makeRunner('src/tools/search.ts'), makeDetector('p1'), {
      cases: [
        { id: 'impl-a', query: 'q1', expectedFiles: ['src/tools/search.ts'] },
        { id: 'pt-b', query: 'q2', expectedFiles: ['src/missing/gold.rs'] },
        { id: 'legacy-style-id', query: 'q3', expectedFiles: ['src/tools/search.ts'] },
      ],
    });
    expect(res.success).toBe(true);
    expect(res.byCategory?.['impl']?.semantic).toMatchObject({ n: 1, top1: 100, top10: 100 });
    expect(res.byCategory?.['pt']?.semantic).toMatchObject({ n: 1, top1: 0, top10: 0 });
    // Unknown prefixes fall into "orig" (the original known-item set).
    expect(res.byCategory?.['orig']?.semantic).toMatchObject({ n: 1, top10: 100 });
    expect(res.byCategory?.['impl']?.hybrid).toMatchObject({ n: 1, top10: 100 });
    // Per-category exact (grep/FTS5) breakdown is now emitted too (P2.9) so the
    // grep-vs-vector contrast is measurable per category, not just in aggregate.
    expect(res.byCategory?.['impl']?.exact).toMatchObject({ n: 1, top1: 100, top10: 100 });
  });

  it('forwards the summary delivery flag to every search call (P2.9)', async () => {
    const runner = makeRunner('src/tools/search.ts');
    const res = await runSearchEval(runner, makeDetector('p1'), {
      cases: [{ query: 'q', expectedFiles: ['src/tools/search.ts'] }],
      summary: true,
    });
    expect(res.success).toBe(true);
    const calls = (runner.search as ReturnType<typeof vi.fn>).mock.calls as Array<
      [Record<string, unknown>]
    >;
    expect(calls.length).toBeGreaterThan(0);
    for (const [options] of calls) {
      expect(options['summary']).toBe(true);
    }
  });

  it('reports the rerank soft-default in `applied` and honors WQM_SEARCH_RERANK (P1.7)', async () => {
    const savedFlag = process.env['WQM_SEARCH_RERANK'];
    const savedWeight = process.env['WQM_SEARCH_RERANK_WEIGHT'];
    try {
      // Env unset ⇒ soft-default ON at the tuned weight (0.1). `applied` surfaces
      // the EFFECTIVE settings so an A/B run records what was actually measured.
      delete process.env['WQM_SEARCH_RERANK'];
      delete process.env['WQM_SEARCH_RERANK_WEIGHT'];
      const runnerOn = makeRunner('src/tools/search.ts');
      const resOn = await runSearchEval(runnerOn, makeDetector('p1'), {
        cases: [{ query: 'q', expectedFiles: ['src/tools/search.ts'] }],
      });
      expect(resOn.applied).toMatchObject({ rerank: true, rerankWeight: 0.1 });
      // Omitting rerank must NOT force a value onto the runner — the search()
      // soft-default resolves it, so eval stays faithful to production behavior.
      const onCalls = (runnerOn.search as ReturnType<typeof vi.fn>).mock.calls as Array<
        [Record<string, unknown>]
      >;
      for (const [options] of onCalls) {
        expect(options['rerank']).toBeUndefined();
      }

      // WQM_SEARCH_RERANK=0 disables the deployment default.
      process.env['WQM_SEARCH_RERANK'] = '0';
      const resOff = await runSearchEval(makeRunner('src/tools/search.ts'), makeDetector('p1'), {
        cases: [{ query: 'q', expectedFiles: ['src/tools/search.ts'] }],
      });
      expect(resOff.applied?.rerank).toBe(false);
    } finally {
      if (savedFlag === undefined) delete process.env['WQM_SEARCH_RERANK'];
      else process.env['WQM_SEARCH_RERANK'] = savedFlag;
      if (savedWeight === undefined) delete process.env['WQM_SEARCH_RERANK_WEIGHT'];
      else process.env['WQM_SEARCH_RERANK_WEIGHT'] = savedWeight;
    }
  });

  it('refuses the bundled dataset when the target tenant is not its home project', async () => {
    // The bundled dataset's gold paths describe only its own home repo. Running
    // it against a different tenant checks for files absent there → every query
    // falsely reports 0% ("poor"). The guard must refuse before parsing.
    const tmp = mkdtempSync(join(tmpdir(), 'searcheval-'));
    const dsAbs = join(tmp, BUNDLED_DATASET_REL);
    mkdirSync(dirname(dsAbs), { recursive: true });
    writeFileSync(dsAbs, 'name: home-only\nqueries: []\n');
    const saved = process.env['WQM_REPO_DIR'];
    process.env['WQM_REPO_DIR'] = tmp;
    try {
      // repoDir (WQM_REPO_DIR=tmp) → home tenant; the eval cwd → a DIFFERENT one.
      const detector = {
        getProjectInfo: vi.fn(async (p: string) =>
          p === tmp ? { projectId: 'wqm-home' } : { projectId: 'doc-v2' }
        ),
      } as unknown as ProjectDetector;
      const res = await runSearchEval(makeRunner('a.ts'), detector, {}); // no cases → bundled
      expect(res.success).toBe(false);
      expect(res.error).toMatch(/bundled dataset/i);
      expect(res.error).toContain('wqm-home');
      expect(res.error).toContain('doc-v2');
    } finally {
      if (saved === undefined) delete process.env['WQM_REPO_DIR'];
      else process.env['WQM_REPO_DIR'] = saved;
      rmSync(tmp, { recursive: true, force: true });
    }
  });

  it('runs the bundled dataset when the target IS its home project', async () => {
    const tmp = mkdtempSync(join(tmpdir(), 'searcheval-'));
    const dsAbs = join(tmp, BUNDLED_DATASET_REL);
    mkdirSync(dirname(dsAbs), { recursive: true });
    writeFileSync(
      dsAbs,
      'name: home\nqueries:\n  - id: q1\n    query: hello\n    expectedFiles:\n      - a.ts\n'
    );
    const saved = process.env['WQM_REPO_DIR'];
    process.env['WQM_REPO_DIR'] = tmp;
    try {
      // Same tenant for both the eval cwd and the dataset home → guard passes.
      const detector = {
        getProjectInfo: vi.fn(async () => ({ projectId: 'same-tenant' })),
      } as unknown as ProjectDetector;
      const res = await runSearchEval(makeRunner('a.ts'), detector, {});
      expect(res.success).toBe(true);
      expect(res.datasetSource).toBe(dsAbs);
      expect(res.projectId).toBe('same-tenant');
    } finally {
      if (saved === undefined) delete process.env['WQM_REPO_DIR'];
      else process.env['WQM_REPO_DIR'] = saved;
      rmSync(tmp, { recursive: true, force: true });
    }
  });
});
