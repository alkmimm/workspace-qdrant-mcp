/**
 * `retrieve` filter-mode paging: `offset` must skip N documents, and only
 * sane values may reach the Qdrant request.
 *
 * Reproduced 2026-09-05: `retrieve {filter:{relative_path}, limit:12}` with
 * offset 0 / 12 / 24 returned the SAME 12 chunks each time (hasMore:true).
 * The numeric offset was passed straight to Qdrant's scroll `offset`, which is
 * a point-id CURSOR, not a skip count: on a UUID-keyed collection the integer
 * sorts before every point and the scroll restarts at page 1. The skip is now
 * emulated client-side (over-fetch, slice), normalized like grep's offset, and
 * bounded so a deep offset cannot ask Qdrant for an unbounded page.
 */
import { describe, it, expect, vi, beforeEach } from 'vitest';

const scrollMock = vi.fn();

vi.mock('@qdrant/js-client-rest', () => ({
  QdrantClient: vi.fn().mockImplementation(() => ({ scroll: scrollMock })),
}));

import { RetrieveTool } from '../../src/tools/retrieve.js';
import type { ProjectDetector } from '../../src/utils/project-detector.js';

const detector = {
  getProjectInfo: vi.fn().mockResolvedValue({ projectId: 'proj-1', projectPath: '/proj' }),
} as unknown as ProjectDetector;

function points(n: number) {
  return Array.from({ length: n }, (_, i) => ({
    id: `p-${i}`,
    payload: { content: `chunk ${i}`, relative_path: 'a.ts', tenant_id: 'proj-1' },
  }));
}

function scrollLimit(): number {
  return (scrollMock.mock.calls[0]?.[1] as Record<string, unknown>)['limit'] as number;
}

describe('retrieve filter-mode offset', () => {
  beforeEach(() => {
    scrollMock.mockReset();
  });

  it('emulates the numeric skip client-side instead of passing it as a Qdrant point-id cursor', async () => {
    scrollMock.mockResolvedValue({ points: points(8) });
    const tool = new RetrieveTool({ qdrantUrl: 'http://localhost:6333' }, detector);
    const res = await tool.retrieve({
      collection: 'scratchpad',
      filter: { document_id: 'x' },
      limit: 3,
      offset: 3,
      projectId: 'proj-1',
    });

    expect(res.success).toBe(true);
    const req = scrollMock.mock.calls[0]?.[1] as Record<string, unknown>;
    expect(req['offset']).toBeUndefined();
    expect(req['limit']).toBe(7); // offset + limit + 1 → knows whether a further page exists
    expect(res.documents.map((d) => d.id)).toEqual(['p-3', 'p-4', 'p-5']);
    expect(res.hasMore).toBe(true);
  });

  it('reports hasMore=false on the last page and an empty page past the end', async () => {
    scrollMock.mockResolvedValue({ points: points(5) });
    const tool = new RetrieveTool({ qdrantUrl: 'http://localhost:6333' }, detector);
    const last = await tool.retrieve({
      collection: 'scratchpad',
      filter: { document_id: 'x' },
      limit: 3,
      offset: 3,
      projectId: 'proj-1',
    });
    expect(last.documents.map((d) => d.id)).toEqual(['p-3', 'p-4']);
    expect(last.hasMore).toBe(false);

    scrollMock.mockResolvedValue({ points: points(5) });
    const past = await tool.retrieve({
      collection: 'scratchpad',
      filter: { document_id: 'x' },
      limit: 3,
      offset: 9,
      projectId: 'proj-1',
    });
    expect(past.success).toBe(true);
    expect(past.documents).toEqual([]);
    expect(past.hasMore).toBe(false);
  });

  it('normalizes a negative or fractional offset like grep does (never a Qdrant parameter)', async () => {
    scrollMock.mockResolvedValue({ points: points(8) });
    const tool = new RetrieveTool({ qdrantUrl: 'http://localhost:6333' }, detector);
    const negative = await tool.retrieve({
      collection: 'scratchpad',
      filter: { document_id: 'x' },
      limit: 3,
      offset: -5,
      projectId: 'proj-1',
    });
    expect(scrollLimit()).toBe(4); // 0 + 3 + 1: a negative offset is page 1, not a shrunken page
    expect(negative.documents.map((d) => d.id)).toEqual(['p-0', 'p-1', 'p-2']);
    expect(negative.hasMore).toBe(true);

    scrollMock.mockReset();
    scrollMock.mockResolvedValue({ points: points(8) });
    const fractional = await tool.retrieve({
      collection: 'scratchpad',
      filter: { document_id: 'x' },
      limit: 3,
      offset: 2.7,
      projectId: 'proj-1',
    });
    expect(scrollLimit()).toBe(6); // floor(2.7) = 2 → 2 + 3 + 1, an integer Qdrant limit
    expect(fractional.documents.map((d) => d.id)).toEqual(['p-2', 'p-3', 'p-4']);
  });

  it('refuses an offset that would over-fetch beyond the filter window instead of asking Qdrant for it', async () => {
    scrollMock.mockResolvedValue({ points: [] });
    const tool = new RetrieveTool({ qdrantUrl: 'http://localhost:6333' }, detector);
    const res = await tool.retrieve({
      collection: 'scratchpad',
      filter: { document_id: 'x' },
      limit: 10,
      offset: 5000,
      projectId: 'proj-1',
    });
    expect(res.success).toBe(false);
    expect(res.message).toMatch(/filter window/);
    expect(scrollMock).not.toHaveBeenCalled();
  });

  it('carries the read-side project echo on project-scoped collections', async () => {
    scrollMock.mockResolvedValue({ points: points(2) });
    const tool = new RetrieveTool({ qdrantUrl: 'http://localhost:6333' }, detector);
    const res = await tool.retrieve({
      collection: 'scratchpad',
      filter: { document_id: 'x' },
      limit: 5,
      projectId: 'proj-1',
    });
    expect(res.success).toBe(true);
    expect(res.project_id).toBe('proj-1');
    expect(res.project_source).toBe('projectId');
  });
});
