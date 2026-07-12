/**
 * Tests for scratchpad write-time provenance (origin_branch / origin_cwd /
 * origin_worktree).
 *
 * The queue item's `branch` must stay "main" (the point id derives from it —
 * a real-branch value would fork a note's identity per branch); provenance
 * travels in dedicated origin_* payload fields instead. Fields the server
 * cannot determine are omitted, never fabricated.
 */

import { describe, it, expect, vi } from 'vitest';
import { storeScratchpad } from '../src/store-handlers.js';
import type { SqliteStateManager } from '../src/clients/sqlite-state-manager.js';
import type { ProjectDetector } from '../src/utils/project-detector.js';
import { runWithRequestContext } from '../src/utils/request-context.js';

function mockStateManager(): SqliteStateManager {
  return {
    enqueueUnified: vi.fn().mockResolvedValue({ status: 'ok', data: { queueId: 'q-1' } }),
    upsertScratchpadMirror: vi.fn(),
  } as unknown as SqliteStateManager;
}

function mockDetector(): ProjectDetector {
  return {
    getProjectInfo: vi.fn().mockResolvedValue({ projectId: 'proj-1' }),
  } as unknown as ProjectDetector;
}

/** The queue payload is the 5th positional arg of enqueueUnified. */
function payloadOf(sm: SqliteStateManager): Record<string, unknown> {
  return (sm.enqueueUnified as unknown as ReturnType<typeof vi.fn>).mock
    .calls[0][4] as Record<string, unknown>;
}

/** The queue item branch is the 7th positional arg of enqueueUnified. */
function branchOf(sm: SqliteStateManager): unknown {
  return (sm.enqueueUnified as unknown as ReturnType<typeof vi.fn>).mock.calls[0][6];
}

describe('storeScratchpad — write-time provenance', () => {
  it('stamps origin_branch and origin_worktree from the session git state', async () => {
    const sm = mockStateManager();

    await storeScratchpad({ content: 'note' }, sm, mockDetector(), {
      projectId: 'proj-1',
      currentBranch: 'feat/thing',
      isWorktree: true,
    });

    const payload = payloadOf(sm);
    expect(payload['origin_branch']).toBe('feat/thing');
    expect(payload['origin_worktree']).toBe(true);
  });

  it('keeps the queue item branch pinned to "main" (point-id stability)', async () => {
    const sm = mockStateManager();

    await storeScratchpad({ content: 'note' }, sm, mockDetector(), {
      projectId: 'proj-1',
      currentBranch: 'feat/thing',
      isWorktree: false,
    });

    expect(branchOf(sm)).toBe('main');
  });

  it('prefers an explicit branch arg over the session branch', async () => {
    const sm = mockStateManager();

    await storeScratchpad({ content: 'note', branch: 'wt/override' }, sm, mockDetector(), {
      projectId: 'proj-1',
      currentBranch: 'main',
      isWorktree: false,
    });

    expect(payloadOf(sm)['origin_branch']).toBe('wt/override');
  });

  it('omits origin_branch when nothing is known (no fabrication)', async () => {
    const sm = mockStateManager();

    await storeScratchpad({ content: 'note' }, sm, mockDetector(), {
      projectId: 'proj-1',
      currentBranch: null,
      isWorktree: false,
    });

    const payload = payloadOf(sm);
    expect('origin_branch' in payload).toBe(false);
    expect(payload['origin_worktree']).toBe(false);
  });

  it('stamps origin_cwd from the bound host cwd on HTTP requests', async () => {
    const sm = mockStateManager();

    await runWithRequestContext({ hostCwd: '/home/user/repos/app-wt-x' }, async () => {
      await storeScratchpad({ content: 'note' }, sm, mockDetector(), {
        projectId: 'proj-1',
        currentBranch: null,
        isWorktree: false,
      });
    });

    expect(payloadOf(sm)['origin_cwd']).toBe('/home/user/repos/app-wt-x');
  });

  it('omits origin_cwd on HTTP requests without a bound cwd (no container WORKDIR leak)', async () => {
    const sm = mockStateManager();

    await runWithRequestContext({}, async () => {
      await storeScratchpad({ content: 'note' }, sm, mockDetector(), {
        projectId: 'proj-1',
        currentBranch: null,
        isWorktree: false,
      });
    });

    expect('origin_cwd' in payloadOf(sm)).toBe(false);
  });

  it('stamps a client-side cwd outside a request context (stdio)', async () => {
    const sm = mockStateManager();

    await storeScratchpad({ content: 'note' }, sm, mockDetector(), {
      projectId: 'proj-1',
      currentBranch: null,
      isWorktree: false,
    });

    // Without an HTTP request context the effective cwd is client-side
    // (spawn cwd / WQM_DEFAULT_HOST_CWD) and safe to attribute.
    expect(typeof payloadOf(sm)['origin_cwd']).toBe('string');
  });
});
