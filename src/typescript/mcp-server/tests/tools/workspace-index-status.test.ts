import { describe, it, expect, vi } from 'vitest';

import { handleWorkspaceIndex } from '../../src/tools/workspace-index.js';
import type { DaemonClient } from '../../src/clients/daemon-client.js';
import type { ProjectDetector } from '../../src/utils/project-detector.js';
import { runWithRequestContext } from '../../src/utils/request-context.js';

function makeStatus(projectId: string, overrides: Record<string, unknown> = {}) {
  return {
    found: true,
    project_id: projectId,
    project_name: 'workspace-qdrant-mcp',
    project_root: '/home/alkmimm/respositorios/workspace-qdrant-mcp',
    is_active: true,
    pending_count: 0,
    in_progress_count: 0,
    failed_count: 0,
    done_count: 1933,
    total_count: 1933,
    percent_complete: 100,
    ...overrides,
  };
}

function makeDaemon(projectId = '367157a01d98'): {
  daemon: DaemonClient;
  getProjectStatus: ReturnType<typeof vi.fn>;
  listProjects: ReturnType<typeof vi.fn>;
} {
  const getProjectStatus = vi.fn().mockResolvedValue(makeStatus(projectId));
  const listProjects = vi.fn().mockResolvedValue({
    projects: [{ project_id: 'fallback-active' }],
  });
  return {
    daemon: { getProjectStatus, listProjects } as unknown as DaemonClient,
    getProjectStatus,
    listProjects,
  };
}

function makeDetector(projectId: string | null): {
  detector: ProjectDetector;
  getProjectInfo: ReturnType<typeof vi.fn>;
} {
  const getProjectInfo = vi.fn().mockResolvedValue(
    projectId
      ? {
          projectId,
          projectPath: '/home/alkmimm/respositorios/workspace-qdrant-mcp',
          isActive: true,
        }
      : null
  );
  return { detector: { getProjectInfo } as unknown as ProjectDetector, getProjectInfo };
}

describe('workspace_index status resolution', () => {
  it('project_status resolves the current repo from request cwd when projectId is omitted', async () => {
    const { daemon, getProjectStatus, listProjects } = makeDaemon();
    const { detector, getProjectInfo } = makeDetector('367157a01d98');

    const result = (await runWithRequestContext(
      { hostCwd: '/home/alkmimm/respositorios/workspace-qdrant-mcp' },
      () => handleWorkspaceIndex({ action: 'project_status' }, daemon, detector)
    )) as Record<string, unknown>;

    expect(getProjectInfo).toHaveBeenCalledWith(
      '/home/alkmimm/respositorios/workspace-qdrant-mcp',
      false,
      { fallbackToSoleProject: true }
    );
    expect(listProjects).not.toHaveBeenCalled();
    expect(getProjectStatus).toHaveBeenCalledWith({ project_id: '367157a01d98' });
    expect(result).toMatchObject({
      success: true,
      action: 'project_status',
      project_id: '367157a01d98',
      project_name: 'workspace-qdrant-mcp',
    });
  });

  it('indexing_status resolves cwd explicitly and reports indexing progress', async () => {
    const { daemon, getProjectStatus, listProjects } = makeDaemon();
    const { detector, getProjectInfo } = makeDetector('367157a01d98');

    const result = (await handleWorkspaceIndex(
      {
        action: 'indexing_status',
        cwd: '/home/alkmimm/respositorios/workspace-qdrant-mcp/src/typescript/mcp-server',
      },
      daemon,
      detector
    )) as Record<string, unknown>;

    expect(getProjectInfo).toHaveBeenCalledWith(
      '/home/alkmimm/respositorios/workspace-qdrant-mcp/src/typescript/mcp-server',
      false,
      { fallbackToSoleProject: true }
    );
    expect(listProjects).not.toHaveBeenCalled();
    expect(getProjectStatus).toHaveBeenCalledWith({ project_id: '367157a01d98' });
    expect(result).toMatchObject({
      success: true,
      action: 'indexing_status',
      project_id: '367157a01d98',
      indexing: {
        pending: 0,
        in_progress: 0,
        failed: 0,
        done: 1933,
        total: 1933,
        percent: 100,
      },
    });
  });

  it('reports session activity separately from indexing activity', async () => {
    const getProjectStatus = vi.fn().mockResolvedValue(
      makeStatus('9634ef90c02d', {
        project_name: 'bws-engineer',
        project_root: '/home/alkmimm/respositorios/bws-engineer',
        is_active: false,
        pending_count: 596,
        done_count: 2459,
        total_count: 3055,
        percent_complete: 80.49,
      })
    );
    const daemon = {
      getProjectStatus,
      listProjects: vi.fn(),
    } as unknown as DaemonClient;

    const result = (await handleWorkspaceIndex(
      { action: 'project_status', projectId: '9634ef90c02d' },
      daemon
    )) as Record<string, unknown>;

    expect(result).toMatchObject({
      success: true,
      action: 'project_status',
      project_id: '9634ef90c02d',
      is_active: false,
      session_active: false,
      indexing_active: true,
      indexing: {
        pending: 596,
        done: 2459,
        total: 3055,
      },
    });
  });

  it('list_branches falls back to daemon-indexed projects missing from the registry', async () => {
    const listProjects = vi.fn().mockResolvedValue({
      projects: [
        {
          project_id: '9634ef90c02d',
          project_name: 'bws-engineer',
          project_root: '/tmp/no-such-bws-engineer',
          priority: 'high',
          is_active: false,
        },
      ],
      total_count: 1,
    });
    const daemon = {
      getProjectStatus: vi.fn(),
      listProjects,
    } as unknown as DaemonClient;

    const result = (await handleWorkspaceIndex(
      {
        action: 'list_branches',
        projectId: '9634ef90c02d',
        registryPath: '/tmp/workspace-qdrant-missing-registry.json',
      },
      daemon
    )) as Record<string, unknown>;

    expect(listProjects).toHaveBeenCalledWith({});
    expect(result).toMatchObject({
      success: true,
      project: 'bws-engineer',
      projectId: '9634ef90c02d',
      source: 'indexed',
      branches: [
        {
          name: 'main',
          kind: 'primary',
          path: '/tmp/no-such-bws-engineer',
          status: 'inactive',
          watchEnabled: true,
          indexed: true,
        },
      ],
    });
  });

  it('falls back to the first active project when cwd cannot be resolved', async () => {
    const { daemon, getProjectStatus, listProjects } = makeDaemon('fallback-active');
    const { detector } = makeDetector(null);

    await handleWorkspaceIndex(
      { action: 'indexing_status', cwd: '/unregistered' },
      daemon,
      detector
    );

    expect(listProjects).toHaveBeenCalledWith({ active_only: true });
    expect(getProjectStatus).toHaveBeenCalledWith({ project_id: 'fallback-active' });
  });
});
