/**
 * Tests for per-hit payload shaping applied at the outer boundary of the
 * `search` tool. Without this cap, broad queries return responses that
 * exceed MCP client per-tool-result token budgets and trigger disk
 * offload — see issue #N (search payload cap).
 *
 * Also covers token-economy metrics (`ShapingMetrics`) emitted alongside
 * the shaped response. Spec: docs/specs/20-token-economy-instrumentation.md
 */

import { describe, it, expect } from 'vitest';
import { shapeHitPayloads, dedupeIdenticalBodies } from '../../src/tools/search-shaping.js';
import {
  DEFAULT_MAX_BYTES_PER_HIT,
  type SearchOptions,
  type SearchResponse,
  type SearchResult,
} from '../../src/tools/search-types.js';

function makeResult(overrides: Partial<SearchResult> = {}): SearchResult {
  return {
    id: 'doc-1',
    score: 0.9,
    collection: 'projects',
    content: 'short body',
    metadata: { file_path: 'src/foo.ts' },
    ...overrides,
  };
}

function makeResponse(results: SearchResult[]): SearchResponse {
  return {
    results,
    total: results.length,
    query: 'q',
    mode: 'hybrid',
    scope: 'project',
    collections_searched: ['projects'],
    status: 'ok',
  };
}

function baseOptions(extra: Partial<SearchOptions> = {}): SearchOptions {
  return { query: 'q', ...extra };
}

