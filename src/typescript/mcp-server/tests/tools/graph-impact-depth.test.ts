/**
 * `graph` impact/usages depth and the cuts the daemon reports, plus the
 * test_gaps ambiguity count (field feedback 2026-10-07, Finance):
 *   - `impact maxHops:1` still answered with distances 1–3: the depth was
 *     ignored.
 *   - a pinned `filePath` dropped every caller that reached the definition only
 *     through an ambiguous same-name call, and nothing said so.
 *   - test_gaps ranked a tested method first because its test's call was
 *     ambiguous, with nothing to tell "untested" from "tested, unpinned".
 */

import { describe, it, expect, vi } from 'vitest';

import { handleGraph } from '../../src/tools/graph.js';

const projectDetector = {
  detectProject: vi.fn().mockResolvedValue({ projectId: 'tenant-1' }),
} as never;

function daemon(response: Record<string, unknown>) {
  const impactAnalysis = vi.fn().mockResolvedValue(response);
  const detectTestGaps = vi.fn().mockResolvedValue(response);
  const textSearchCount = vi.fn().mockResolvedValue({ count: 0 });
  return {
    client: { impactAnalysis, detectTestGaps, textSearchCount } as never,
    impactAnalysis,
  };
}

function nodes(count: number, distance: number) {
  return Array.from({ length: count }, (_, i) => ({
    node_id: `n${distance}-${i}`,
    symbol_name: `sym${i}`,
    file_path: 'a.dart',
    impact_type: 'direct_caller',
    distance,
    confidence: 1,
  }));
}

describe('graph impact depth', () => {
  it('forwards maxHops for impact, and omits it when not given', async () => {
    const { client, impactAnalysis } = daemon({ impacted_nodes: [], total_impacted: 0 });
    await handleGraph(
      { action: 'impact', symbol: 'x', maxHops: 1, projectId: 'tenant-1' },
      client,
      projectDetector
    );
    expect(impactAnalysis).toHaveBeenLastCalledWith(expect.objectContaining({ max_hops: 1 }));

    await handleGraph({ action: 'impact', symbol: 'x', projectId: 'tenant-1' }, client, projectDetector);
    expect(impactAnalysis.mock.calls[1][0]).not.toHaveProperty('max_hops');
  });

  it('rejects a depth that is not a whole number of hops', async () => {
    const { client, impactAnalysis } = daemon({ impacted_nodes: [], total_impacted: 0 });
    for (const maxHops of [0, -1, 1.5]) {
      await expect(
        handleGraph({ action: 'impact', symbol: 'x', maxHops, projectId: 'tenant-1' }, client, projectDetector)
      ).rejects.toThrow(/maxHops/);
    }
    expect(impactAnalysis).not.toHaveBeenCalled();
  });

  it('says when the daemon capped the depth', async () => {
    const { client } = daemon({ impacted_nodes: nodes(1, 1), total_impacted: 1, max_hops: 5 });
    const out = (await handleGraph(
      { action: 'impact', symbol: 'x', maxHops: 9, projectId: 'tenant-1' },
      client,
      projectDetector
    )) as Record<string, unknown>;
    expect(out['max_hops']).toBe(5);
    expect(String(out['hint'])).toContain('maxHops:9 was capped at 5');
  });
});

