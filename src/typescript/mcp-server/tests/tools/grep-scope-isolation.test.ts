/**
 * Tenant-isolation contract for the grep (FTS5) tool — mirrors
 * exact-scope-isolation. A project-scoped grep (scope:"project", the default)
 * that finds nothing must NOT silently return matches from OTHER repositories;
 * it returns 0 matches plus a hint offering scope:"all". Passing scope:"all"
 * explicitly still searches across every repository on demand.
 */

import { describe, it, expect, vi } from 'vitest';
import { GrepTool } from '../../src/tools/grep.js';
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

function makeProjectDetector(projectId: string | undefined): ProjectDetector {
  return {
    findProjectRoot: vi.fn().mockReturnValue('/some/path'),
    getProjectInfo: vi.fn().mockResolvedValue(projectId ? { projectId } : null),
  } as unknown as ProjectDetector;
}

/** Daemon whose textSearch returns a hit ONLY on a cross-tenant call (tenant_id
 *  absent). Every other method (fire-and-forget instrumentation) is stubbed as a
 *  resolved promise so the tool's event logging never throws. */
function makeDaemon(impl: (req: { tenant_id?: string }) => Promise<unknown>): {
  daemon: DaemonClient;
  textSearch: ReturnType<typeof vi.fn>;
} {
  const textSearch = vi.fn().mockImplementation(impl);
  const target: Record<string, unknown> = { textSearch };
  const daemon = new Proxy(target, {
    get(t: Record<string, unknown>, prop: string | symbol) {
      if (typeof prop === 'string' && prop in t) return t[prop];
      if (prop === 'then' || typeof prop === 'symbol') return undefined;
      return () => Promise.resolve(undefined);
    },
  }) as unknown as DaemonClient;
  return { daemon, textSearch };
}

const OTHER_REPO_MATCH = {
  file_path: 'example-tool/tests/test_redaction.py',
  line_number: 55,
  content: 'JWT_SECRET = "not-a-real-secret"',
  context_before: [],
  context_after: [],
};

describe('GrepTool — tenant isolation (no silent cross-project fallback)', () => {
  it('REGRESSION: a project-scoped miss does NOT return matches from another repo', async () => {
    const { daemon, textSearch } = makeDaemon((req) =>
      req.tenant_id === undefined
        ? Promise.resolve({ matches: [OTHER_REPO_MATCH], total_matches: 1, truncated: false })
        : Promise.resolve({ matches: [], total_matches: 0, truncated: false })
    );
    const tool = new GrepTool(daemon, makeProjectDetector(undefined), makeStateManager());

    const res = await tool.grep({ pattern: 'JWT_SECRET', scope: 'project', projectId: 'project-a' });

    // No cross-tenant query was ever issued — the boundary was never crossed.
    expect(textSearch.mock.calls.some((c) => c[0].tenant_id === undefined)).toBe(false);
    expect(res.matches).toHaveLength(0);
    expect(res.message).toBeDefined();
    expect(res.message).toMatch(/scope:"all"/i);
  });

  it('passing scope:"all" explicitly still searches across every repository', async () => {
    const { daemon } = makeDaemon((req) =>
      req.tenant_id === undefined
        ? Promise.resolve({ matches: [OTHER_REPO_MATCH], total_matches: 1, truncated: false })
        : Promise.resolve({ matches: [], total_matches: 0, truncated: false })
    );
    const tool = new GrepTool(daemon, makeProjectDetector(undefined), makeStateManager());

    const res = await tool.grep({ pattern: 'JWT_SECRET', scope: 'all' });

    expect(res.matches).toHaveLength(1);
  });

  it('does NOT widen when the project-scoped grep already has results', async () => {
    const { daemon, textSearch } = makeDaemon((req) =>
      req.tenant_id !== undefined
        ? Promise.resolve({
            matches: [
              {
                file_path: 'project-a/src/x.ts',
                line_number: 1,
                content: 'JWT_SECRET',
                context_before: [],
                context_after: [],
              },
            ],
            total_matches: 1,
            truncated: false,
          })
        : Promise.resolve({ matches: [], total_matches: 0, truncated: false })
    );
    const tool = new GrepTool(daemon, makeProjectDetector(undefined), makeStateManager());

    const res = await tool.grep({ pattern: 'JWT_SECRET', scope: 'project', projectId: 'project-a' });

    expect(textSearch.mock.calls.some((c) => c[0].tenant_id === undefined)).toBe(false);
    expect(res.matches).toHaveLength(1);
    expect(res.message ?? '').not.toMatch(/scope:"all"/i);
  });
});