describe('shapeHitPayloads', () => {
  describe('default truncation (maxBytesPerHit unset → 1500)', () => {
    it('passes through hits whose content is at or below the cap untouched', () => {
      const r = makeResult({ content: 'a'.repeat(100) });
      const { response: shaped } = shapeHitPayloads(makeResponse([r]), baseOptions());
      expect(shaped.results[0].content).toBe('a'.repeat(100));
    });

    it('truncates content longer than the cap with a marker pointing to retrieve()', () => {
      const longText = 'x'.repeat(5000);
      const r = makeResult({ id: 'doc-42', collection: 'projects', content: longText });
      const { response: shaped } = shapeHitPayloads(makeResponse([r]), baseOptions());
      const shapedContent = shaped.results[0].content;
      expect(shapedContent.length).toBeLessThanOrEqual(DEFAULT_MAX_BYTES_PER_HIT);
      expect(shapedContent).toContain('[truncated');
      // The marker must include the docId and collection so the agent
      // can call retrieve() without re-searching.
      expect(shapedContent).toContain('doc-42');
      expect(shapedContent).toContain('projects');
      expect(shapedContent).toContain('retrieve');
    });

    it('points exact-search hits at filePath + lineNumber instead of documentId', () => {
      const longText = 'x'.repeat(5000);
      const r = makeResult({
        id: 'src/foo.ts:42',
        collection: 'projects',
        content: longText,
        metadata: { file_path: 'src/foo.ts', line_number: 42 },
      });
      const { response: shaped } = shapeHitPayloads(makeResponse([r]), baseOptions());
      const shapedContent = shaped.results[0].content;
      expect(shapedContent).toContain(
        'retrieve(filePath="src/foo.ts", lineNumber=42, collection="projects")'
      );
      expect(shapedContent).not.toContain('retrieve(documentId=');
    });

    it('keeps the 10-hit response well under 25k chars on broad results', () => {
      // Simulate a worst-case broad search: 10 hits of ~10kB chunk text.
      const hits = Array.from({ length: 10 }, (_, i) =>
        makeResult({ id: `doc-${i}`, content: 'a'.repeat(10_000) })
      );
      const { response: shaped } = shapeHitPayloads(makeResponse(hits), baseOptions());
      const totalChars = shaped.results.reduce((acc, r) => acc + r.content.length, 0);
      // 10 hits × 1500 chars = 15k upper bound — comfortably below the
      // 25k informal budget for an MCP tool result.
      expect(totalChars).toBeLessThanOrEqual(10 * DEFAULT_MAX_BYTES_PER_HIT);
    });

    it('also truncates parent_context.unit_text when present', () => {
      const r = makeResult({
        id: 'doc-99',
        content: 'short',
        parent_context: {
          parent_unit_id: 'parent-1',
          unit_type: 'function',
          unit_text: 'y'.repeat(5000),
        },
      });
      const { response: shaped } = shapeHitPayloads(makeResponse([r]), baseOptions());
      const parentText = shaped.results[0].parent_context?.unit_text ?? '';
      expect(parentText.length).toBeLessThanOrEqual(DEFAULT_MAX_BYTES_PER_HIT);
      expect(parentText).toContain('[truncated');
    });

    it('strips duplicated content from metadata so the body is not shipped twice', () => {
      // The Qdrant payload carries `content` both as result.content and
      // duplicated in metadata. Without dedup, every hit would ship the
      // text twice — defeating the cap.
      const longText = 'z'.repeat(3000);
      const r = makeResult({
        content: longText,
        metadata: { file_path: 'a.ts', content: longText, chunk_text: longText },
      });
      const { response: shaped } = shapeHitPayloads(makeResponse([r]), baseOptions());
      expect(shaped.results[0].metadata).not.toHaveProperty('content');
      expect(shaped.results[0].metadata).not.toHaveProperty('chunk_text');
      // Non-text metadata must be preserved.
      expect(shaped.results[0].metadata).toHaveProperty('file_path', 'a.ts');
    });

    it('strips the daemon ranking-aid fields (keywords/baskets/tags) from metadata', () => {
      // keyword_extract.rs injects these onto every code chunk; they are
      // indexing signal (~1.5–2k tokens/hit) the agent never reads. Default
      // (truncate) mode must drop them, just as summary mode already does.
      const r = makeResult({
        content: 'short body',
        metadata: {
          file_path: 'a.ts',
          keywords: Array.from({ length: 50 }, (_, i) => `kw${i}`),
          keyword_baskets: { tagA: ['kw1', 'kw2'], tagB: ['kw3'] },
          concept_tags: ['c1', 'c2'],
          structural_tags: { fn: ['x'] },
        },
      });
      const { response: shaped } = shapeHitPayloads(makeResponse([r]), baseOptions());
      expect(shaped.results[0].metadata).not.toHaveProperty('keywords');
      expect(shaped.results[0].metadata).not.toHaveProperty('keyword_baskets');
      expect(shaped.results[0].metadata).not.toHaveProperty('concept_tags');
      expect(shaped.results[0].metadata).not.toHaveProperty('structural_tags');
      // Discovery-relevant metadata must survive.
      expect(shaped.results[0].metadata).toHaveProperty('file_path', 'a.ts');
    });
  });

  describe('custom maxBytesPerHit', () => {
    it('respects a custom cap', () => {
      const r = makeResult({ content: 'q'.repeat(1000) });
      const { response: shaped } = shapeHitPayloads(
        makeResponse([r]),
        baseOptions({ maxBytesPerHit: 200 })
      );
      expect(shaped.results[0].content.length).toBeLessThanOrEqual(200);
      expect(shaped.results[0].content).toContain('[truncated');
    });

    it('disables truncation when maxBytesPerHit is 0', () => {
      const longText = 'k'.repeat(50_000);
      const r = makeResult({ content: longText });
      const { response: shaped } = shapeHitPayloads(
        makeResponse([r]),
        baseOptions({ maxBytesPerHit: 0 })
      );
      expect(shaped.results[0].content).toBe(longText);
    });

    it('disables truncation when maxBytesPerHit is negative', () => {
      const r = makeResult({ content: 'p'.repeat(10_000) });
      const { response: shaped } = shapeHitPayloads(
        makeResponse([r]),
        baseOptions({ maxBytesPerHit: -1 })
      );
      expect(shaped.results[0].content.length).toBe(10_000);
    });
  });

  describe('responseFormat + global byte budget (P1.5)', () => {
    it('detailed returns full bodies (per-hit cap disabled)', () => {
      const long = 'z'.repeat(5000);
      const { response: shaped } = shapeHitPayloads(
        makeResponse([makeResult({ content: long })]),
        baseOptions({ responseFormat: 'detailed' })
      );
      expect(shaped.results[0].content).toBe(long);
    });

    it('concise truncates like the default', () => {
      const { response: shaped } = shapeHitPayloads(
        makeResponse([makeResult({ content: 'y'.repeat(5000) })]),
        baseOptions({ responseFormat: 'concise' })
      );
      expect(shaped.results[0].content.length).toBeLessThanOrEqual(DEFAULT_MAX_BYTES_PER_HIT);
      expect(shaped.results[0].content).toContain('[truncated');
    });

    it('explicit maxBytesPerHit overrides responseFormat=detailed', () => {
      const { response: shaped } = shapeHitPayloads(
        makeResponse([makeResult({ content: 'y'.repeat(5000) })]),
        baseOptions({ responseFormat: 'detailed', maxBytesPerHit: 200 })
      );
      expect(shaped.results[0].content.length).toBeLessThanOrEqual(200);
    });

    it('drops trailing hits past the response budget and reports budget_truncated', () => {
      const hits = Array.from({ length: 5 }, (_, i) =>
        makeResult({ id: `doc-${i}`, content: 'a'.repeat(1000) })
      );
      const { response: shaped } = shapeHitPayloads(
        makeResponse(hits),
        baseOptions({ responseFormat: 'detailed', maxResponseBytes: 2500 })
      );
      expect(shaped.results.length).toBeGreaterThanOrEqual(1);
      expect(shaped.results.length).toBeLessThan(5);
      expect(shaped.budget_truncated?.dropped).toBe(5 - shaped.results.length);
    });

    it('always keeps at least one hit even if it alone exceeds the budget', () => {
      const { response: shaped } = shapeHitPayloads(
        makeResponse([makeResult({ content: 'a'.repeat(10_000) })]),
        baseOptions({ responseFormat: 'detailed', maxResponseBytes: 100 })
      );
      expect(shaped.results.length).toBe(1);
      expect(shaped.budget_truncated).toBeUndefined();
    });

    it('maxResponseBytes=0 disables the global budget', () => {
      const hits = Array.from({ length: 5 }, (_, i) =>
        makeResult({ id: `doc-${i}`, content: 'a'.repeat(1000) })
      );
      const { response: shaped } = shapeHitPayloads(
        makeResponse(hits),
        baseOptions({ responseFormat: 'detailed', maxResponseBytes: 0 })
      );
      expect(shaped.results.length).toBe(5);
      expect(shaped.budget_truncated).toBeUndefined();
    });

    it('recomputes bytesOutShaped from kept hits after a budget drop', () => {
      const hits = Array.from({ length: 5 }, (_, i) =>
        makeResult({ id: `doc-${i}`, content: 'a'.repeat(1000) })
      );
      const { response: shaped, metrics } = shapeHitPayloads(
        makeResponse(hits),
        baseOptions({ responseFormat: 'detailed', maxResponseBytes: 2500 })
      );
      expect(metrics.bytesOutShaped).toBe(shaped.results.length * 1000);
    });

    it('applies the budget in concise/truncate mode too (not only detailed)', () => {
      const hits = Array.from({ length: 5 }, (_, i) =>
        makeResult({ id: `doc-${i}`, content: 'a'.repeat(1000) })
      );
      const { response: shaped } = shapeHitPayloads(
        makeResponse(hits),
        baseOptions({ responseFormat: 'concise', maxResponseBytes: 2500 })
      );
      expect(shaped.results.length).toBeLessThan(5);
      expect(shaped.budget_truncated?.dropped).toBe(5 - shaped.results.length);
    });
  });

  describe('summary mode', () => {
    it('drops chunk bodies but keeps id, score, collection, title, and structural metadata', () => {
      const r = makeResult({
        id: 'doc-7',
        score: 0.7,
        title: 'Foo function',
        content: 'a'.repeat(5000),
        metadata: {
          // Real daemon payload keys (schema/qdrant/projects.rs): tree-sitter
          // chunk metadata is `chunk_`-prefixed.
          file_path: 'src/foo.ts',
          chunk_start_line: 10,
          chunk_symbol_name: 'fooFn',
          content: 'a'.repeat(5000),
          chunk_text: 'a'.repeat(5000),
        },
      });
      const { response: shaped } = shapeHitPayloads(
        makeResponse([r]),
        baseOptions({ summary: true })
      );
      expect(shaped.results[0].content).toBe('');
      expect(shaped.results[0].id).toBe('doc-7');
      expect(shaped.results[0].score).toBe(0.7);
      expect(shaped.results[0].collection).toBe('projects');
      expect(shaped.results[0].title).toBe('Foo function');
      // Structural metadata must survive.
      expect(shaped.results[0].metadata).toMatchObject({
        file_path: 'src/foo.ts',
        chunk_start_line: 10,
        chunk_symbol_name: 'fooFn',
      });
      // Text fields must be gone.
      expect(shaped.results[0].metadata).not.toHaveProperty('content');
      expect(shaped.results[0].metadata).not.toHaveProperty('chunk_text');
    });

    it('keeps a 10-hit summary response well under 5k chars', () => {
      const hits = Array.from({ length: 10 }, (_, i) =>
        makeResult({
          id: `doc-${i}`,
          title: `Hit ${i}`,
          content: 'a'.repeat(20_000),
          metadata: {
            file_path: `src/file${i}.ts`,
            chunk_start_line: i,
            content: 'a'.repeat(20_000),
          },
        })
      );
      const { response: shaped } = shapeHitPayloads(
        makeResponse(hits),
        baseOptions({ summary: true })
      );
      const serialized = JSON.stringify(shaped);
      expect(serialized.length).toBeLessThan(5000);
    });

    it('summary takes precedence over maxBytesPerHit', () => {
      const r = makeResult({ content: 'a'.repeat(3000) });
      const { response: shaped } = shapeHitPayloads(
        makeResponse([r]),
        baseOptions({ summary: true, maxBytesPerHit: 5000 })
      );
      expect(shaped.results[0].content).toBe('');
    });
  });

  describe('immutability', () => {
    it('does not mutate the input response or its hits', () => {
      const originalContent = 'r'.repeat(5000);
      const r = makeResult({ content: originalContent });
      const response = makeResponse([r]);
      shapeHitPayloads(response, baseOptions());
      expect(response.results[0].content).toBe(originalContent);
    });
  });

  describe('shaping metrics (token economy)', () => {
    it('reports mode "none" with bytes_in == bytes_out when cap is disabled', () => {
      const r = makeResult({ content: 'a'.repeat(800) });
      const { metrics } = shapeHitPayloads(makeResponse([r]), baseOptions({ maxBytesPerHit: 0 }));
      expect(metrics.mode).toBe('none');
      expect(metrics.bytesInShaped).toBe(800);
      expect(metrics.bytesOutShaped).toBe(800);
      expect(metrics.hitsTruncated).toBe(0);
    });

    it('reports mode "truncate" with bytes_out < bytes_in when content exceeds the cap', () => {
      const r = makeResult({ content: 'x'.repeat(5000) });
      const { metrics } = shapeHitPayloads(makeResponse([r]), baseOptions());
      expect(metrics.mode).toBe('truncate');
      expect(metrics.bytesInShaped).toBe(5000);
      expect(metrics.bytesOutShaped).toBeLessThanOrEqual(DEFAULT_MAX_BYTES_PER_HIT);
      expect(metrics.bytesOutShaped).toBeLessThan(metrics.bytesInShaped);
      expect(metrics.hitsTruncated).toBe(1);
    });

    it('counts only hits that actually got truncated', () => {
      const small = makeResult({ id: 'small', content: 'a'.repeat(100) });
      const big = makeResult({ id: 'big', content: 'a'.repeat(5000) });
      const { metrics } = shapeHitPayloads(makeResponse([small, big]), baseOptions());
      expect(metrics.hitsTruncated).toBe(1);
      expect(metrics.bytesInShaped).toBe(5100);
    });

    it('counts parent_context.unit_text into bytes_in alongside content', () => {
      const r = makeResult({
        content: 'a'.repeat(500),
        parent_context: {
          parent_unit_id: 'p',
          unit_type: 'function',
          unit_text: 'b'.repeat(300),
        },
      });
      const { metrics } = shapeHitPayloads(makeResponse([r]), baseOptions());
      expect(metrics.bytesInShaped).toBe(800);
    });

    it('marks a hit truncated when parent_context exceeds the cap even if content is small', () => {
      const r = makeResult({
        content: 'a'.repeat(50),
        parent_context: {
          parent_unit_id: 'p',
          unit_type: 'function',
          unit_text: 'b'.repeat(5000),
        },
      });
      const { metrics } = shapeHitPayloads(makeResponse([r]), baseOptions());
      expect(metrics.hitsTruncated).toBe(1);
    });

    it('reports mode "summary" with bytes_out == 0 when summary is requested', () => {
      const r = makeResult({ content: 'a'.repeat(4000) });
      const { metrics } = shapeHitPayloads(makeResponse([r]), baseOptions({ summary: true }));
      expect(metrics.mode).toBe('summary');
      expect(metrics.bytesInShaped).toBe(4000);
      expect(metrics.bytesOutShaped).toBe(0);
      expect(metrics.hitsTruncated).toBe(0);
    });

    it('reports zeros for an empty response', () => {
      const { metrics } = shapeHitPayloads(makeResponse([]), baseOptions());
      expect(metrics.mode).toBe('truncate');
      expect(metrics.bytesInShaped).toBe(0);
      expect(metrics.bytesOutShaped).toBe(0);
      expect(metrics.hitsTruncated).toBe(0);
    });
  });
});

