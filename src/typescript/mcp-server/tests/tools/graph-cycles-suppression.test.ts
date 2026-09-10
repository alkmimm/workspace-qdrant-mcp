/**
 * `graph action:"cycles"` suppression surfacing.
 *
 * The daemon drops symbols that too many callers resolved to before running
 * Tarjan — the shape of a name colliding with a language SDK method, which the
 * `weight >= 0.6` confidence gate cannot catch because a tenant-unique name
 * scores 0.7. A dropped node cannot appear in ANY cycle, so a short list is not
 * by itself evidence of a clean codebase.
 *
 * The tool must say so through `hint` — the same channel the test_gaps
 * reliability warning and the empty usages/impact caveat use — and place it
 * BEFORE `cycles`, so an agent reading top-down learns the list was filtered
 * before it draws a conclusion from its length.
 */

import { describe, it, expect, vi } from 'vitest';

import { handleGraph } from '../../src/tools/graph.js';

function daemonReturning(response: Record<string, unknown>) {
  return { detectCycles: vi.fn().mockResolvedValue(response) } as never;
}

const projectDetector = {
  detectProject: vi.fn().mockResolvedValue({ projectId: 'tenant-1' }),
} as never;

const ARGS = { action: 'cycles', projectId: 'tenant-1' };

const CYCLE = {
  members: [
    { node_id: 'svc', symbol_name: 'getViewUrl', symbol_type: 'method', file_path: 'resources_service.dart' },
    { node_id: 'repo', symbol_name: 'getResourceViewUrl', symbol_type: 'method', file_path: 'resources_repository.dart' },
  ],
  files: ['resources_repository.dart', 'resources_service.dart'],
  cross_file: true,
};

describe('graph cycles ubiquity suppression', () => {
  it('reports the suppressed count as a hint, ahead of the cycle list', async () => {
    const out = (await handleGraph(
      ARGS,
      daemonReturning({
        cycles: [CYCLE],
        total: 1,
        query_time_ms: 928,
        suppressed_ubiquitous: 7,
      }),
      projectDetector
    )) as Record<string, unknown>;

    const hint = out['hint'] as string;
    expect(hint).toContain('7 symbol(s)');
    // Ordering is the point: the caveat must precede the data it qualifies.
    const keys = Object.keys(out);
    expect(keys.indexOf('hint')).toBeLessThan(keys.indexOf('cycles'));
    // The cycles still ship, and the raw count stays readable alongside the hint.
    expect(out['cycles']).toHaveLength(1);
    expect(out['suppressed_ubiquitous']).toBe(7);
  });

  it('stays silent when nothing was suppressed', async () => {
    const out = (await handleGraph(
      ARGS,
      daemonReturning({
        cycles: [CYCLE],
        total: 1,
        query_time_ms: 12,
        suppressed_ubiquitous: 0,
      }),
      projectDetector
    )) as Record<string, unknown>;

    expect(out).not.toHaveProperty('hint');
    expect(out['cycles']).toHaveLength(1);
  });

  it('does not emit NaN against a daemon that predates the field', async () => {
    // A stale memexd omits the field entirely on the wire. The hint must not
    // appear at all rather than appear reading "NaN symbol(s)".
    const out = (await handleGraph(
      ARGS,
      daemonReturning({ cycles: [], total: 0, query_time_ms: 3 }),
      projectDetector
    )) as Record<string, unknown>;

    expect(out).not.toHaveProperty('hint');
    expect(out['cycles']).toHaveLength(0);
  });
});