describe('graph impact/usages report the cuts', () => {
  it('names the callers a pinned filePath dropped', async () => {
    const { client } = daemon({
      impacted_nodes: nodes(2, 1),
      total_impacted: 2,
      dropped_below_confidence_floor: 3,
    });
    for (const action of ['impact', 'usages']) {
      const out = (await handleGraph(
        { action, symbol: 'set', filePath: 'lib/writes.dart', projectId: 'tenant-1' },
        client,
        projectDetector
      )) as Record<string, unknown>;
      expect(out['dropped_below_confidence_floor']).toBe(3);
      const hint = String(out['hint']);
      expect(hint).toContain('3 caller(s)');
      expect(hint).toContain('omit filePath');
    }
  });

  it('says nothing about the floor when it dropped nobody', async () => {
    const { client } = daemon({ impacted_nodes: nodes(2, 1), total_impacted: 2 });
    const out = (await handleGraph(
      { action: 'impact', symbol: 'set', filePath: 'lib/writes.dart', projectId: 'tenant-1' },
      client,
      projectDetector
    )) as Record<string, unknown>;
    expect(out['hint'] ?? '').not.toContain('ambiguous');
  });

  it('flags a walk the node budget cut short', async () => {
    const { client } = daemon({
      impacted_nodes: nodes(2, 1),
      total_impacted: 2,
      node_budget_reached: true,
    });
    const out = (await handleGraph(
      { action: 'impact', symbol: 'x', projectId: 'tenant-1' },
      client,
      projectDetector
    )) as Record<string, unknown>;
    expect(String(out['hint'])).toContain('node budget');
  });
});

describe('graph usages over a one-hop walk', () => {
  // A daemon that honours max_hops walks direct references only and echoes 1,
  // so its total_impacted IS the number of direct references.
  it('reports the true direct total and truncates against it', async () => {
    const { client } = daemon({ impacted_nodes: nodes(2, 1), total_impacted: 7, max_hops: 1 });
    const out = (await handleGraph(
      { action: 'usages', symbol: 'x', topK: 2, projectId: 'tenant-1' },
      client,
      projectDetector
    )) as Record<string, unknown>;
    expect(out['total_impacted']).toBe(7);
    expect(out['truncated']).toBe(true);
    expect(out).not.toHaveProperty('total_impacted_all_depths');
    expect(String(out['hint'])).toContain('7 direct references exist and 2 are listed');
  });

  it('is not truncated when every direct reference fits', async () => {
    const { client } = daemon({ impacted_nodes: nodes(2, 1), total_impacted: 2, max_hops: 1 });
    const out = (await handleGraph(
      { action: 'usages', symbol: 'x', topK: 2, projectId: 'tenant-1' },
      client,
      projectDetector
    )) as Record<string, unknown>;
    expect(out['truncated']).toBe(false);
    expect(out['total_impacted']).toBe(2);
  });
});

describe('graph test_gaps ambiguous test callers', () => {
  it('says how many gaps a test calls only ambiguously', async () => {
    const { client } = daemon({
      gaps: [
        {
          node_id: 'bs',
          symbol_name: 'set',
          parent_symbol: 'FirestoreFinanceBatch',
          symbol_type: 'method',
          file_path: 'lib/writes.dart',
          production_dependents: 47,
          ambiguous_test_callers: 1,
        },
      ],
      total_production: 10,
      covered: 3,
      gap_count: 7,
      query_time_ms: 1,
      gaps_with_ambiguous_test_callers: 1,
    });
    const out = (await handleGraph({ action: 'test_gaps', projectId: 'tenant-1' }, client, projectDetector)) as Record<
      string,
      unknown
    >;
    const hint = String(out['hint']);
    expect(hint).toContain('1 gap(s) are called by a test through an ambiguous same-name call');
    expect((out['gaps'] as Array<Record<string, unknown>>)[0]['parent_symbol']).toBe('FirestoreFinanceBatch');
  });

  it('keeps the reliability warning first when both apply', async () => {
    const { client } = daemon({
      gaps: [],
      total_production: 10,
      covered: 0,
      gap_count: 10,
      query_time_ms: 1,
      reliability_warning: 'UNRELIABLE: test edges did not resolve',
      gaps_with_ambiguous_test_callers: 2,
    });
    const out = (await handleGraph({ action: 'test_gaps', projectId: 'tenant-1' }, client, projectDetector)) as Record<
      string,
      unknown
    >;
    const hint = String(out['hint']);
    expect(hint.startsWith('UNRELIABLE')).toBe(true);
    expect(hint).toContain('2 gap(s)');
  });
});
