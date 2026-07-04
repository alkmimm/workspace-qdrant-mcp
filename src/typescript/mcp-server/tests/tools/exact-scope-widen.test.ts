/**
 * Empty-result recovery for exact (FTS5) search: when a project-scoped exact
 * search finds nothing, re-run across ALL projects (the literal usually lives
 * in another repo — measured ~90% of empty exact events were cross-project),
 * and on a genuine total miss return an actionable recovery hint instead of a
 * bare empty result (which agents tend to retry verbatim).
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

/** textSearch that is empty for any tenant-scoped call and only returns hits on
 *  the cross-tenant (tenant_id undefined) widen call. Robust to intervening
 *  branch-widen calls, which keep the tenant_id. */
function makeSeqDaemon(crossProjectMatch: Record<string, unknown> | null): DaemonClient {
  return {
    textSearch: vi.fn().mockImplementation((req: { tenant_id?: string }) => {
      if (req.tenant_id === undefined && crossProjectMatch) {
        return Promise.resolve({ matches: [crossProjectMatch], total_matches: 1, truncated: false });
      }
      return Promise.resolve({ matches: [], total_matches: 0, truncated: false });
    }),
  } as unknown as DaemonClient;
}

describe('searchExact — empty-result scope widening', () => {
  it('widens to ALL projects when the project-scoped exact search is empty', async () => {
    const daemon = makeSeqDaemon({
      file_path: 'bws-engineer/frontend/transforms-builder.component.ts',
      line_number: 55,
      content: 'export class TransformsBuilderComponent {',
      tenant_id: 'other-project',
    });

    const response = await searchExact(
      makeQdrant(),
      daemon,
      makeStateManager(),
      makeProjectDetector(undefined),
      makeOptions({ projectId: 'project-a' })
    );

    // The cross-project widen ran with NO tenant filter…
    const calls = (daemon.textSearch as ReturnType<typeof vi.fn>).mock.calls;
    expect(calls.some((c) => c[0].tenant_id === undefined)).toBe(true);
    // …and surfaced the match that the project-scoped search missed.
    expect(response.results).toHaveLength(1);
    expect(response.hint).toBeDefined();
    expect(response.hint).toMatch(/all projects/i);
    expect(response.hint).toMatch(/scope/i);
    // The response advertises the widened scope, not the caller's 'project', so
    // the structured scope agrees with the cross-project results.
    expect(response.scope).toBe('all');
  });

  it('does NOT widen when the project-scoped exact search already has results', async () => {
    // In-project (tenant-scoped) call returns a hit; any widen would be a
    // cross-tenant call (tenant_id undefined), which must NOT happen here.
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
    // No cross-project widen fired…
    expect(calls.some((c) => c[0].tenant_id === undefined)).toBe(false);
    // …the good in-project result is returned untouched, scope stays 'project',
    // and no widen hint is attached.
    expect(response.results).toHaveLength(1);
    expect(response.scope).toBe('project');
    expect(response.hint ?? '').not.toMatch(/all projects/i);
  });

  it('returns an actionable recovery hint when nothing matches anywhere', async () => {
    const daemon = makeSeqDaemon(null); // empty for every call, including the widen

    const response = await searchExact(
      makeQdrant(),
      daemon,
      makeStateManager(),
      makeProjectDetector(undefined),
      makeOptions({ projectId: 'project-a' })
    );

    expect(response.results).toHaveLength(0);
    expect(response.hint).toBeDefined();
    expect(response.hint).toMatch(/search tool|semantic|broaden/i);
  });
});
