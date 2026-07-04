/**
 * Tenant-isolation contract for exact (FTS5) search.
 *
 * A project-scoped exact search (scope:"project", the default) that finds
 * nothing must NOT silently fall back to OTHER repositories — that would cross
 * the tenant data-isolation boundary without the caller opting in (a
 * confidential repo indexed in the same instance leaking into another project's
 * session). It returns 0 hits plus a hint offering scope:"all". Passing
 * scope:"all" explicitly still searches across every repository on demand.
 */

import { describe, it, expect, vi } from 'vitest';
import { searchExact } from '../../src/tools/search-exact.js';
import type { DaemonClient } from '../../src/clients/daemon-client.js';
import type { SqliteStateManager } from '../../src/clients/sqlite-state-manager.js';
import type { ProjectDetector } from '../../src/utils/project-detector.js';
import type { SearchOptions } from '../../src/tools/search-types.js';

function makeStateManager(): SqliteStateManager {
  return {
    logSearchEvent: vi.fn(),
    updateSearchEvent: vi.fn(),
    updateSearchEventEconomy: vi.fn(),
    getProjectById: vi.fn().mockReturnValue({ data: null }),
    getWatchFolderIdByTenantId: vi.fn().mockReturnValue(null),
    getBaseBranch: vi.fn().mockReturnValue(null),
  } as unknown as SqliteStateManager;
}

function makeProjectDetector(projectId: string | undefined): ProjectDetector {
  return {
    findProjectRoot: vi.fn().mockReturnValue('/some/path'),
    getProjectInfo: vi.fn().mockResolvedValue(projectId ? { projectId } : null),
  } as unknown as ProjectDetector;
}

function makeQdrant(): Parameters<typeof searchExact>[0] {
  return { scroll: vi.fn().mockResolvedValue({ points: [] }) } as unknown as Parameters<
    typeof searchExact
  >[0];
}

function makeOptions(overrides: Partial<SearchOptions> = {}): SearchOptions {
  return { query: 'TransformsBuilderComponent', scope: 'project', ...overrides };
}

/** textSearch that returns a hit ONLY on a cross-tenant (tenant_id === undefined)
 *  call — i.e. the literal lives exclusively in another repo. Any tenant-scoped
 *  call (the current project, and the same-tenant branch-widen) is empty. */
function makeCrossProjectOnlyDaemon(match: Record<string, unknown> | null): DaemonClient {
  return {
    textSearch: vi.fn().mockImplementation((req: { tenant_id?: string }) => {
      if (req.tenant_id === undefined && match) {
        return Promise.resolve({ matches: [match], total_matches: 1, truncated: false });
      }
      return Promise.resolve({ matches: [], total_matches: 0, truncated: false });
    }),
  } as unknown as DaemonClient;
}

const OTHER_REPO_MATCH = {
  file_path: 'compress-mcp/tests/test_redaction.py',
  line_number: 55,
  content: 'JWT_SECRET = "not-a-real-secret"',
  tenant_id: 'other-project',
};

describe('searchExact — tenant isolation (no silent cross-project fallback)', () => {
  it('REGRESSION: a project-scoped miss does NOT return hits from another repo', async () => {
    // The literal exists only in repo B; cwd is project A, scope defaults to project.
    const daemon = makeCrossProjectOnlyDaemon(OTHER_REPO_MATCH);

    const response = await searchExact(
      makeQdrant(),
      daemon,
      makeStateManager(),
      makeProjectDetector(undefined),
      makeOptions({ projectId: 'project-a' })
    );

    const calls = (daemon.textSearch as ReturnType<typeof vi.fn>).mock.calls;
    // No cross-tenant query was ever issued — the boundary was never crossed.
    expect(calls.some((c) => c[0].tenant_id === undefined)).toBe(false);
    // 0 hits, scope stays 'project', and the hint offers the explicit opt-in.
    expect(response.results).toHaveLength(0);
    expect(response.scope).toBe('project');
    expect(response.hint).toBeDefined();
    expect(response.hint).toMatch(/scope:"all"/i);
  });

  it('passing scope:"all" explicitly still searches across every repository', async () => {
    const daemon = makeCrossProjectOnlyDaemon(OTHER_REPO_MATCH);

    const response = await searchExact(
      makeQdrant(),
      daemon,
      makeStateManager(),
      makeProjectDetector(undefined),
      makeOptions({ scope: 'all', projectId: 'project-a' })
    );

    // scope:"all" resolves to an unscoped (tenant_id undefined) primary query,
    // so the cross-repo hit is returned — on demand, because the caller asked.
    expect(response.results).toHaveLength(1);
    expect(response.scope).toBe('all');
  });

  it('does NOT widen when the project-scoped search already has results', async () => {
    const daemon = {
      textSearch: vi.fn().mockImplementation((req: { tenant_id?: string }) => {
        if (req.tenant_id !== undefined) {
          return Promise.resolve({
            matches: [
              {
                file_path: 'project-a/src/index.ts',
                line_number: 1,
                content: 'export class TransformsBuilderComponent {',
                tenant_id: 'project-a',
              },
            ],
            total_matches: 1,
            truncated: false,
          });
        }
        return Promise.resolve({ matches: [], total_matches: 0, truncated: false });
      }),
    } as unknown as DaemonClient;

    const response = await searchExact(
      makeQdrant(),
      daemon,
      makeStateManager(),
      makeProjectDetector(undefined),
      makeOptions({ projectId: 'project-a' })
    );

    const calls = (daemon.textSearch as ReturnType<typeof vi.fn>).mock.calls;
    expect(calls.some((c) => c[0].tenant_id === undefined)).toBe(false);
    expect(response.results).toHaveLength(1);
    expect(response.scope).toBe('project');
    expect(response.hint ?? '').not.toMatch(/all projects|scope:"all"/i);
  });

  it('returns an actionable opt-in hint on a total project miss', async () => {
    const daemon = makeCrossProjectOnlyDaemon(null); // empty everywhere

    const response = await searchExact(
      makeQdrant(),
      daemon,
      makeStateManager(),
      makeProjectDetector(undefined),
      makeOptions({ projectId: 'project-a' })
    );

    expect(response.results).toHaveLength(0);
    expect(response.hint).toBeDefined();
    // Hint names the explicit opt-in.
    expect(response.hint).toMatch(/scope:"all"/i);
  });
});