describe('grep-like location locator (item 4)', () => {
  it('lifts relative_path:line into a top-level location in truncate mode', () => {
    const r = makeResult({ metadata: { relative_path: 'src/a.ts', line_number: 5 } });
    const { response } = shapeHitPayloads(makeResponse([r]), baseOptions());
    expect(response.results[0].location).toBe('src/a.ts:5');
  });

  it('prefers relative_path over the absolute file_path', () => {
    const r = makeResult({
      metadata: { relative_path: 'src/a.ts', file_path: '/abs/src/a.ts', line_number: 9 },
    });
    const { response } = shapeHitPayloads(makeResponse([r]), baseOptions());
    expect(response.results[0].location).toBe('src/a.ts:9');
  });

  it('falls back to file_path and chunk_start_line when those are all that exist', () => {
    const r = makeResult({ metadata: { file_path: 'src/b.ts', chunk_start_line: 42 } });
    const { response } = shapeHitPayloads(makeResponse([r]), baseOptions());
    expect(response.results[0].location).toBe('src/b.ts:42');
  });

  it('emits a bare path when no line number is known', () => {
    const r = makeResult({ metadata: { relative_path: 'README.md' } });
    const { response } = shapeHitPayloads(makeResponse([r]), baseOptions());
    expect(response.results[0].location).toBe('README.md');
  });

  it('omits location entirely when the hit carries no path', () => {
    const r = makeResult({ metadata: {} });
    const { response } = shapeHitPayloads(makeResponse([r]), baseOptions());
    expect(response.results[0].location).toBeUndefined();
  });

  it('sets location in summary mode', () => {
    const r = makeResult({ metadata: { relative_path: 'src/c.ts', chunk_start_line: 1 } });
    const { response } = shapeHitPayloads(makeResponse([r]), baseOptions({ summary: true }));
    expect(response.results[0].location).toBe('src/c.ts:1');
  });

  it('sets location even when truncation is disabled (cap=0) without altering content', () => {
    const r = makeResult({
      content: 'a'.repeat(50),
      metadata: { file_path: 'src/d.ts', line_number: 7 },
    });
    const { response } = shapeHitPayloads(makeResponse([r]), baseOptions({ maxBytesPerHit: 0 }));
    expect(response.results[0].location).toBe('src/d.ts:7');
    expect(response.results[0].content).toBe('a'.repeat(50));
  });
});

