/**
 * `graph action:"test_gaps"` reliability surfacing.
 *
 * The daemon judges whether the coverage figure is trustworthy (tests indexed
 * but reaching almost nothing = unresolved test→production edges, not untested
 * code) and ships `reliability_warning`. The tool must lift it into `hint` —
 * the same channel the empty usages/impact/relations caveat uses — and place it
 * BEFORE `gaps`, so an agent reading top-down sees "this is noise" ahead of the
 * ranking it would otherwise act on.
 *
 * Also pins that proto-loader's synthetic `_<field>` oneof markers never reach
 * the agent.
 */

import { describe, it, expect, vi } from 'vitest';

import { handleGraph } from '../../src/tools/graph.js';

function daemonReturning(response: Record<string, unknown>) {
  return { detectTestGaps: vi.fn().mockResolvedValue(response) } as never;
}

const projectDetector = {
  detectProject: vi.fn().mockResolvedValue({ projectId: 'tenant-1' }),
} as never;

const ARGS = { action: 'test_gaps', projectId: 'tenant-1' };

const GAPS = [
  { node_id: 'n1', symbol_name: 'handleUseCaseError', symbol_type: 'function', file_path: 'lib/api/handle-use-case-error.ts', production_dependents: 165 },
];

describe('graph test_gaps reliability warning', () => {
  it('lifts reliability_warning into hint, ahead of the gap ranking', async () => {
    const warning = 'UNRELIABLE: 178 test symbols are indexed, yet only 21 of 3555 …';
    const out = (await handleGraph(
      ARGS,
      daemonReturning({
        gaps: GAPS,
        total_production: 3555,
        covered: 21,
        gap_count: 3534,
        query_time_ms: 301,
        test_nodes: 178,
        reliability_warning: warning,
      }),
      projectDetector
    )) as Record<string, unknown>;

    expect(out['hint']).toBe(warning);
    // The raw field is consumed by the lift, not shipped twice.
    expect(out).not.toHaveProperty('reliability_warning');
    // Ordering is the point: the caveat must precede the data it discredits.
    const keys = Object.keys(out);
    expect(keys.indexOf('hint')).toBeLessThan(keys.indexOf('gaps'));
    // The gaps still ship — the caller is told to distrust them, not denied them.
    expect(out['gaps']).toHaveLength(1);
    expect(out['test_nodes']).toBe(178);
  });

  it('emits no hint when the daemon judged the measurement plausible', async () => {
    const out = (await handleGraph(
      ARGS,
      daemonReturning({
        gaps: GAPS,
        total_production: 8906,
        covered: 2535,
        gap_count: 6371,
        query_time_ms: 212,
        test_nodes: 4968,
      }),
      projectDetector
    )) as Record<string, unknown>;

    expect(out).not.toHaveProperty('hint');
    expect(out['test_nodes']).toBe(4968);
  });

  it('strips proto-loader synthetic oneof markers', async () => {
    // `oneofs: true` pairs every proto3 `optional` with `_<field>: "<field>"`.
    const out = (await handleGraph(
      ARGS,
      daemonReturning({
        gaps: GAPS,
        total_production: 10,
        covered: 1,
        gap_count: 9,
        query_time_ms: 1,
        test_nodes: 2,
        reliability_warning: 'UNRELIABLE: …',
        _reliability_warning: 'reliability_warning',
      }),
      projectDetector
    )) as Record<string, unknown>;

    expect(out).not.toHaveProperty('_reliability_warning');
    expect(out['hint']).toBe('UNRELIABLE: …');
  });

  it('keeps payload keys that merely start with an underscore', async () => {
    // A marker is `_x` whose value is exactly "x"; `_search_type: "semantic"`
    // is real data and must survive.
    const out = (await handleGraph(
      ARGS,
      daemonReturning({
        gaps: [{ ...GAPS[0], _search_type: 'semantic' }],
        total_production: 10,
        covered: 5,
        gap_count: 5,
        query_time_ms: 1,
        test_nodes: 2,
      }),
      projectDetector
    )) as Record<string, unknown>;

    expect((out['gaps'] as Record<string, unknown>[])[0]).toHaveProperty(
      '_search_type',
      'semantic'
    );
  });
});
