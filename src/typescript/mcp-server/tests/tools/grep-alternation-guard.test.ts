/**
 * Truncated-alternation absence-trap guard (field feedback 2026-07-15): a
 * regex alternation under `truncated:true` splits the visible page among the
 * branches, so "zero hits for term X on this page" proves nothing — an agent
 * read such a page and published two false "zero references" claims. The
 * response message must warn whenever an alternation pattern has a hidden
 * tail (daemon cap or byte-budget drop), and stay silent otherwise.
 */

import { describe, it, expect, vi } from 'vitest';
import { GrepTool, alternationTruncationHint } from '../../src/tools/grep.js';
import type { DaemonClient } from '../../src/clients/daemon-client.js';
import type { SqliteStateManager } from '../../src/clients/sqlite-state-manager.js';
import type { ProjectDetector } from '../../src/utils/project-detector.js';

function makeStateManager(): SqliteStateManager {
  return {
    getWatchFolderIdByTenantId: vi.fn().mockReturnValue(null),
    getBaseBranch: vi.fn().mockReturnValue(null),
    getProjectById: vi.fn().mockReturnValue({ data: null }),
  } as unknown as SqliteStateManager;
}

function makeProjectDetector(): ProjectDetector {
  return {
    findProjectRoot: vi.fn().mockReturnValue('/repo'),
    getProjectInfo: vi.fn().mockResolvedValue({ projectId: 'p-a' }),
  } as unknown as ProjectDetector;
}

function makeDaemon(response: unknown): DaemonClient {
  const textSearch = vi.fn().mockResolvedValue(response);
  const target: Record<string, unknown> = { textSearch };
  return new Proxy(target, {
    get(t: Record<string, unknown>, prop: string | symbol) {
      if (typeof prop === 'string' && prop in t) return t[prop];
      if (prop === 'then' || typeof prop === 'symbol') return undefined;
      return () => Promise.resolve(undefined);
    },
  }) as unknown as DaemonClient;
}

const match = (file: string, line: number, content: string): Record<string, unknown> => ({
  file_path: file,
  line_number: line,
  content,
  context_before: [],
  context_after: [],
  file_size: 100,
});

describe('alternationTruncationHint (pure)', () => {
  it('fires only for a regex alternation with a hidden tail', () => {
    expect(alternationTruncationHint(true, 'a|b|c', true)).toMatch(/do NOT use this result/);
    expect(alternationTruncationHint(true, 'a|b|c', false)).toBeUndefined(); // full set visible
    expect(alternationTruncationHint(false, 'a|b|c', true)).toBeUndefined(); // literal "|", not alternation
    expect(alternationTruncationHint(true, 'singleTerm', true)).toBeUndefined(); // no alternation
  });
});

describe('GrepTool — truncated alternation guard', () => {
  it('warns when a regex alternation result is daemon-truncated', async () => {
    const tool = new GrepTool(
      makeDaemon({
        matches: [match('/repo/a.dart', 1, 'rejectionReason'), match('/repo/b.dart', 2, 'reviewNote')],
        total_matches: 175,
        truncated: true,
      }),
      makeProjectDetector(),
      makeStateManager()
    );

    const res = await tool.grep({
      pattern: 'rejectionReason|reviewNote|approvalNote|approverName',
      regex: true,
      scope: 'project',
      projectId: 'p-a',
    });

    expect(res.matches.length).toBeGreaterThan(0);
    expect(res.truncated).toBe(true);
    expect(res.message).toMatch(/alternation/i);
    expect(res.message).toMatch(/one term per query/);
  });

  it('stays silent for an untruncated alternation result', async () => {
    const tool = new GrepTool(
      makeDaemon({
        matches: [match('/repo/a.dart', 1, 'rejectionReason')],
        total_matches: 1,
        truncated: false,
      }),
      makeProjectDetector(),
      makeStateManager()
    );

    const res = await tool.grep({
      pattern: 'rejectionReason|reviewNote',
      regex: true,
      scope: 'project',
      projectId: 'p-a',
    });

    expect(res.message ?? '').not.toMatch(/alternation/i);
  });

  it('stays silent for a truncated NON-regex result (literal "|" is not alternation)', async () => {
    const tool = new GrepTool(
      makeDaemon({
        matches: [match('/repo/a.sh', 1, 'cmd | tail')],
        total_matches: 500,
        truncated: true,
      }),
      makeProjectDetector(),
      makeStateManager()
    );

    const res = await tool.grep({
      pattern: 'cmd | tail',
      scope: 'project',
      projectId: 'p-a',
    });

    expect(res.message ?? '').not.toMatch(/alternation/i);
  });
});