describe('in-band graph hint (item 3)', () => {
  it('attaches a graph hint when at least one hit is a named code symbol', () => {
    const r = makeResult({ metadata: { file_path: 'src/a.ts', chunk_symbol_name: 'fooFn' } });
    const { response } = shapeHitPayloads(makeResponse([r]), baseOptions());
    expect(response.hint).toBeDefined();
    expect(response.hint).toContain('graph');
  });

  it('omits the hint when no result is a code symbol', () => {
    const r = makeResult({ metadata: { file_path: 'README.md' } });
    const { response } = shapeHitPayloads(makeResponse([r]), baseOptions());
    expect(response.hint).toBeUndefined();
  });

  it('attaches the hint in summary mode too (symbol name survives the allowlist)', () => {
    const r = makeResult({ metadata: { file_path: 'src/a.ts', chunk_symbol_name: 'barFn' } });
    const { response } = shapeHitPayloads(makeResponse([r]), baseOptions({ summary: true }));
    expect(response.hint).toBeDefined();
  });

  it('does not mutate the input response when adding a hint', () => {
    const r = makeResult({ metadata: { file_path: 'src/a.ts', chunk_symbol_name: 'bazFn' } });
    const response = makeResponse([r]);
    shapeHitPayloads(response, baseOptions());
    expect(response.hint).toBeUndefined();
  });
});

