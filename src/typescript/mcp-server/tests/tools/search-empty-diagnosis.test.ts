/**
 * Empty-result diagnosis for the SEMANTIC search path (issue #247). The shared
 * `diagnoseEmptyResult` funnel is reused in `mode: 'semantic'`, which must:
 *   1a. still blame a path filter whose SHAPE selects no indexed file (glob is
 *       mode-agnostic — same message as grep/exact),
 *   1b. NOT reuse the literal "pattern absent / check casing" wording — semantic
 *       has no literal, so a well-formed glob over relevant-less files yields a
 *       score-threshold/relevance message instead, and
 *   2.  still name a not-yet-indexed branch (mode-agnostic).
 */

import { describe, it, expect, vi } from 'vitest';
import {
  diagnoseEmptyResult,
  noSemanticMatchUnderPathFilterMessage,
  patternAbsentUnderPathFilterMessage,
} from '../../src/tools/empty-diagnosis.js';
import type { SearchDbReader } from '../../src/clients/search-db-reader.js';

function makeReader(
  rows: Array<{ branch: string; files: number }>,
  filesMatchingPathFilters = 0
): SearchDbReader {
  return {
    listBranchCounts: vi.fn().mockReturnValue(
      rows.map((r) => ({ tenant_id: 'project-a', branch: r.branch, files: r.files, total_bytes: 0 }))
    ),
    countFilesMatchingPathFilters: vi.fn().mockReturnValue(filesMatchingPathFilters),
  } as unknown as SearchDbReader;
}

describe('diagnoseEmptyResult — semantic mode (#247)', () => {
  it('1a: blames the glob SHAPE (mode-agnostic) when it selects no indexed file', async () => {
    const msg = await diagnoseEmptyResult({
      tenantId: 'project-a',
      branch: 'main',
      pathGlob: 'management/test/**/*.dart',
      pathExclude: undefined,
      searchDbReader: makeReader([{ branch: 'main', files: 5 }], 0), // glob selects 0
      mode: 'semantic',
      countWithoutPathFilter: async () => 7, // relevant results exist without the filter
    });
    expect(msg).toMatch(/matches NO indexed file/i);
    expect(msg).toMatch(/ADJACENT/);
  });

  it('1b: a well-formed glob over relevant-less files gives the SCORE-THRESHOLD message, not casing', async () => {
    const msg = await diagnoseEmptyResult({
      tenantId: 'project-a',
      branch: 'main',
      pathGlob: 'src/**/*.rs',
      pathExclude: undefined,
      searchDbReader: makeReader([{ branch: 'main', files: 5 }], 9), // glob selects 9 real files
      mode: 'semantic',
      countWithoutPathFilter: async () => 4,
    });
    expect(msg).toMatch(/score threshold/i);
    expect(msg).toMatch(/well-formed/i);
    expect(msg).toContain('9 indexed file(s)');
    // Must NOT reach for literal-mode wording.
    expect(msg).not.toMatch(/naming\/casing/i);
    expect(msg).not.toMatch(/snake_case/i);
    expect(msg).not.toMatch(/ADJACENT/);
  });

  it('literal mode (default) keeps the casing wording for the same inputs — the fork is real', async () => {
    const msg = await diagnoseEmptyResult({
      tenantId: 'project-a',
      branch: 'main',
      pathGlob: 'src/**/*.rs',
      pathExclude: undefined,
      searchDbReader: makeReader([{ branch: 'main', files: 5 }], 9),
      // no mode → 'literal'
      countWithoutPathFilter: async () => 4,
    });
    expect(msg).toMatch(/naming\/casing/i);
    expect(msg).not.toMatch(/score threshold/i);
  });

  it('2: names a not-yet-indexed branch in semantic mode (mode-agnostic probe)', async () => {
    const msg = await diagnoseEmptyResult({
      tenantId: 'project-a',
      branch: 'fix/new-feature',
      pathGlob: undefined,
      pathExclude: undefined,
      searchDbReader: makeReader([
        { branch: 'main', files: 145 },
        { branch: 'develop', files: 10 },
      ]),
      mode: 'semantic',
      countWithoutPathFilter: async () => 0,
    });
    expect(msg).toMatch(/0 files indexed under its own name/i);
    expect(msg).toMatch(/main, develop/);
  });

  it('returns undefined when the query is empty even without the filter (not the filter’s fault)', async () => {
    const msg = await diagnoseEmptyResult({
      tenantId: 'project-a',
      branch: 'main',
      pathGlob: 'src/**/*.rs',
      pathExclude: undefined,
      searchDbReader: makeReader([{ branch: 'main', files: 5 }], 9),
      mode: 'semantic',
      countWithoutPathFilter: async () => 0, // nothing relevant anywhere
    });
    expect(msg).toBeUndefined();
  });
});

describe('noSemanticMatchUnderPathFilterMessage', () => {
  it('names the filter, the file count, and points at the threshold — not casing', () => {
    const msg = noSemanticMatchUnderPathFilterMessage(50, 9, 'src/**/*.rs', undefined);
    expect(msg).toContain('pathGlob "src/**/*.rs"');
    expect(msg).toContain('9 indexed file(s)'); // below cap → no "+"
    expect(msg).toContain('50+'); // unfiltered probe hit the cap → "+"
    expect(msg).toMatch(/score threshold/i);
    expect(msg).toMatch(/scoreThreshold/); // actionable knob named
    expect(msg).not.toMatch(/naming\/casing/i);
    expect(msg).not.toMatch(/ADJACENT/);
  });

  it('reads distinctly from the literal-mode message for identical inputs', () => {
    const semantic = noSemanticMatchUnderPathFilterMessage(50, 9, 'src/**/*.rs', undefined);
    const literal = patternAbsentUnderPathFilterMessage(50, 9, 'src/**/*.rs', undefined);
    expect(semantic).not.toEqual(literal);
  });
});
