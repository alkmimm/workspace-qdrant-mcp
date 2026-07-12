/**
 * Tests for the ScratchpadTool (list / update / delete).
 *
 * Mutations enqueue to the unified queue (item_type "text") scoped to the
 * resolved tenant; the entry is identified by its current content. list scrolls
 * the scratchpad Qdrant collection filtered by tenant.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { ScratchpadTool } from '../../src/tools/scratchpad.js';
import type { SqliteStateManager } from '../../src/clients/sqlite-state-manager.js';
import type { ProjectDetector } from '../../src/utils/project-detector.js';

let lastScrollFilter: unknown;
let lastScrollRequest: ScrollReq | undefined;

/**
 * Contents that "exist" in the mock store. The noteExists() pre-check scrolls
 * with a tenant + content filter (2 `must` entries); a content not in this set
 * returns zero points so the delete/update fails loudly instead of no-op'ing.
 */
const EXISTING_CONTENTS = new Set(['a note', 'old text', 'x']);

interface MatchCond {
  match?: { value?: unknown };
}
interface ScrollReq {
  filter?: { must?: MatchCond[] };
  limit?: number;
  offset?: unknown;
}
interface RetrieveReq {
  ids?: Array<string | number>;
}

/** Points resolvable by id (the `retrieve` path used by id-addressed ops). */
const POINTS_BY_ID: Record<string, { content: string; tenant_id: string }> = {
  'pt-1': { content: 'a note', tenant_id: 't1' },
  'pt-old': { content: 'old text', tenant_id: 't1' },
  'pt-foreign': { content: 'someone else note', tenant_id: 't-other' },
};

const DEFAULT_POINT = {
  id: 'pt-1',
  payload: {
    content: 'a project note',
    title: 'T',
    tags: ['x'],
    created_at: '2026-06-04T00:00:00Z',
  },
};

/** Points a tenant-only (list) scroll returns — tests override per case. */
let listPoints: Array<{ id: string; payload: Record<string, unknown> }> = [DEFAULT_POINT];
/** next_page_offset the list scroll returns (null = no further page). */
let listNextPageOffset: unknown = null;
/** count() result; undefined makes count() reject (best-effort `total` path). */
let countResult: number | undefined = undefined;

vi.mock('@qdrant/js-client-rest', () => ({
  QdrantClient: vi.fn().mockImplementation(() => ({
    scroll: vi.fn().mockImplementation((_coll: string, req: ScrollReq) => {
      lastScrollFilter = req.filter;
      lastScrollRequest = req;
      const must = req.filter?.must ?? [];
      // Existence pre-check (tenant + content) → 2 must entries. Echo a point
      // only when one of the matched values is a known-existing content.
      if (must.length >= 2) {
        const hit = must.some((c) => EXISTING_CONTENTS.has(c.match?.value as string));
        return Promise.resolve({ points: hit ? [DEFAULT_POINT] : [] });
      }
      // Tenant-only filter (list) → the configured page.
      return Promise.resolve({ points: listPoints, next_page_offset: listNextPageOffset });
    }),
    retrieve: vi.fn().mockImplementation((_coll: string, req: RetrieveReq) => {
      const id = String(req.ids?.[0] ?? '');
      const payload = POINTS_BY_ID[id];
      return Promise.resolve(payload ? [{ id, payload }] : []);
    }),
    count: vi.fn().mockImplementation(() =>
      countResult === undefined
        ? Promise.reject(new Error('count unavailable'))
        : Promise.resolve({ count: countResult })
    ),
  })),
}));

function mockStateManager(): SqliteStateManager {
  return {
    enqueueUnified: vi.fn().mockResolvedValue({ status: 'ok', data: { queueId: 'q-1' } }),
    upsertScratchpadMirror: vi.fn(),
  } as unknown as SqliteStateManager;
}

function mockProjectDetector(): ProjectDetector {
  return {
    getProjectInfo: vi.fn().mockResolvedValue({ projectId: 'detected' }),
  } as unknown as ProjectDetector;
}

function enqueueCall(sm: SqliteStateManager): unknown[] {
  return (sm.enqueueUnified as unknown as ReturnType<typeof vi.fn>).mock.calls[0];
}

