/**
 * End to end through GrepTool: the `checkout_stale` warning the daemon's
 * project status feeds (checkout-staleness.ts) reaches the grep response —
 * the page and the countOnly shape alike — and stays out when the main
 * checkout is current or the daemon cannot say. On 2026-11-12 DOC-V2's main
 * checkout was 412 commits behind origin/main and grep answered from it
 * without a word.
 */

import { beforeEach, describe, expect, it, vi } from 'vitest';
import { GrepTool } from '../../src/tools/grep.js';
import type { DaemonClient } from '../../src/clients/daemon-client.js';
import type { GetProjectStatusResponse } from '../../src/clients/grpc-types.js';
import type { SqliteStateManager } from '../../src/clients/sqlite-state-manager.js';
import type { ProjectDetector } from '../../src/utils/project-detector.js';
import { resetIndexStalenessCacheForTests } from '../../src/tools/index-staleness.js';

vi.mock('../../src/utils/git-utils.js', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../src/utils/git-utils.js')>();
  return { ...actual, getCurrentBranch: vi.fn().mockReturnValue('main') };
});

/** A daemon with one grep hit and the given project status (or a failing one). */
function daemon(status: Partial<GetProjectStatusResponse> | Error): DaemonClient {
  const target: Record<string, unknown> = {
    textSearch: vi.fn().mockResolvedValue({
      matches: [
        {
          file_path: '/repo/src/a.ts',
          line_number: 1,
          content: 'export const x = 1;',
          context_before: [],
          context_after: [],
        },
      ],
      total_matches: 1,
      truncated: false,
    }),
    getProjectStatus:
      status instanceof Error
        ? vi.fn().mockRejectedValue(status)
        : vi.fn().mockResolvedValue({ found: true, ...status }),
  };
  return new Proxy(target, {
    get(t: Record<string, unknown>, prop: string | symbol) {
      if (typeof prop === 'string' && prop in t) return t[prop];
      if (prop === 'then' || typeof prop === 'symbol') return undefined;
      return () => Promise.resolve(undefined);
    },
  }) as unknown as DaemonClient;
}

function stateManager(): SqliteStateManager {
  return {
    logSearchEvent: vi.fn(),
    updateSearchEvent: vi.fn(),
    updateSearchEventEconomy: vi.fn(),
    getProjectById: vi.fn().mockReturnValue({ data: { project_path: '/repo' } }),
    getWatchFolderIdByTenantId: vi.fn().mockReturnValue('watch-a'),
    getBaseBranch: vi.fn().mockReturnValue('main'),
    getIsTestByFilePaths: vi.fn().mockReturnValue(new Map()),
    getFileAnnotationsByFilePaths: vi.fn().mockReturnValue(new Map()),
  } as unknown as SqliteStateManager;
}

/** A fresh tenant per test: both read caches are keyed by tenant. */
let seq = 0;
function detector(): ProjectDetector {
  seq += 1;
  return {
    findProjectRoot: vi.fn().mockReturnValue('/repo'),
    getProjectInfo: vi
      .fn()
      .mockResolvedValue({ projectId: `grep-lag-${seq}`, projectPath: '/repo' }),
  } as unknown as ProjectDetector;
}

const LAGGING = { checkout_trunk: 'main', checkout_behind_commits: 412, checkout_ahead_commits: 0 };

describe('grep checkout_stale', () => {
  beforeEach(() => resetIndexStalenessCacheForTests());

  it('rides on a grep page when the main checkout trails origin', async () => {
    const tool = new GrepTool(daemon(LAGGING), detector(), stateManager());
    const res = await tool.grep({ pattern: 'const x', cwd: '/repo' });
    expect(res.success).toBe(true);
    expect(res.matches).toHaveLength(1);
    expect(res.checkout_stale).toMatchObject({ trunk: 'main', behind: 412 });
    expect(res.checkout_stale?.message).toContain('git pull --ff-only');
  });

  it('rides on the countOnly shape too', async () => {
    const tool = new GrepTool(daemon(LAGGING), detector(), stateManager());
    const res = await tool.grep({ pattern: 'const x', cwd: '/repo', countOnly: true });
    expect(res.total_matches).toBe(1);
    expect(res.checkout_stale?.behind).toBe(412);
  });

  it('stays out when the checkout is current or below the line', async () => {
    for (const behind of [0, 19]) {
      const tool = new GrepTool(
        daemon({ ...LAGGING, checkout_behind_commits: behind }),
        detector(),
        stateManager()
      );
      const res = await tool.grep({ pattern: 'const x', cwd: '/repo' });
      expect(res.matches).toHaveLength(1);
      expect(res.checkout_stale, `behind=${behind}`).toBeUndefined();
    }
  });

  it('a daemon that cannot answer leaves grep working and silent', async () => {
    const tool = new GrepTool(daemon(new Error('UNAVAILABLE')), detector(), stateManager());
    const res = await tool.grep({ pattern: 'const x', cwd: '/repo' });
    expect(res.success).toBe(true);
    expect(res.matches).toHaveLength(1);
    expect(res.checkout_stale).toBeUndefined();
  });
});
