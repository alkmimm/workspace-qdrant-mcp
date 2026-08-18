/**
 * Tests for the shared "you got a slice, not the whole symbol" hint.
 *
 * Field feedback (DOC-V2, 2026-08-13): a single-file pathGlob search over
 * `Capability.java` — 326 lines, ONE enum, 12 fragments — returned only
 * fragment 11, the utility tail, and the call was wasted. The payload already
 * carried the fragment fields; what was missing was telling the caller what
 * they meant.
 */

import { describe, it, expect } from 'vitest';

import { fragmentSliceHint, firstFragmentSliceHint } from '../../src/common/fragment-hint.js';

/** The real payload of the hit that produced the report. */
const TAIL_FRAGMENT = {
  chunk_is_fragment: 'true',
  chunk_fragment_index: '11',
  chunk_total_fragments: '12',
  chunk_symbol_name: 'Capability',
  chunk_chunk_type: 'enum',
  relative_path: 'doc-backend/domain/src/main/java/com/doc/domain/model/security/Capability.java',
};

describe('fragmentSliceHint', () => {
  it('fires on a non-zero fragment and names index, total, symbol and path', () => {
    const hint = fragmentSliceHint({ ...TAIL_FRAGMENT });
    expect(hint).toBeDefined();
    expect(hint).toContain('fragment 11 of 12');
    expect(hint).toContain('Capability');
    expect(hint).toContain('Capability.java');
    // The actionable half: what to do instead.
    expect(hint).toContain('fragment 0');
  });

  it('stays silent on fragment 0 — it already carries the declarations', () => {
    expect(
      fragmentSliceHint({ ...TAIL_FRAGMENT, chunk_fragment_index: '0' })
    ).toBeUndefined();
  });

  it('stays silent on a whole (unfragmented) symbol', () => {
    expect(
      fragmentSliceHint({ chunk_symbol_name: 'collapseBranchSet', relative_path: 'a.ts' })
    ).toBeUndefined();
  });

  it('stays silent when the payload claims a single fragment', () => {
    expect(
      fragmentSliceHint({
        ...TAIL_FRAGMENT,
        chunk_fragment_index: '0',
        chunk_total_fragments: '1',
      })
    ).toBeUndefined();
  });

  it('accepts numeric as well as string fragment fields', () => {
    // The daemon writes these as strings; a future numeric payload must not
    // silently disable the hint.
    const hint = fragmentSliceHint({
      chunk_is_fragment: 'true',
      chunk_fragment_index: 7,
      chunk_total_fragments: 9,
      chunk_symbol_name: 'Big',
    });
    expect(hint).toContain('fragment 7 of 9');
  });

  it('degrades gracefully with no path', () => {
    const hint = fragmentSliceHint({
      chunk_is_fragment: 'true',
      chunk_fragment_index: '3',
      chunk_total_fragments: '4',
    });
    expect(hint).toContain('fragment 3 of 4');
    expect(hint).not.toContain(' in undefined');
  });
});

describe('firstFragmentSliceHint', () => {
  it('returns the first applicable hint across a page', () => {
    const hint = firstFragmentSliceHint([
      { metadata: { chunk_symbol_name: 'whole' } },
      { metadata: { ...TAIL_FRAGMENT } },
    ]);
    expect(hint).toContain('fragment 11 of 12');
  });

  it('returns undefined when no hit is a tail fragment', () => {
    expect(
      firstFragmentSliceHint([{ metadata: { chunk_symbol_name: 'whole' } }])
    ).toBeUndefined();
  });
});
