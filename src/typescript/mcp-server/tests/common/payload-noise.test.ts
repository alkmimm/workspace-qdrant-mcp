/**
 * Tests for stripServedNoise — the shared payload trimmer used by the `search`
 * shaping path and the `retrieve` metadata extractor.
 *
 * Field feedback (v0-bws-training audit, 2026-08): a 9-line code hit shipped 28
 * metadata fields whose serialization exceeded the code itself, and
 * `responseFormat:"concise"` did not help because it only ever governed the
 * body cap. These tests pin the three categories it now removes and, just as
 * importantly, pin what it must NOT remove.
 */

import { describe, it, expect } from 'vitest';

import {
  INTERNAL_PLUMBING_KEYS,
  RANKING_AID_KEYS,
  REDUNDANT_METADATA_PAIRS,
  stripServedNoise,
} from '../../src/common/payload-noise.js';

/** The metadata bag observed on a real projects-collection code hit. */
function realCodeHitMetadata(): Record<string, unknown> {
  return {
    chunk_chunk_type: 'function',
    file_hash: 'e1775fe041aede12427236a4bbd0927930c604348c5f8ec82d1c6cd9042b5ae8',
    chunk_signature: 'function collapseBranchSet(',
    item_type: 'file',
    file_extension: 'ts',
    file_type: 'code',
    tags: ['code', 'typescript', 'ts'],
    chunk_symbol_kind: 'function',
    tenant_id: '367157a01d98',
    chunk_symbol_name: 'collapseBranchSet',
    chunk_encoding: 'utf-8',
    chunk_source_format: 'code',
    chunk_language: 'typescript',
    chunk_line_count: '179',
    language: 'typescript',
    document_id: 'f7c7f147-5375-5b99-82bc-4b1066710df5',
    relative_path: 'src/typescript/mcp-server/src/tools/branch-scope.ts',
    base_point: '8686aa3465fafeb71068229bc818d94e',
    chunk_calls: 'includes,join',
    idf_epoch: 39174,
    chunk_index: 9,
    chunk_collection: 'projects',
    document_type: 'code',
    chunk_end_line: '127',
    absolute_path: '/home/u/repo/src/typescript/mcp-server/src/tools/branch-scope.ts',
    file_path: '/home/u/repo/src/typescript/mcp-server/src/tools/branch-scope.ts',
    chunk_start_line: '119',
    _search_type: 'semantic',
  };
}

describe('stripServedNoise', () => {
  it('drops the daemon ranking-aid fields', () => {
    const out = stripServedNoise({ file_path: 'a.ts', ...Object.fromEntries(RANKING_AID_KEYS.map((k) => [k, ['x']])) });
    for (const key of RANKING_AID_KEYS) expect(out).not.toHaveProperty(key);
    expect(out).toEqual({ file_path: 'a.ts' });
  });

  it('drops ingest plumbing unconditionally', () => {
    const payload = Object.fromEntries(INTERNAL_PLUMBING_KEYS.map((k) => [k, 'v']));
    expect(stripServedNoise({ ...payload, chunk_symbol_name: 'foo' })).toEqual({
      chunk_symbol_name: 'foo',
    });
  });

  it('drops a redundant field only when byte-identical to the one it duplicates', () => {
    for (const [drop, keep] of REDUNDANT_METADATA_PAIRS) {
      const same = stripServedNoise({ [drop]: 'x', [keep]: 'x' });
      expect(same, `${drop} duplicates ${keep}`).toEqual({ [keep]: 'x' });

      // Divergence is signal, not noise: keep both rather than hide it.
      const differs = stripServedNoise({ [drop]: 'x', [keep]: 'y' });
      expect(differs, `${drop} differs from ${keep}`).toEqual({ [drop]: 'x', [keep]: 'y' });
    }
  });

  it('keeps a redundant field when the one it duplicates is absent', () => {
    // Nothing supersedes it here, so dropping it would lose the only copy.
    expect(stripServedNoise({ absolute_path: '/repo/a.ts' })).toEqual({
      absolute_path: '/repo/a.ts',
    });
  });

  it('drops file_extension only when a path field already shows the suffix', () => {
    expect(stripServedNoise({ file_extension: 'ts', relative_path: 'src/a.ts' })).toEqual({
      relative_path: 'src/a.ts',
    });
    // Mismatch (or an extensionless path) keeps the explicit value.
    expect(stripServedNoise({ file_extension: 'tsx', relative_path: 'src/a.ts' })).toEqual({
      file_extension: 'tsx',
      relative_path: 'src/a.ts',
    });
    expect(stripServedNoise({ file_extension: 'ts', relative_path: 'Makefile' })).toEqual({
      file_extension: 'ts',
      relative_path: 'Makefile',
    });
  });

  it('honours caller-supplied extra keys', () => {
    expect(stripServedNoise({ content: 'body', file_path: 'a.ts' }, ['content'])).toEqual({
      file_path: 'a.ts',
    });
  });

  it('preserves unrecognized fields (scratchpad/rules/library payloads)', () => {
    // Denylist, not allowlist: the four collections carry different shapes and
    // an allowlist tuned on code chunks would swallow scratchpad provenance.
    const note = {
      created_at: '2026-08-18T00:00:00Z',
      origin_branch: 'main',
      origin_worktree: false,
      title: 'a note',
      some_future_field: 42,
    };
    expect(stripServedNoise({ ...note })).toEqual(note);
  });

  it('cuts a real code hit from 28 fields to the ones an agent reads', () => {
    const out = stripServedNoise(realCodeHitMetadata());

    // The reported offenders are gone.
    for (const key of [
      'file_hash',
      'base_point',
      'idf_epoch',
      'chunk_encoding',
      'absolute_path',
      'tenant_id',
      'chunk_collection',
    ]) {
      expect(out, key).not.toHaveProperty(key);
    }
    // What survives is what locates and describes the chunk.
    expect(out).toEqual({
      chunk_chunk_type: 'function',
      chunk_signature: 'function collapseBranchSet(',
      file_type: 'code',
      tags: ['code', 'typescript', 'ts'],
      chunk_symbol_name: 'collapseBranchSet',
      language: 'typescript',
      document_id: 'f7c7f147-5375-5b99-82bc-4b1066710df5',
      relative_path: 'src/typescript/mcp-server/src/tools/branch-scope.ts',
      chunk_calls: 'includes,join',
      chunk_end_line: '127',
      file_path: '/home/u/repo/src/typescript/mcp-server/src/tools/branch-scope.ts',
      chunk_start_line: '119',
      _search_type: 'semantic',
    });
    expect(Object.keys(out)).toHaveLength(13);
    expect(JSON.stringify(out).length).toBeLessThan(
      JSON.stringify(realCodeHitMetadata()).length / 2
    );
  });
});
