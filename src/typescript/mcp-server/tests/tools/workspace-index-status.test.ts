import { mkdtempSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';

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
    project_root: '/home/dev/repos/workspace-qdrant-mcp',
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

function makeDaemon(
  projectId = '367157a01d98',
  statusOverrides: Record<string, unknown> = {}
): {
  daemon: DaemonClient;
  getProjectStatus: ReturnType<typeof vi.fn>;
  listProjects: ReturnType<typeof vi.fn>;
  listFailedItems: ReturnType<typeof vi.fn>;
} {
  const getProjectStatus = vi.fn().mockResolvedValue(makeStatus(projectId, statusOverrides));
  const listProjects = vi.fn().mockResolvedValue({
    projects: [{ project_id: 'fallback-active' }],
  });
  const listFailedItems = vi.fn().mockResolvedValue({ items: [], total_failed: 0 });
  return {
    daemon: { getProjectStatus, listProjects, listFailedItems } as unknown as DaemonClient,
    getProjectStatus,
    listProjects,
    listFailedItems,
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
          projectPath: '/home/dev/repos/workspace-qdrant-mcp',
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
      { hostCwd: '/home/dev/repos/workspace-qdrant-mcp' },
      () => handleWorkspaceIndex({ action: 'project_status' }, daemon, detector)
    )) as Record<string, unknown>;

    expect(getProjectInfo).toHaveBeenCalledWith(
      '/home/dev/repos/workspace-qdrant-mcp',
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
        cwd: '/home/dev/repos/workspace-qdrant-mcp/src/typescript/mcp-server',
      },
      daemon,
      detector
    )) as Record<string, unknown>;

    expect(getProjectInfo).toHaveBeenCalledWith(
      '/home/dev/repos/workspace-qdrant-mcp/src/typescript/mcp-server',
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
        project_name: 'example-service',
        project_root: '/home/dev/repos/example-service',
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
      indexing_active: true,
      indexing: {
        pending: 596,
        done: 2459,
        total: 3055,
      },
    });
    // `session_active` was a byte-for-byte alias of `is_active`; it must not
    // reappear as a redundant, misleading second signal.
    expect(result).not.toHaveProperty('session_active');
  });

  it('list_branches falls back to daemon-indexed projects missing from the registry', async () => {
    const listProjects = vi.fn().mockResolvedValue({
      projects: [
        {
          project_id: '9634ef90c02d',
          project_name: 'example-service',
          project_root: '/tmp/no-such-example-service',
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
      project: 'example-service',
      projectId: '9634ef90c02d',
      source: 'indexed',
      branches: [
        {
          name: 'main',
          kind: 'primary',
          path: '/tmp/no-such-example-service',
          status: 'inactive',
          watchEnabled: true,
          indexed: true,
        },
      ],
    });
  });
  it('list_branches accepts cwd as a project directory selector', async () => {
    const listProjects = vi.fn().mockResolvedValue({
      projects: [
        {
          project_id: '9634ef90c02d',
          project_name: 'example-service',
          project_root: '/tmp/no-such-example-service',
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

    const registryPath = join(
      mkdtempSync(join(tmpdir(), 'wqm-registry-')),
      'indexed-projects.json'
    );
    writeFileSync(
      registryPath,
      JSON.stringify({
        schemaVersion: 2,
        kind: 'indexed-projects',
        updatedAt: new Date().toISOString(),
        projects: [
          {
            name: 'Finance',
            root: '/home/dev/repos/Finance',
            branches: [
              {
                name: 'main',
                kind: 'primary',
                path: '/home/dev/repos/Finance',
                status: 'active',
                createdAt: new Date().toISOString(),
                lastSeenAt: new Date().toISOString(),
              },
            ],
          },
        ],
      })
    );

    const result = (await handleWorkspaceIndex(
      {
        action: 'list_branches',
        cwd: '/tmp/no-such-example-service',
        registryPath,
      },
      daemon
    )) as Record<string, unknown>;

    expect(result).toMatchObject({
      success: true,
      project: 'example-service',
      projectId: '9634ef90c02d',
      source: 'indexed',
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

  it('list_branches falls back to daemon-only projects by projectId', async () => {
    const { daemon, listProjects } = makeDaemon('daemon-only');
    // project_root must NOT be a live git checkout: branch synthesis calls
    // getCurrentBranch(root), so a real repo path makes the assertion depend on
    // that repo's current branch (non-hermetic). A non-existent path forces the
    // deterministic getCurrentBranch → null → 'main' fallback.
    listProjects.mockResolvedValue({
      projects: [
        {
          project_id: 'daemon-only',
          project_name: 'example-service',
          project_root: join(tmpdir(), 'wqm-daemon-only-no-such-repo'),
        },
      ],
    });

    const result = (await handleWorkspaceIndex(
      { action: 'list_branches', projectId: 'daemon-only' },
      daemon,
      undefined
    )) as Record<string, unknown>;

    expect(listProjects).toHaveBeenCalledWith({});
    expect(result).toMatchObject({
      success: true,
      project: 'example-service',
      projectId: 'daemon-only',
      branches: [
        expect.objectContaining({
          name: 'main',
          indexed: true,
          kind: 'primary',
          note: expect.stringContaining('Synthesized from daemon ListProjects'),
        }),
      ],
    });
  });

  it('surfaces the failing files (path + error + retry) plus a remediation hint', async () => {
    const { daemon, listFailedItems } = makeDaemon('367157a01d98', {
      failed_count: 2,
      done_count: 1931,
      total_count: 1933,
    });
    listFailedItems.mockResolvedValue({
      items: [
        {
          queue_id: 'q1',
          tenant_id: '367157a01d98',
          branch: 'main',
          collection: 'projects',
          item_type: 'file',
          op: 'Uplift',
          file_path: 'src/broken.rs',
          error_message: 'parse error at line 3',
          retry_count: 4,
          last_error_at: '2026-07-28T10:00:00Z',
          updated_at: '2026-07-28T10:00:00Z',
        },
      ],
      total_failed: 2,
    });

    const result = (await handleWorkspaceIndex(
      { action: 'indexing_status', projectId: '367157a01d98' },
      daemon,
      undefined
    )) as Record<string, unknown>;

    expect(listFailedItems).toHaveBeenCalledWith({ tenant_id: '367157a01d98', limit: 25 });
    expect(result).toMatchObject({
      success: true,
      indexing: { failed: 2 },
      failed_items: [
        {
          // queue_id is kept — it is the handle for `wqm queue retry <queue_id>`.
          queue_id: 'q1',
          file_path: 'src/broken.rs',
          op: 'Uplift',
          error_message: 'parse error at line 3',
          retry_count: 4,
          last_error_at: '2026-07-28T10:00:00Z',
        },
      ],
      // one shown of two → truncation flagged with the true total
      failed_items_truncated: { shown: 1, total: 2 },
    });
    // remediation leads with the per-file incremental retry, not the broad --all.
    expect(result.remediation).toContain('wqm queue retry <queue_id>');
    // compact projection — the tenant/collection/branch noise is dropped
    const items = result.failed_items as Array<Record<string, unknown>>;
    expect(items[0]).not.toHaveProperty('tenant_id');
    expect(items[0]).not.toHaveProperty('collection');
  });

  it('omits failed_items and never calls ListFailedItems when nothing failed', async () => {
    const { daemon, listFailedItems } = makeDaemon('367157a01d98');

    const result = (await handleWorkspaceIndex(
      { action: 'indexing_status', projectId: '367157a01d98' },
      daemon,
      undefined
    )) as Record<string, unknown>;

    expect(listFailedItems).not.toHaveBeenCalled();
    expect(result).not.toHaveProperty('failed_items');
    expect(result).not.toHaveProperty('remediation');
  });

  it('keeps the status usable when ListFailedItems itself fails (advisory)', async () => {
    const { daemon, listFailedItems } = makeDaemon('367157a01d98', {
      failed_count: 1,
      done_count: 1932,
      total_count: 1933,
    });
    listFailedItems.mockRejectedValue(new Error('daemon RPC error'));

    const result = (await handleWorkspaceIndex(
      { action: 'indexing_status', projectId: '367157a01d98' },
      daemon,
      undefined
    )) as Record<string, unknown>;

    expect(result).toMatchObject({ success: true, indexing: { failed: 1 } });
    expect(result).not.toHaveProperty('failed_items');
    // The remediation lever is still offered even without the detail list.
    expect(result.remediation).toContain('wqm queue retry');
  });

  it('reports inactive watcher with pending indexing as an explicit state', async () => {
    const { daemon } = makeDaemon('367157a01d98', {
      is_active: false,
      pending_count: 139,
      in_progress_count: 0,
      failed_count: 2,
      done_count: 2464,
      total_count: 2605,
      percent_complete: 94.6,
    });

    const result = (await handleWorkspaceIndex(
      { action: 'project_status', projectId: '367157a01d98' },
      daemon,
      undefined
    )) as Record<string, unknown>;

    expect(result).toMatchObject({
      success: true,
      is_active: false,
      status_reason: 'Watcher is inactive but the daemon still reports queued indexing work.',
      indexing: {
        pending: 139,
        failed: 2,
        state: 'inactive_with_pending_queue',
      },
    });
  });
});
