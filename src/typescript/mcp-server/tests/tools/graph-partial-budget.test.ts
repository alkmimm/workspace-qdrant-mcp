/**
 * `graph action:"bridges"` / `"modules"` — a budget-cut pass must say so.
 *
 * Both daemon passes run under a wall-clock budget and, when it fires, return
 * whatever they had accumulated. That answer is NOT a smaller version of the
 * exact one: which sources the Brandes loop reached (or how many label
 * iterations ran) depends on machine load, so an identical call can return
 * different scores and a different top-K. Measured live on a 17k-node graph:
 * two identical `bridges` calls, both cut at ~20 s, every score different, the
 * last entry different — and the old response was byte-indistinguishable from
 * an exact result.
 *
 * The daemon now reports `partial` (plus what it processed); the tool must put
 * that in front of the caller as `hint` BEFORE the list, keep `partial` as an
 * always-present boolean, and stay quiet when the pass completed.
 */

import { describe, it, expect, vi } from 'vitest';

import { handleGraph } from '../../src/tools/graph.js';

const projectDetector = {
  detectProject: vi.fn().mockResolvedValue({ projectId: 'tenant-1' }),
} as never;

const ENTRY = {
  node_id: 'n1',
  symbol_name: 'take',
  symbol_type: 'method',
  file_path: 'branch_prune.rs',
  score: 0.19,
};

describe('graph bridges — budget-cut betweenness is labelled partial', () => {
  it('leads with a hint naming the processed/total sources and sets partial:true', async () => {
    const daemon = {
      computeBetweenness: vi.fn().mockResolvedValue({
        entries: [ENTRY],
        total: 17167,
        query_time_ms: 20380,
        partial: true,
        sources_processed: 1523,
        sources_total: 17167,
      }),
    } as never;

    const out = (await handleGraph(
      { action: 'bridges', projectId: 'tenant-1' },
      daemon,
      projectDetector
    )) as Record<string, unknown>;

    expect(out['partial']).toBe(true);
    const hint = out['hint'] as string;
    expect(hint).toContain('PARTIAL');
    expect(hint).toContain('1523 of 17167');
    // Actionable: the bound that makes the next run walk the same source set.
    expect(hint).toContain('maxSamples at or below 1523');
    // The caveat precedes the data it qualifies.
    const keys = Object.keys(out);
    expect(keys.indexOf('hint')).toBeLessThan(keys.indexOf('entries'));
    expect(out['entries']).toHaveLength(1);
  });

  it('is silent and partial:false when the pass completed (field absent on the wire)', async () => {
    // proto3 drops a false bool, so an exact run arrives WITHOUT `partial`.
    const daemon = {
      computeBetweenness: vi.fn().mockResolvedValue({
        entries: [ENTRY],
        total: 5,
        query_time_ms: 12,
        sources_processed: 5,
        sources_total: 5,
      }),
    } as never;

    const out = (await handleGraph(
      { action: 'bridges', projectId: 'tenant-1' },
      daemon,
      projectDetector
    )) as Record<string, unknown>;

    expect(out['partial']).toBe(false);
    expect(out['hint']).toBeUndefined();
  });
});

describe('graph modules — budget-cut label propagation is labelled partial', () => {
  const community = {
    community_id: 0,
    member_count: 3,
    members: [{ node_id: 'a', symbol_name: 'A', symbol_type: 'struct', file_path: 'a.rs' }],
  };

  it('leads with a hint naming the completed iterations and sets partial:true', async () => {
    const daemon = {
      detectCommunities: vi.fn().mockResolvedValue({
        communities: [community],
        total_communities: 885,
        query_time_ms: 20001,
        partial: true,
        iterations: 3,
        converged: false,
      }),
    } as never;

    const out = (await handleGraph(
      { action: 'modules', projectId: 'tenant-1' },
      daemon,
      projectDetector
    )) as Record<string, unknown>;

    expect(out['partial']).toBe(true);
    expect(out['iterations']).toBe(3);
    expect(out['converged']).toBe(false);
    const hint = out['hint'] as string;
    expect(hint).toContain('PARTIAL');
    expect(hint).toContain('3 completed iteration');
    const keys = Object.keys(out);
    expect(keys.indexOf('hint')).toBeLessThan(keys.indexOf('communities'));
  });

  it('is silent and partial:false on a converged pass', async () => {
    const daemon = {
      detectCommunities: vi.fn().mockResolvedValue({
        communities: [community],
        total_communities: 1,
        query_time_ms: 374,
        iterations: 4,
        converged: true,
      }),
    } as never;

    const out = (await handleGraph(
      { action: 'modules', projectId: 'tenant-1' },
      daemon,
      projectDetector
    )) as Record<string, unknown>;

    expect(out['partial']).toBe(false);
    expect(out['converged']).toBe(true);
    expect(out['hint']).toBeUndefined();
  });
});
