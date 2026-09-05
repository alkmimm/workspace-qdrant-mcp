/**
 * `graph` carries the same read-side project echo as search/grep/list/retrieve —
 * including `project_path` for an explicit projectId, which needs the registry
 * (stateManager) the dispatcher now threads in.
 */
import { describe, it, expect, vi } from 'vitest';
import { handleGraph } from '../../src/tools/graph.js';
import type { DaemonClient } from '../../src/clients/daemon-client.js';
import type { ProjectDetector } from '../../src/utils/project-detector.js';
import type { SqliteStateManager } from '../../src/clients/sqlite-state-manager.js';
import { runWithRequestContext } from '../../src/utils/request-context.js';

function client(): DaemonClient {
  return {
    getGraphStats: vi
      .fn()
      .mockResolvedValue({ total_nodes: 1, total_edges: 0, nodes_by_type: {}, edges_by_type: {} }),
  } as unknown as DaemonClient;
}

const detector = {
  getProjectInfo: vi.fn().mockResolvedValue({ projectId: 'auto-tenant', projectPath: '/auto' }),
} as unknown as ProjectDetector;

const stateManager = {
  getProjectById: vi.fn().mockReturnValue({ data: { project_path: '/registry/repo' } }),
} as unknown as SqliteStateManager;

describe('graph read-side project echo', () => {
  it('completes project_path from the registry for an explicit projectId', async () => {
    const result = (await handleGraph(
      { action: 'stats', projectId: 't1' },
      client(),
      detector,
      stateManager
    )) as Record<string, unknown>;
    expect(result['tenant_id']).toBe('t1');
    expect(result['project_id']).toBe('t1');
    expect(result['project_path']).toBe('/registry/repo');
    expect(result['project_source']).toBe('projectId');
  });

  it('labels a cwd-resolved tenant, and a sticky cwd as sticky-cwd', async () => {
    const plain = (await handleGraph({ action: 'stats' }, client(), detector)) as Record<
      string,
      unknown
    >;
    expect(plain['project_id']).toBe('auto-tenant');
    expect(plain['project_path']).toBe('/auto');
    expect(plain['project_source']).toBe('cwd');

    const sticky = (await runWithRequestContext({ hostCwd: '/auto', cwdSource: 'sticky' }, () =>
      handleGraph({ action: 'stats' }, client(), detector)
    )) as Record<string, unknown>;
    expect(sticky['project_source']).toBe('sticky-cwd');
  });
});
