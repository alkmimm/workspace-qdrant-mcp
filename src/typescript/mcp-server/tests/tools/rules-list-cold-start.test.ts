/**
 * Cold-start graceful degradation for `rules action=list`.
 *
 * A freshly (re)started MCP container pays Qdrant connection warmup on its FIRST
 * scroll, which can exceed the MCP client's request timeout (surfacing as
 * -32001) even though a retry succeeds. `listRules` now bounds the scroll with a
 * deadline and falls back to the local rules mirror instead of hanging.
 */

import { describe, it, expect, vi } from 'vitest';
import type { QdrantClient } from '@qdrant/js-client-rest';
import { listRules } from '../../src/tools/rules-list.js';
import type { SqliteStateManager } from '../../src/clients/sqlite-state-manager.js';
import type { ProjectDetector } from '../../src/utils/project-detector.js';

function makeDetector(): ProjectDetector {
  return {
    findProjectRoot: vi.fn().mockReturnValue('/some/path'),
    getProjectInfo: vi.fn().mockResolvedValue(null),
  } as unknown as ProjectDetector;
}

function makeStateManager(mirrorRows: unknown[]): SqliteStateManager {
  return {
    listRulesMirror: vi.fn().mockReturnValue(mirrorRows),
  } as unknown as SqliteStateManager;
}

const MIRROR_ROW = {
  ruleId: 'r1',
  ruleText: 'Rule served from the local mirror',
  scope: 'global',
  tenantId: null,
  createdAt: '2026-01-01T00:00:00.000Z',
  updatedAt: '2026-01-01T00:00:00.000Z',
};

/** A scroll that never settles — models a cold connection warmup that outlasts
 *  the deadline. */
function hangingQdrant(): QdrantClient {
  return { scroll: vi.fn(() => new Promise(() => undefined)) } as unknown as QdrantClient;
}

describe('listRules — cold-start deadline fallback', () => {
  it('REGRESSION: a slow (cold) Qdrant scroll falls back to the mirror instead of hanging', async () => {
    vi.useFakeTimers();
    try {
      const qdrant = hangingQdrant();
      const stateManager = makeStateManager([MIRROR_ROW]);

      const promise = listRules(qdrant, stateManager, makeDetector(), { scope: 'global' });
      // Advance past the internal scroll deadline (~2.5s).
      await vi.advanceTimersByTimeAsync(3000);
      const res = await promise;

      expect(res.success).toBe(true);
      expect(res.rules).toHaveLength(1);
      expect(res.rules[0].content).toBe('Rule served from the local mirror');
      expect(res.message).toMatch(/mirror/i);
      expect(stateManager.listRulesMirror).toHaveBeenCalledTimes(1);
    } finally {
      vi.useRealTimers();
    }
  });

  it('returns Qdrant results on a fast scroll (normal path unchanged; mirror untouched)', async () => {
    const qdrant = {
      scroll: vi.fn().mockResolvedValue({
        points: [{ id: 'q1', payload: { content: 'Rule from Qdrant', scope: 'global' } }],
      }),
    } as unknown as QdrantClient;
    const stateManager = makeStateManager([MIRROR_ROW]);

    const res = await listRules(qdrant, stateManager, makeDetector(), { scope: 'global' });

    expect(res.rules).toHaveLength(1);
    expect(res.rules[0].content).toBe('Rule from Qdrant');
    expect(res.message).toMatch(/Found 1 rule/);
    expect(stateManager.listRulesMirror).not.toHaveBeenCalled();
  });

  it('falls back to the mirror when the Qdrant scroll errors (pre-existing behavior)', async () => {
    const qdrant = {
      scroll: vi.fn().mockRejectedValue(new Error('Collection not found')),
    } as unknown as QdrantClient;
    const stateManager = makeStateManager([MIRROR_ROW]);

    const res = await listRules(qdrant, stateManager, makeDetector(), { scope: 'global' });

    expect(res.rules).toHaveLength(1);
    expect(res.message).toMatch(/mirror/i);
  });

  it('returns a soft cold-start message when Qdrant is slow AND the mirror is empty', async () => {
    vi.useFakeTimers();
    try {
      const qdrant = hangingQdrant();
      const stateManager = makeStateManager([]); // empty mirror → readRulesFromMirror returns null

      const promise = listRules(qdrant, stateManager, makeDetector(), { scope: 'global' });
      await vi.advanceTimersByTimeAsync(3000);
      const res = await promise;

      expect(res.success).toBe(false);
      expect(res.rules).toHaveLength(0);
      expect(res.message).toMatch(/cold start|slow/i);
    } finally {
      vi.useRealTimers();
    }
  });
});
