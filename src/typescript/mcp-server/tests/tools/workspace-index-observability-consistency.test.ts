import { mkdtempSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';

import { afterEach, describe, expect, it, vi } from 'vitest';

import type { DaemonClient } from '../../src/clients/daemon-client.js';
import {
  runIncrementalCheckAll,
  runObserveAll,
  runObserveProject,
  runStatusAll,
  type RegistryProject,
} from '../../src/tools/indexed-projects-registry.js';

function writeRegistry(projects: RegistryProject[] = []): string {
  const dir = mkdtempSync(join(tmpdir(), 'wqm-observability-'));
  const registryPath = join(dir, 'indexed-projects.json');
  writeFileSync(
    registryPath,
    JSON.stringify({
      schemaVersion: 2,
      kind: 'indexed-projects',
      updatedAt: new Date().toISOString(),
      projects,
    })
  );
  return registryPath;
}

function makeDaemon(
  projects = [
    {
      project_id: 'daemon-only',
      project_name: 'daemon-project',
      project_root: join(tmpdir(), 'wqm-daemon-project-no-such-repo'),
      is_active: true,
    },
  ]
): DaemonClient {
  return {
    listProjects: vi.fn().mockResolvedValue({ projects, total_count: projects.length }),
    healthCheck: vi.fn().mockResolvedValue({ status: 1, components: [] }),
    getQueueStats: vi.fn().mockResolvedValue({
      pending_count: 0,
      in_progress_count: 0,
      completed_count: 10,
      failed_count: 0,
      stale_items_count: 0,
      by_item_type: {},
      by_collection: {},
    }),
    getProjectStatus: vi.fn().mockResolvedValue({
      found: true,
      project_id: 'daemon-only',
      is_active: true,
      pending_count: 0,
      in_progress_count: 0,
      failed_count: 0,
      done_count: 10,
      total_count: 10,
      percent_complete: 100,
    }),
  } as unknown as DaemonClient;
}

const originalQdrantUrl = process.env['QDRANT_URL'];
const originalDaemonEndpoint = process.env['WQM_DAEMON_ENDPOINT'];
const originalMemexdUrl = process.env['MEMEXD_GRPC_URL'];

afterEach(() => {
  if (originalQdrantUrl === undefined) delete process.env['QDRANT_URL'];
  else process.env['QDRANT_URL'] = originalQdrantUrl;
  if (originalDaemonEndpoint === undefined) delete process.env['WQM_DAEMON_ENDPOINT'];
  else process.env['WQM_DAEMON_ENDPOINT'] = originalDaemonEndpoint;
  if (originalMemexdUrl === undefined) delete process.env['MEMEXD_GRPC_URL'];
  else process.env['MEMEXD_GRPC_URL'] = originalMemexdUrl;
  vi.unstubAllGlobals();
});

describe('workspace_index observability consistency', () => {
  it('uses runtime service endpoints instead of generated localhost defaults', async () => {
    process.env['QDRANT_URL'] = 'http://runtime-qdrant:7333';
    process.env['WQM_DAEMON_ENDPOINT'] = '127.0.0.1:54321';
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ status: 200 }));

    const registryPath = writeRegistry([
      {
        name: 'registered-project',
        projectId: 'registered',
        root: join(tmpdir(), 'wqm-registered-no-such-repo'),
        branches: [],
      },
    ]);
    const daemon = makeDaemon([
      {
        project_id: 'registered',
        project_name: 'registered-project',
        project_root: join(tmpdir(), 'wqm-registered-no-such-repo'),
        is_active: true,
      },
    ]);

    const result = (await runStatusAll({ registryPath }, daemon)) as {
      projects: Array<{
        qdrant: { endpoint: string; ok: boolean };
        daemonTcp: { host: string; port: number };
      }>;
    };

    expect(fetch).toHaveBeenCalledWith(
      'http://runtime-qdrant:7333/collections',
      expect.objectContaining({ method: 'GET' })
    );
    expect(result.projects[0]).toMatchObject({
      qdrant: { endpoint: 'http://runtime-qdrant:7333/collections', ok: true },
      daemonTcp: { host: '127.0.0.1', port: 54321 },
    });
  });

  it('includes daemon-only projects in observe project/all and incremental check all', async () => {
    process.env['QDRANT_URL'] = 'http://runtime-qdrant:7333';
    process.env['WQM_DAEMON_ENDPOINT'] = '127.0.0.1:54321';
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ status: 200 }));

    const registryPath = writeRegistry();
    const daemon = makeDaemon();

    const one = (await runObserveProject({ registryPath, projectId: 'daemon-only' }, daemon)) as {
      observation: { project: string };
    };
    const all = (await runObserveAll({ registryPath }, daemon)) as {
      count: number;
      observations: Array<{ project: string }>;
    };
    const incremental = (await runIncrementalCheckAll({ registryPath }, daemon)) as {
      results: Array<{ project: string; branch: string }>;
    };

    expect(one.observation.project).toBe('daemon-project');
    expect(all).toMatchObject({
      count: 1,
      observations: [{ project: 'daemon-project' }],
    });
    expect(incremental.results).toEqual([
      expect.objectContaining({ project: 'daemon-project', branch: 'main' }),
    ]);
  });
});
