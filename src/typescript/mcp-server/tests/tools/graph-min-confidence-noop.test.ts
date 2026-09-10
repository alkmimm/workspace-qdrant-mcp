/**
 * `minConfidence` must say when it removed nothing (#383).
 *
 * On a real `usages` result the confidences take three values, all >= 0.85, so
 * every threshold from 0.1 to 0.85 — including the natural "medium confidence"
 * choice of 0.5 — drops nothing and returns a response byte-identical to
 * omitting the parameter. The caller cannot tell "my filter ran and kept
 * everything" from "my filter did nothing", which is why the field report read
 * it as "0.5 is the floor of almost everything, not a filter".
 *
 * The daemon knows the count. These pin that it reaches the caller, and that a
 * filter which DID work stays quiet rather than nagging.
 */

import { describe, it, expect, vi } from 'vitest';

import { handleGraph } from '../../src/tools/graph.js';

function daemonReturning(response: Record<string, unknown>) {
  return {
    impactAnalysis: vi.fn().mockResolvedValue(response),
    textSearchCount: vi.fn().mockResolvedValue({ count: 0 }),
  } as never;
}

const projectDetector = {
  detectProject: vi.fn().mockResolvedValue({ projectId: 'tenant-1' }),
} as never;

// Two direct references, both above any threshold a caller would reach for.
const NODES = [
  { node_id: 'a', symbol_name: 'callerOne', file_path: 'a.dart', impact_type: 'CALLS', distance: 1, confidence: 1.0 },
  { node_id: 'b', symbol_name: 'callerTwo', file_path: 'b.dart', impact_type: 'CALLS', distance: 1, confidence: 0.85 },
];

describe('graph usages minConfidence no-op reporting', () => {
  it('says so when the threshold removed nothing', async () => {
    const out = (await handleGraph(
      { action: 'usages', symbol: 'x', projectId: 'tenant-1', minConfidence: 0.5 },
      daemonReturning({
        impacted_nodes: NODES,
        total_impacted: 2,
        query_time_ms: 4,
        filtered_by_min_confidence: 0,
      }),
      projectDetector
    )) as Record<string, unknown>;

    const hint = out['hint'] as string;
    expect(hint).toContain('minConfidence:0.5 removed nothing');
    expect(hint).toContain('0.85');
    // The results still ship — the caller is told the filter was inert, not
    // denied the answer.
    expect(out['impacted_nodes']).toHaveLength(2);
    expect(out['filtered_by_min_confidence']).toBe(0);
  });

  it('stays quiet when the threshold actually removed something', async () => {
    const out = (await handleGraph(
      { action: 'usages', symbol: 'x', projectId: 'tenant-1', minConfidence: 0.9 },
      daemonReturning({
        impacted_nodes: [NODES[0]],
        total_impacted: 1,
        query_time_ms: 4,
        filtered_by_min_confidence: 7,
      }),
      projectDetector
    )) as Record<string, unknown>;

    expect(out['hint'] ?? '').not.toContain('removed nothing');
    expect(out['filtered_by_min_confidence']).toBe(7);
  });

  it('says nothing about the filter when the caller passed no threshold', async () => {
    // Without minConfidence there is no claim to correct: a count of 0 here
    // means "no filter ran", and warning about it would be noise on every call.
    const out = (await handleGraph(
      { action: 'usages', symbol: 'x', projectId: 'tenant-1' },
      daemonReturning({
        impacted_nodes: NODES,
        total_impacted: 2,
        query_time_ms: 4,
        filtered_by_min_confidence: 0,
      }),
      projectDetector
    )) as Record<string, unknown>;

    expect(out['hint'] ?? '').not.toContain('removed nothing');
  });
});