describe('ScratchpadTool', () => {
  let sm: SqliteStateManager;
  let detector: ProjectDetector;
  let tool: ScratchpadTool;

  beforeEach(() => {
    vi.clearAllMocks();
    lastScrollRequest = undefined;
    listPoints = [DEFAULT_POINT];
    listNextPageOffset = null;
    countResult = undefined;
    sm = mockStateManager();
    detector = mockProjectDetector();
    tool = new ScratchpadTool({ qdrantUrl: 'http://localhost:6333' }, sm, detector);
  });

  it('delete enqueues a tenant-scoped delete op identified by content', async () => {
    const res = await tool.execute({ action: 'delete', content: 'a note', projectId: 't1' });

    expect(res.success).toBe(true);
    const call = enqueueCall(sm);
    expect(call[0]).toBe('text'); // item_type
    expect(call[1]).toBe('delete'); // op
    expect(call[2]).toBe('t1'); // tenant
    expect(call[4]).toMatchObject({ content: 'a note', source_type: 'scratchpad' });
  });

  it('delete without content is rejected (no destructive guess)', async () => {
    const res = await tool.execute({ action: 'delete', projectId: 't1' });
    expect(res.success).toBe(false);
    expect(sm.enqueueUnified).not.toHaveBeenCalled();
  });

  it('delete with non-matching content fails loudly instead of no-op enqueue', async () => {
    const res = await tool.execute({
      action: 'delete',
      content: 'truncated search hit…',
      projectId: 't1',
    });

    expect(res.success).toBe(false);
    expect(res.message).toMatch(/exact content/i);
    expect(sm.enqueueUnified).not.toHaveBeenCalled();
  });

  it('update with non-matching content fails before enqueue or mirror write', async () => {
    const res = await tool.execute({
      action: 'update',
      content: 'not the real note',
      newContent: 'whatever',
      projectId: 't1',
    });

    expect(res.success).toBe(false);
    expect(res.message).toMatch(/exact content/i);
    expect(sm.enqueueUnified).not.toHaveBeenCalled();
    expect(sm.upsertScratchpadMirror).not.toHaveBeenCalled();
  });

  it('update enqueues new content + old_content and refreshes the mirror', async () => {
    const res = await tool.execute({
      action: 'update',
      content: 'old text',
      newContent: 'new text',
      title: 'Title',
      tags: ['t'],
      projectId: 't1',
    });

    expect(res.success).toBe(true);
    const call = enqueueCall(sm);
    expect(call[1]).toBe('update');
    expect(call[2]).toBe('t1');
    expect(call[4]).toMatchObject({
      content: 'new text',
      old_content: 'old text',
      source_type: 'scratchpad',
      title: 'Title',
      tags: ['t'],
    });
    expect(sm.upsertScratchpadMirror).toHaveBeenCalledTimes(1);
  });

  it('update without newContent is rejected', async () => {
    const res = await tool.execute({ action: 'update', content: 'old', projectId: 't1' });
    expect(res.success).toBe(false);
    expect(sm.enqueueUnified).not.toHaveBeenCalled();
  });

  it('list scrolls the scratchpad collection filtered by tenant', async () => {
    const res = await tool.execute({ action: 'list', projectId: 't1', limit: 10 });

    expect(res.success).toBe(true);
    expect(res.count).toBe(1);
    // Summary is the default: preview + content_length instead of a full body.
    expect(res.entries?.[0]).toMatchObject({
      id: 'pt-1',
      preview: 'a project note',
      content_length: 'a project note'.length,
      title: 'T',
    });
    expect(res.entries?.[0]?.content).toBeUndefined();
    expect(res.hint).toMatch(/summary:false/);
    // tenant filter applied
    expect(JSON.stringify(lastScrollFilter)).toContain('t1');
  });

  it('list summary:false returns full note bodies (no preview fields)', async () => {
    const res = await tool.execute({ action: 'list', projectId: 't1', summary: false });

    expect(res.success).toBe(true);
    expect(res.entries?.[0]).toMatchObject({ id: 'pt-1', content: 'a project note' });
    expect(res.entries?.[0]?.preview).toBeUndefined();
    expect(res.hint).toBeUndefined();
  });

  it('list caps summary previews at 200 chars while reporting the full length', async () => {
    const bigContent = 'y'.repeat(5000);
    listPoints = [{ id: 'pt-big', payload: { content: bigContent } }];

    const res = await tool.execute({ action: 'list', projectId: 't1' });

    expect(res.entries?.[0]?.preview).toHaveLength(200);
    expect(res.entries?.[0]?.content_length).toBe(5000);
  });

  it('default summary list of many large notes stays under the response budget (66k-chars regression)', async () => {
    listPoints = Array.from({ length: 30 }, (_, i) => ({
      id: `pt-${i}`,
      payload: { content: `note ${i} `.padEnd(3000, 'z'), title: `Note ${i}` },
    }));

    const res = await tool.execute({ action: 'list', projectId: 't1', limit: 30 });

    expect(res.success).toBe(true);
    expect(res.count).toBe(30); // nothing dropped — summaries fit
    expect(res.budget_truncated).toBeUndefined();
    expect(JSON.stringify(res.entries).length).toBeLessThan(24000);
  });

  it('list enforces the byte budget on full bodies: drops the tail, reports it, resumes at the first dropped id', async () => {
    listPoints = Array.from({ length: 3 }, (_, i) => ({
      id: `pt-${i}`,
      payload: { content: 'x'.repeat(300) },
    }));

    const res = await tool.execute({
      action: 'list',
      projectId: 't1',
      summary: false,
      maxResponseBytes: 400,
    });

    expect(res.success).toBe(true);
    expect(res.count).toBe(1); // >=1 always kept
    expect(res.budget_truncated).toEqual({ dropped: 2 });
    // Cursor resumes AT the first dropped entry — budget pagination is lossless.
    expect(res.next_cursor).toBe('pt-1');
  });

  it('list passes the cursor as scroll offset and surfaces next_page_offset as next_cursor', async () => {
    listNextPageOffset = 'pt-9';

    const res = await tool.execute({ action: 'list', projectId: 't1', cursor: 'pt-5' });

    expect(lastScrollRequest?.offset).toBe('pt-5');
    expect(res.next_cursor).toBe('pt-9');
  });

  it('list reports the tenant total when the count API is available, and omits it when not', async () => {
    countResult = 7;
    const withCount = await tool.execute({ action: 'list', projectId: 't1' });
    expect(withCount.total).toBe(7);

    countResult = undefined; // count() rejects → best-effort omission, list still succeeds
    const withoutCount = await tool.execute({ action: 'list', projectId: 't1' });
    expect(withoutCount.success).toBe(true);
    expect(withoutCount.total).toBeUndefined();
  });

  it('resolves the tenant from the project detector when no projectId is given', async () => {
    await tool.execute({ action: 'delete', content: 'x' });
    expect(detector.getProjectInfo).toHaveBeenCalled();
    expect(enqueueCall(sm)[2]).toBe('detected');
  });

  it('delete by point id resolves the content and enqueues the same flow', async () => {
    const res = await tool.execute({ action: 'delete', id: 'pt-1', projectId: 't1' });

    expect(res.success).toBe(true);
    const call = enqueueCall(sm);
    expect(call[1]).toBe('delete');
    expect(call[4]).toMatchObject({ content: 'a note', source_type: 'scratchpad' });
  });

  it('delete by id refuses a point belonging to another tenant (no cross-tenant leak)', async () => {
    const res = await tool.execute({ action: 'delete', id: 'pt-foreign', projectId: 't1' });

    expect(res.success).toBe(false);
    expect(res.message).toMatch(/point id/i);
    expect(sm.enqueueUnified).not.toHaveBeenCalled();
  });

  it('delete by unknown id fails loudly', async () => {
    const res = await tool.execute({ action: 'delete', id: 'pt-nope', projectId: 't1' });

    expect(res.success).toBe(false);
    expect(res.message).toMatch(/point id/i);
    expect(sm.enqueueUnified).not.toHaveBeenCalled();
  });

  it('update by point id enqueues new content + resolved old_content', async () => {
    const res = await tool.execute({
      action: 'update',
      id: 'pt-old',
      newContent: 'new text',
      projectId: 't1',
    });

    expect(res.success).toBe(true);
    const call = enqueueCall(sm);
    expect(call[1]).toBe('update');
    expect(call[4]).toMatchObject({
      content: 'new text',
      old_content: 'old text',
      source_type: 'scratchpad',
    });
  });

  it('verbatim content wins over id when both are given', async () => {
    const res = await tool.execute({
      action: 'delete',
      content: 'a note',
      id: 'pt-old',
      projectId: 't1',
    });

    expect(res.success).toBe(true);
    expect(enqueueCall(sm)[4]).toMatchObject({ content: 'a note' });
  });
});