// ───────────────────── cross-file identical-body dedup ─────────────────────

describe('dedupeIdenticalBodies', () => {
  const bigBody = 'const x = 1;\n'.repeat(20); // > DUPLICATE_BODY_MIN_CHARS

  it('collapses a byte-identical body from another file onto the best-ranked hit', () => {
    const a = makeResult({ id: 'a', content: bigBody, metadata: { file_path: 'src/a.ts' } });
    const b = makeResult({ id: 'b', content: bigBody, metadata: { file_path: 'vendor/a.ts' } });
    const { kept, dropped } = dedupeIdenticalBodies([a, b]);
    expect(kept.map((r) => r.id)).toEqual(['a']);
    expect(dropped).toBe(1);
  });

  it('keeps distinct bodies and short identical snippets', () => {
    const a = makeResult({ id: 'a', content: bigBody });
    const b = makeResult({ id: 'b', content: `${bigBody}// different` });
    const shortDup1 = makeResult({ id: 'c', content: 'return null;' });
    const shortDup2 = makeResult({ id: 'd', content: 'return null;' });
    const { kept, dropped } = dedupeIdenticalBodies([a, b, shortDup1, shortDup2]);
    expect(kept).toHaveLength(4);
    expect(dropped).toBe(0);
  });

  it('compares trimmed bodies so trailing whitespace does not defeat the dedup', () => {
    const a = makeResult({ id: 'a', content: bigBody });
    const b = makeResult({ id: 'b', content: `${bigBody}\n\n` });
    const { kept } = dedupeIdenticalBodies([a, b]);
    expect(kept.map((r) => r.id)).toEqual(['a']);
  });
});

