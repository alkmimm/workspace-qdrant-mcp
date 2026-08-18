/**
 * The `search` response carries ONE hint slot. These pin which message wins.
 *
 * Priority is by SPECIFICITY: the two situational hints each mark a call the
 * caller would otherwise waste, so they outrank the generic graph tip. Keeping
 * it to one message also keeps the token cost of a hint flat instead of
 * additive — the graph tip fires on nearly every code search.
 */

import { describe, it, expect, afterEach } from 'vitest';

import { shapeHitPayloads } from '../../src/tools/search-shaping.js';
import type { SearchOptions, SearchResponse, SearchResult } from '../../src/tools/search-types.js';

function result(metadata: Record<string, unknown>): SearchResult {
  return { id: 'p1', score: 0.9, collection: 'projects', content: 'body', metadata };
}

function response(results: SearchResult[]): SearchResponse {
  return {
    results,
    total: results.length,
    query: 'q',
    mode: 'semantic',
    scope: 'project',
    collections_searched: ['projects'],
    status: 'ok',
  };
}

const opts: SearchOptions = { query: 'q' };

const TAIL_FRAGMENT = {
  chunk_is_fragment: 'true',
  chunk_fragment_index: '11',
  chunk_total_fragments: '12',
  chunk_symbol_name: 'Capability',
  relative_path: 'doc-backend/.../Capability.java',
};

afterEach(() => {
  delete process.env['WQM_SEARCH_DERANK'];
});

describe('search hint priority', () => {
  it('a tail-fragment hit wins over the generic graph tip', () => {
    // Both apply (the fragment hit is also a named symbol); the specific one
    // must win, since it names a call the caller is about to waste.
    const { response: shaped } = shapeHitPayloads(response([result({ ...TAIL_FRAGMENT })]), opts);
    expect(shaped.hint).toContain('fragment 11 of 12');
    expect(shaped.hint).not.toContain('graph(action=');
  });

  it('de-ranked hits are named when nothing more specific applies', () => {
    process.env['WQM_SEARCH_DERANK'] = 'docs/archive/,old_project/';
    const { response: shaped } = shapeHitPayloads(
      response([
        result({ relative_path: 'docs/archive/plans/evidence/tmp/legacy-v11.txt' }),
        result({ relative_path: 'docs/archive/plans/evidence/tmp/legacy-v12.txt' }),
        result({ relative_path: 'docs/plans/testing-gap-plan.md' }),
      ]),
      opts
    );
    expect(shaped.hint).toContain('2 of 3 hits');
    expect(shaped.hint).toContain('docs/archive/');
    // Only the substring that actually matched is named.
    expect(shaped.hint).not.toContain('old_project/');
    expect(shaped.hint).toContain('pathExclude');
  });

  it('the fragment hint still outranks the de-rank note', () => {
    process.env['WQM_SEARCH_DERANK'] = 'docs/archive/';
    const { response: shaped } = shapeHitPayloads(
      response([
        result({ ...TAIL_FRAGMENT }),
        result({ relative_path: 'docs/archive/legacy.txt' }),
      ]),
      opts
    );
    expect(shaped.hint).toContain('fragment 11 of 12');
  });

  it('falls back to the graph tip for an ordinary symbol hit', () => {
    const { response: shaped } = shapeHitPayloads(
      response([result({ chunk_symbol_name: 'collapseBranchSet', relative_path: 'a.ts' })]),
      opts
    );
    expect(shaped.hint).toContain('graph(action=');
  });

  it('emits no hint at all when nothing applies', () => {
    const { response: shaped } = shapeHitPayloads(
      response([result({ relative_path: 'notes.md' })]),
      opts
    );
    expect(shaped.hint).toBeUndefined();
  });

  it('says nothing about de-ranking when the deployment configures none', () => {
    // No WQM_SEARCH_DERANK -> the note must not fire on arbitrary paths.
    const { response: shaped } = shapeHitPayloads(
      response([result({ relative_path: 'docs/archive/legacy.txt' })]),
      opts
    );
    expect(shaped.hint).toBeUndefined();
  });

  it('summary mode keeps the fragment fields so discovery is not blind', () => {
    const { response: shaped } = shapeHitPayloads(response([result({ ...TAIL_FRAGMENT })]), {
      ...opts,
      summary: true,
    });
    expect(shaped.results[0].metadata).toMatchObject({
      chunk_is_fragment: 'true',
      chunk_fragment_index: '11',
      chunk_total_fragments: '12',
    });
  });
});
