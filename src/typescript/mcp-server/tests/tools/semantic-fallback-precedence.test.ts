/**
 * The vector lane, end to end through `finalizeResults`: on a feature branch the
 * caller's own generation of a file must win over the base branch's older one —
 * even when the older one scores higher.
 *
 * The Qdrant filter widens with a plain `should: [branch, fallback]`, so both
 * generations of a file changed on the feature branch come back. The per-file
 * collapse downstream keeps the best-RANKED chunk, so a stale generation that
 * happened to score higher replaced the current one silently — no duplicate to
 * notice. `dropFallbackDuplicatesByPath` runs before fusion to prevent that; the
 * helper has unit tests, and this pins that it is actually wired in.
 */

import { describe, it, expect, vi } from 'vitest';
import { finalizeResults } from '../../src/tools/search-helpers.js';
import type { SearchResult, SearchOptions } from '../../src/tools/search-types.js';
import type { QdrantClient } from '@qdrant/js-client-rest';
import type { DaemonClient } from '../../src/clients/daemon-client.js';
import type { SqliteStateManager } from '../../src/clients/sqlite-state-manager.js';

function hit(id: string, score: number, relativePath: string, branch: unknown): SearchResult {
  return {
    id,
    score,
    collection: 'projects',
    content: `body of ${id}`,
    metadata: { relative_path: relativePath, branch, _search_type: 'semantic' },
  };
}

async function finalize(allResults: SearchResult[], options: SearchOptions) {
  return finalizeResults(
    {} as unknown as QdrantClient,
    {} as unknown as DaemonClient,
    { updateSearchEvent: vi.fn() } as unknown as SqliteStateManager,
    {
      allResults,
      mode: 'semantic',
      limit: 10,
      options,
      eventId: 'evt',
      searchStartMs: Date.now(),
      query: options.query,
      scope: 'global', // skips the project-only indexing probe
      collectionsToSearch: ['projects'],
      status: 'ok',
      statusReason: undefined,
      currentProjectId: undefined,
    }
  );
}

describe('vector lane — the caller branch owns any path it carries', () => {
  it('serves the feature-branch generation even when the base copy scores higher', async () => {
    const response = await finalize(
      [
        hit('stale', 0.9, 'src/a.rs', 'main'),
        hit('current', 0.6, 'src/a.rs', 'feature/x'),
        hit('unchanged', 0.5, 'src/b.rs', 'main'),
      ],
      { query: 'something', branch: 'feature/x', fallbackBranch: 'main' }
    );

    const ids = response.results.map((r) => r.id);
    expect(ids).toContain('current');
    expect(ids).not.toContain('stale');
    // A file only the base branch has is exactly what the fallback is for.
    expect(ids).toContain('unchanged');
  });

  it('keeps a generation identical on both branches', async () => {
    const response = await finalize(
      [hit('shared', 0.7, 'src/a.rs', ['feature/x', 'main'])],
      { query: 'something', branch: 'feature/x', fallbackBranch: 'main' }
    );

    expect(response.results.map((r) => r.id)).toEqual(['shared']);
  });

  it('changes nothing on the trunk, where no fallback is in play', async () => {
    const response = await finalize(
      [hit('a', 0.7, 'src/a.rs', 'main'), hit('b', 0.6, 'src/b.rs', 'main')],
      { query: 'something', branch: 'main' }
    );

    expect(response.results.map((r) => r.id).sort()).toEqual(['a', 'b']);
  });
});
