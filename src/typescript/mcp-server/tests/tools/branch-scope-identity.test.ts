/**
 * resolveProjectIdentity: an explicit `projectId` must still yield a
 * `projectPath` (looked up in the registry) so branch scoping survives.
 * Regression for the field-reported path: cwd resolution unavailable →
 * caller falls back to projectId → the read silently lost its branch filter
 * and returned cross-branch stale content generations.
 */

import { describe, it, expect, vi } from 'vitest';
import { resolveProjectIdentity } from '../../src/tools/branch-scope.js';
import type { ProjectDetector } from '../../src/utils/project-detector.js';
import type { SqliteStateManager } from '../../src/clients/sqlite-state-manager.js';

describe('resolveProjectIdentity', () => {
  it('completes projectPath from the registry for an explicit projectId', async () => {
    const detector = { getProjectInfo: vi.fn() } as unknown as ProjectDetector;
    const getProjectById = vi.fn().mockReturnValue({ data: { project_path: '/repo/a' } });
    const stateManager = { getProjectById } as unknown as SqliteStateManager;

    const identity = await resolveProjectIdentity(detector, 'tenant-a', true, stateManager);

    expect(identity).toEqual({
      projectId: 'tenant-a',
      projectPath: '/repo/a',
      source: 'projectId',
    });
    expect(getProjectById).toHaveBeenCalledWith('tenant-a');
    // Explicit id short-circuits cwd detection entirely.
    expect(detector.getProjectInfo).not.toHaveBeenCalled();
  });

  it('leaves projectPath undefined without a stateManager (no registry to ask)', async () => {
    const detector = { getProjectInfo: vi.fn() } as unknown as ProjectDetector;
    const identity = await resolveProjectIdentity(detector, 'tenant-a');
    expect(identity).toEqual({
      projectId: 'tenant-a',
      projectPath: undefined,
      source: 'projectId',
    });
  });

  it('tolerates an unknown projectId (registry has no row)', async () => {
    const stateManager = {
      getProjectById: vi.fn().mockReturnValue({ data: null }),
    } as unknown as SqliteStateManager;
    const identity = await resolveProjectIdentity(
      {} as ProjectDetector,
      'ghost',
      true,
      stateManager
    );
    expect(identity).toEqual({ projectId: 'ghost', projectPath: undefined, source: 'projectId' });
  });

  it('still detects by cwd when no explicit projectId is given', async () => {
    const detector = {
      getProjectInfo: vi
        .fn()
        .mockResolvedValue({ projectId: 'detected', projectPath: '/repo/detected' }),
    } as unknown as ProjectDetector;
    const identity = await resolveProjectIdentity(detector, undefined);
    expect(identity).toEqual({
      projectId: 'detected',
      projectPath: '/repo/detected',
      source: 'cwd',
    });
  });
});