// ───────────────────── packed bundle (responseFormat: "packed") ─────────────────────

describe('responseFormat "packed"', () => {
  function packedOptions(extra: Partial<SearchOptions> = {}): SearchOptions {
    return baseOptions({ responseFormat: 'packed', ...extra });
  }

  it('assembles one bundle with location · symbol headers and capped bodies', () => {
    const r = makeResult({
      id: 'doc-1',
      content: 'fn body line',
      metadata: {
        relative_path: 'src/a.rs',
        file_path: '/repo/src/a.rs',
        chunk_symbol_name: 'process_batch',
        chunk_start_line: 10,
        chunk_end_line: 42,
      },
    });
    const { response, metrics } = shapeHitPayloads(makeResponse([r]), packedOptions());
    expect(response.packed_bundle).toBeDefined();
    const bundle = response.packed_bundle!;
    expect(bundle.text).toContain('src/a.rs:10-42');
    expect(bundle.text).toContain('process_batch');
    expect(bundle.text).toContain('fn body line');
    expect(bundle.included).toBe(1);
    expect(metrics.mode).toBe('packed');
    // results carry metadata-only entries (summary allowlist).
    expect(response.results[0].content).toBe('');
  });

  it('fills rank-strictly under maxResponseBytes and reports the dropped tail', () => {
    const body = 'y'.repeat(400);
    const results = [1, 2, 3, 4].map((i) =>
      makeResult({ id: `d${i}`, content: `${body}${i}`, metadata: { file_path: `src/f${i}.ts` } })
    );
    // Budget fits roughly two ~430-byte sections.
    const { response } = shapeHitPayloads(
      makeResponse(results),
      packedOptions({ maxResponseBytes: 900 })
    );
    const bundle = response.packed_bundle!;
    expect(bundle.included).toBe(2);
    expect(bundle.dropped).toBe(2);
    expect(bundle.text.length).toBeLessThanOrEqual(900);
    // All four hits remain discoverable as metadata entries.
    expect(response.results).toHaveLength(4);
  });

  it('always packs at least one section even when the budget is tiny', () => {
    const r = makeResult({ content: 'z'.repeat(500) });
    const { response } = shapeHitPayloads(
      makeResponse([r]),
      packedOptions({ maxResponseBytes: 10 })
    );
    expect(response.packed_bundle!.included).toBe(1);
  });

  it('skips byte-identical duplicate bodies inside the bundle', () => {
    const dupBody = 'const shared = true;\n'.repeat(10);
    const a = makeResult({ id: 'a', content: dupBody, metadata: { file_path: 'src/a.ts' } });
    const b = makeResult({ id: 'b', content: dupBody, metadata: { file_path: 'vendor/b.ts' } });
    const { response } = shapeHitPayloads(makeResponse([a, b]), packedOptions());
    const bundle = response.packed_bundle!;
    expect(bundle.included).toBe(1);
    expect(bundle.dropped).toBe(1);
  });

  it('caps long bodies at maxBytesPerHit with a retrieve() marker and counts them truncated', () => {
    const r = makeResult({ id: 'doc-9', content: 'w'.repeat(5000) });
    const { response, metrics } = shapeHitPayloads(makeResponse([r]), packedOptions());
    expect(metrics.hitsTruncated).toBe(1);
    expect(response.packed_bundle!.text).toContain('retrieve');
    expect(response.packed_bundle!.text.length).toBeLessThan(5000);
  });

  it('reports bytesOutShaped as the bundle length', () => {
    const r = makeResult({ content: 'body text here' });
    const { response, metrics } = shapeHitPayloads(makeResponse([r]), packedOptions());
    expect(metrics.bytesOutShaped).toBe(response.packed_bundle!.text.length);
    expect(metrics.bytesInShaped).toBeGreaterThan(0);
  });

  it('summary:true still wins over packed', () => {
    const r = makeResult({ content: 'body' });
    const { response, metrics } = shapeHitPayloads(
      makeResponse([r]),
      packedOptions({ summary: true })
    );
    expect(metrics.mode).toBe('summary');
    expect(response.packed_bundle).toBeUndefined();
  });
});
