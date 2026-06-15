/**
 * Regression guardrail for findExactContentRuleId.
 *
 * The exact-duplicate idempotency check must paginate the rules scroll. A
 * single 256-row page let a duplicate that sorted past the first page slip
 * through, creating exactly the duplicate row the check exists to prevent. The
 * check is also fail-open: a Qdrant read error must not block the add.
 */
import { describe, it, expect, vi } from 'vitest';
import type { QdrantClient } from '@qdrant/js-client-rest';
import { findExactContentRuleId } from '../../src/tools/rules-mutation-helpers.js';
import { FIELD_CONTENT } from '../../src/common/native-bridge.js';

interface Page {
  points: Array<{ id: string; payload: Record<string, unknown> }>;
  next?: string;
}

function qdrantWithPages(pages: Page[]): QdrantClient {
  let call = 0;
  const scroll = vi.fn(async () => {
    const page = pages[call] ?? { points: [] };
    call++;
    return { points: page.points, next_page_offset: page.next ?? null };
  });
  return { scroll } as unknown as QdrantClient;
}

const GLOBAL_FILTER = { must: [{ key: 'scope', match: { value: 'global' } }] };

describe('findExactContentRuleId', () => {
  it('finds an exact duplicate on a later page (paginates past the first 256)', async () => {
    const filler: Page['points'] = Array.from({ length: 256 }, (_, i) => ({
      id: `p${i}`,
      payload: { [FIELD_CONTENT]: `unrelated rule ${i}` },
    }));
    const q = qdrantWithPages([
      { points: filler, next: 'cursor-1' },
      {
        points: [{ id: 'dup-point', payload: { [FIELD_CONTENT]: 'the duplicate rule', label: 'dup' } }],
      },
    ]);

    const found = await findExactContentRuleId(q, GLOBAL_FILTER, '  the duplicate rule  ');

    expect(found).toEqual({ id: 'dup-point', label: 'dup' });
    expect(q.scroll as unknown as ReturnType<typeof vi.fn>).toHaveBeenCalledTimes(2);
    // The second page request must carry the cursor returned by page one.
    const secondCallReq = (q.scroll as unknown as ReturnType<typeof vi.fn>).mock.calls[1][1];
    expect(secondCallReq).toMatchObject({ offset: 'cursor-1' });
  });

  it('returns null when no page contains the content', async () => {
    const q = qdrantWithPages([
      { points: [{ id: 'a', payload: { [FIELD_CONTENT]: 'x' } }], next: 'c1' },
      { points: [{ id: 'b', payload: { [FIELD_CONTENT]: 'y' } }] },
    ]);

    const found = await findExactContentRuleId(q, GLOBAL_FILTER, 'z');

    expect(found).toBeNull();
    expect(q.scroll as unknown as ReturnType<typeof vi.fn>).toHaveBeenCalledTimes(2);
  });

  it('stops at the first page when Qdrant returns no further cursor', async () => {
    const q = qdrantWithPages([{ points: [{ id: 'm', payload: { [FIELD_CONTENT]: 'match me' } }] }]);

    const found = await findExactContentRuleId(q, GLOBAL_FILTER, 'match me');

    expect(found).toEqual({ id: 'm' });
    expect(q.scroll as unknown as ReturnType<typeof vi.fn>).toHaveBeenCalledTimes(1);
  });

  it('fails open (returns null) when the scroll throws', async () => {
    const scroll = vi.fn(async () => {
      throw new Error('qdrant unavailable');
    });
    const q = { scroll } as unknown as QdrantClient;

    const found = await findExactContentRuleId(q, GLOBAL_FILTER, 'anything');

    expect(found).toBeNull();
  });

  it('returns null for empty content without scrolling', async () => {
    const q = qdrantWithPages([{ points: [{ id: 'a', payload: { [FIELD_CONTENT]: '' } }] }]);

    const found = await findExactContentRuleId(q, GLOBAL_FILTER, '   ');

    expect(found).toBeNull();
    expect(q.scroll as unknown as ReturnType<typeof vi.fn>).not.toHaveBeenCalled();
  });
});
