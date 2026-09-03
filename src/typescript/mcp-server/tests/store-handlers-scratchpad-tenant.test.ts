/**
 * Tests for storeScratchpad tenant resolution.
 *
 * A scratchpad note must carry the current project's tenant_id so the
 * tenant-filtered recall lane can surface it on project-scoped search. The
 * resolution order — shared with every other project-scoped write via
 * `resolveScopedTenant`, and matched to the read surfaces — is:
 * explicit projectId → project detected from the effective cwd → the active
 * session project → global tenant.
 *
 * The session project ranks BELOW the cwd on purpose: it is set fire-and-forget
 * by `ensureClientProjectActive` and lags the caller, so preferring it silently
 * misrouted notes to the previously-active repo. See tests/store-tenant-cwd-
 * precedence.test.ts for that regression in full.
 */

import { describe, it, expect, vi } from 'vitest';
import { storeScratchpad } from '../src/store-handlers.js';
import type { SqliteStateManager } from '../src/clients/sqlite-state-manager.js';
import type { ProjectDetector } from '../src/utils/project-detector.js';
import { TENANT_GLOBAL } from '../src/constants/tenants.js';

function mockStateManager(): SqliteStateManager {
  return {
    enqueueUnified: vi.fn().mockResolvedValue({ status: 'ok', data: { queueId: 'q-1' } }),
    upsertScratchpadMirror: vi.fn(),
    getProjectById: vi.fn().mockReturnValue({ status: 'ok', data: null }),
  } as unknown as SqliteStateManager;
}

function mockDetector(projectId: string | null): ProjectDetector {
  return {
    getProjectInfo: vi.fn().mockResolvedValue(projectId ? { projectId } : null),
  } as unknown as ProjectDetector;
}

/** Minimal session slice storeScratchpad consumes (tenant + git provenance). */
function session(projectId: string | undefined) {
  return { projectId: projectId ?? null, currentBranch: null, isWorktree: false };
}

/** The tenant_id is the 3rd positional arg of enqueueUnified. */
function tenantOf(sm: SqliteStateManager): unknown {
  return (sm.enqueueUnified as unknown as ReturnType<typeof vi.fn>).mock.calls[0][2];
}

describe('storeScratchpad — tenant resolution', () => {
  it('prefers an explicit projectId arg over everything else', async () => {
    const sm = mockStateManager();
    const detector = mockDetector('detected-xyz');

    const res = await storeScratchpad(
      { content: 'note', projectId: 'explicit-123' },
      sm,
      detector,
      session('session-abc')
    );

    expect(res.success).toBe(true);
    expect(detector.getProjectInfo).not.toHaveBeenCalled();
    expect(tenantOf(sm)).toBe('explicit-123');
  });

  it('prefers the cwd-detected project over the active session project', async () => {
    const sm = mockStateManager();
    const detector = mockDetector('detected-xyz');

    const res = await storeScratchpad({ content: 'note' }, sm, detector, session('session-abc'));

    expect(res.success).toBe(true);
    expect(detector.getProjectInfo).toHaveBeenCalled();
    expect(tenantOf(sm)).toBe('detected-xyz');
  });

  it('detects the project from cwd when no session project is set', async () => {
    const sm = mockStateManager();
    const detector = mockDetector('detected-xyz');

    await storeScratchpad({ content: 'note' }, sm, detector, session(undefined));

    expect(detector.getProjectInfo).toHaveBeenCalled();
    expect(tenantOf(sm)).toBe('detected-xyz');
  });

  it('falls back to the session project when the cwd resolves nothing', async () => {
    const sm = mockStateManager();
    const detector = mockDetector(null);

    await storeScratchpad({ content: 'note' }, sm, detector, session('session-abc'));

    expect(tenantOf(sm)).toBe('session-abc');
  });

  it('falls back to the global tenant when nothing resolves', async () => {
    const sm = mockStateManager();
    const detector = mockDetector(null);

    await storeScratchpad({ content: 'note' }, sm, detector, session(undefined));

    expect(tenantOf(sm)).toBe(TENANT_GLOBAL);
  });

  it('falls back to global when project detection throws', async () => {
    const sm = mockStateManager();
    const detector = {
      getProjectInfo: vi.fn().mockRejectedValue(new Error('boom')),
    } as unknown as ProjectDetector;

    await storeScratchpad({ content: 'note' }, sm, detector, session(undefined));

    expect(tenantOf(sm)).toBe(TENANT_GLOBAL);
  });
});
