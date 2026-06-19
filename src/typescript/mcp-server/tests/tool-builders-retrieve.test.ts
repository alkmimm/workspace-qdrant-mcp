/**
 * Tests for buildRetrieveOptions — the raw-args → RetrieveOptions mapper.
 *
 * Regression guard: the builder whitelists fields explicitly, so a tool-schema
 * field is silently dropped unless it is wired here too. `filePath`/`lineNumber`
 * were advertised in the schema and implemented in the handler
 * (`retrieveByLocation`) but never mapped by the builder, so the documented
 * exact-search-locator form (`filePath` + `lineNumber`) degraded into a broad
 * tenant scroll that returned arbitrary documents.
 */

import { describe, it, expect } from 'vitest';
import { buildRetrieveOptions } from '../src/tool-builders/retrieve.js';

describe('buildRetrieveOptions — exact-search locator (filePath + lineNumber)', () => {
  it('maps filePath through to options', () => {
    const opts = buildRetrieveOptions({ filePath: 'src/tools/search-filters.ts' });
    expect(opts.filePath).toBe('src/tools/search-filters.ts');
  });

  it('maps lineNumber through (including 1, the smallest valid line)', () => {
    expect(buildRetrieveOptions({ filePath: 'a.ts', lineNumber: 44 }).lineNumber).toBe(44);
    expect(buildRetrieveOptions({ filePath: 'a.ts', lineNumber: 1 }).lineNumber).toBe(1);
  });

  it('does not flag filePath/lineNumber as unknown args (they are in the schema)', () => {
    const opts = buildRetrieveOptions({ filePath: 'a.ts', lineNumber: 1 });
    expect(opts.unknownArgs).toBeUndefined();
  });

  it('leaves filePath/lineNumber undefined when omitted', () => {
    const opts = buildRetrieveOptions({ documentId: 'abc' });
    expect(opts.filePath).toBeUndefined();
    expect(opts.lineNumber).toBeUndefined();
  });
});

describe('buildRetrieveOptions — existing fields still map', () => {
  it('maps documentId and collection', () => {
    const opts = buildRetrieveOptions({ documentId: 'id-1', collection: 'scratchpad' });
    expect(opts.documentId).toBe('id-1');
    expect(opts.collection).toBe('scratchpad');
  });

  it('flags genuinely unknown args (e.g. the search-only `query`)', () => {
    const opts = buildRetrieveOptions({ documentId: 'id-1', query: 'nope' });
    expect(opts.unknownArgs).toEqual(['query']);
  });
});
