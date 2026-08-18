/**
 * Tests for extractMetadata — the payload→metadata projection used by the
 * `retrieve` tool. It must drop the chunk body, raw vectors, and the daemon's
 * ranking-aid fields (keywords/baskets/tags) while preserving everything an
 * agent actually consumes.
 */

import { describe, it, expect } from 'vitest';

import { extractMetadata } from '../../src/tools/retrieve-types.js';

describe('extractMetadata', () => {
  it('returns an empty object for null/undefined payloads', () => {
    expect(extractMetadata(null)).toEqual({});
    expect(extractMetadata(undefined)).toEqual({});
  });

  it('drops content and raw vector fields', () => {
    const md = extractMetadata({
      content: 'the chunk body',
      dense_vector: [0.1, 0.2],
      sparse_vector: { indices: [1], values: [0.5] },
      file_path: 'src/foo.ts',
    });
    expect(md).not.toHaveProperty('content');
    expect(md).not.toHaveProperty('dense_vector');
    expect(md).not.toHaveProperty('sparse_vector');
    expect(md).toHaveProperty('file_path', 'src/foo.ts');
  });

  it('drops the daemon ranking-aid fields (keywords/baskets/tags)', () => {
    // keyword_extract.rs injects these on every code chunk; they are indexing
    // signal (~1.5–2k tokens/hit) the agent never reads. The retrieve path must
    // strip the same set the search truncate path does.
    const md = extractMetadata({
      file_path: 'src/foo.ts',
      chunk_symbol_name: 'fooFn',
      keywords: Array.from({ length: 50 }, (_, i) => `kw${i}`),
      keyword_baskets: { tagA: ['kw1'], tagB: ['kw2'] },
      concept_tags: ['c1', 'c2'],
      structural_tags: { fn: ['x'] },
    });
    expect(md).not.toHaveProperty('keywords');
    expect(md).not.toHaveProperty('keyword_baskets');
    expect(md).not.toHaveProperty('concept_tags');
    expect(md).not.toHaveProperty('structural_tags');
    // Discovery-relevant metadata survives.
    expect(md).toEqual({ file_path: 'src/foo.ts', chunk_symbol_name: 'fooFn' });
  });

  it('drops ingest plumbing and provably-redundant duplicates', () => {
    // Shared with the search shaping path (CLAUDE.md shared-behavior rule):
    // retrieve used to ship the same file_hash / base_point / idf_epoch /
    // absolute_path plumbing that search had already been trimming.
    const md = extractMetadata({
      file_path: '/repo/src/foo.ts',
      absolute_path: '/repo/src/foo.ts',
      relative_path: 'src/foo.ts',
      file_hash: 'e1775fe041aede12427236a4bbd0927930c604348c5f8ec82d1c6cd9042b5ae8',
      base_point: '8686aa3465fafeb71068229bc818d94e',
      idf_epoch: 39174,
      tenant_id: '367157a01d98',
      item_type: 'file',
      chunk_index: 9,
      chunk_symbol_name: 'fooFn',
    });
    expect(md).toEqual({
      file_path: '/repo/src/foo.ts',
      relative_path: 'src/foo.ts',
      chunk_symbol_name: 'fooFn',
    });
  });

  it('preserves scratchpad provenance and timestamps', () => {
    // The trimmer is a denylist precisely so non-code collections keep their
    // own shape — an allowlist tuned on code chunks would swallow these.
    const note = {
      created_at: '2026-08-18T00:00:00Z',
      updated_at: '2026-08-18T00:00:00Z',
      origin_branch: 'main',
      origin_cwd: '/repo',
      origin_worktree: false,
      title: 'a note',
      tags: ['triage'],
    };
    expect(extractMetadata({ ...note })).toEqual(note);
  });
});
