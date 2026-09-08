/**
 * `graph action:"usages"` / `"impact"` truncation honesty (#367).
 *
 * The daemon caps at `top_k` AFTER ordering nearest-first, so a page that comes
 * back full may have cut nodes that did not fit — and `usages`, which then keeps
 * only distance 1, can lose direct references entirely. Reporting that truncated
 * count as `total_impacted` with no flag is what let the tool answer a different
 * arbitrary subset on every call and still read as complete: measured live,
 * 50 of 3644 usages, a different 50 per run, labelled `total_impacted: 50`.
 */

import { describe, it, expect, vi } from 'vitest';

import { handleGraph } from '../../src/tools/graph.js';

function daemonReturning(response: Record<string, unknown>) {
  return { impactAnalysis: vi.fn().mockResolvedValue(response) } as never;
}

const projectDetector = {
  detectProject: vi.fn().mockResolvedValue({ projectId: 'tenant-1' }),
} as never;

/** `count` impacted nodes, all at `distance` hops. */
function nodesAt(count: number, distance: number) {
  return Array.from({ length: count }, (_, i) => ({
    node_id: `n${distance}-${i}`,
    symbol_name: `sym${distance}_${i}`,
    file_path: 'src/a.ts',
    impact_type: 'CALLS',
    distance,
    confidence: 0.9,
  }));
}

describe('graph usages/impact truncation is visible', () => {
  it('flags a saturated page and preserves the true all-depths total', async () => {
    // Page came back exactly full (2 of topK=2): more may exist beyond the cap.
    const out = (await handleGraph(
      { action: 'usages', symbol: 'x', topK: 2, projectId: 'tenant-1' },
      daemonReturning({ impacted_nodes: nodesAt(2, 1), total_impacted: 900 }),
      projectDetector
    )) as Record<string, unknown>;

    expect(out['truncated']).toBe(true);
    expect(out['total_impacted_all_depths']).toBe(900);
    expect(String(out['hint'])).toContain('topK');
  });

  it('does not flag an unsaturated page — there the count IS the total', async () => {
    const out = (await handleGraph(
      { action: 'usages', symbol: 'x', topK: 50, projectId: 'tenant-1' },
      daemonReturning({ impacted_nodes: nodesAt(3, 1), total_impacted: 3 }),
      projectDetector
    )) as Record<string, unknown>;

    expect(out['truncated']).toBe(false);
    expect(out['total_impacted']).toBe(3);
    expect(out).not.toHaveProperty('total_impacted_all_depths');
  });

  it('keeps BOTH caveats when a full page holds no direct reference', async () => {
    // Every returned node is transitive, so `usages` filters them all away and
    // the caller sees zero — a truncation artifact, not an absence. Both things
    // are true at once, and one `hint` must not overwrite the other.
    const out = (await handleGraph(
      { action: 'usages', symbol: 'x', topK: 2, projectId: 'tenant-1' },
      daemonReturning({ impacted_nodes: nodesAt(2, 2), total_impacted: 900 }),
      projectDetector
    )) as Record<string, unknown>;

    expect(out['impacted_nodes']).toHaveLength(0);
    expect(out['truncated']).toBe(true);
    const hint = String(out['hint']);
    expect(hint).toContain('topK');
    // The pre-existing "0 does not prove the symbol is unused" caveat survives.
    expect(hint).toContain('grep');
  });

  it('flags truncation on impact too, not only usages', async () => {
    const out = (await handleGraph(
      { action: 'impact', symbol: 'x', topK: 2, projectId: 'tenant-1' },
      daemonReturning({ impacted_nodes: nodesAt(2, 1), total_impacted: 900 }),
      projectDetector
    )) as Record<string, unknown>;

    expect(out['truncated']).toBe(true);
    // `impact` is transitive by definition, so the daemon's true total stands.
    expect(out['total_impacted']).toBe(900);
  });

  it('topK:0 removes the cap, so nothing is ever flagged truncated', async () => {
    const out = (await handleGraph(
      { action: 'usages', symbol: 'x', topK: 0, projectId: 'tenant-1' },
      daemonReturning({ impacted_nodes: nodesAt(120, 1), total_impacted: 120 }),
      projectDetector
    )) as Record<string, unknown>;

    expect(out['truncated']).toBe(false);
    expect(out['total_impacted']).toBe(120);
  });
});
