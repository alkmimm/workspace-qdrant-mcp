/**
 * Tests for the idle-session reaper that reclaims leaked stateful sessions,
 * and for the header filter behind the chunked-only MCP response framing.
 *
 * A client that loses its SSE stream re-initializes with a NEW session id and
 * never DELETEs the old one, so without eviction each reconnect would leak a
 * transport + MCP server instance (and its daemon gRPC connection).
 */

import { EventEmitter } from 'node:events';
import { beforeEach, describe, it, expect, vi } from 'vitest';
import type { StreamableHTTPServerTransport } from '@modelcontextprotocol/sdk/server/streamableHttp.js';
import {
  observeToolResponseDelivery,
  reapIdleSessions,
  SessionEventStore,
  toolNamesFromBody,
  updateActiveRequests,
  withoutContentLength,
} from '../src/mcp-http-server.js';
import { register } from '../src/telemetry/metrics.js';

function fakeTransport(): StreamableHTTPServerTransport {
  return {
    close: vi.fn().mockResolvedValue(undefined),
  } as unknown as StreamableHTTPServerTransport;
}

const NOW = 1_000_000_000_000;
const TTL = 10 * 60_000; // 10 min

beforeEach(() => {
  register.resetMetrics();
});

describe('reapIdleSessions', () => {
  it('evicts only sessions idle longer than the TTL', () => {
    const a = fakeTransport(); // fresh
    const b = fakeTransport(); // stale
    const c = fakeTransport(); // borderline-fresh
    const transports = new Map([
      ['a', a],
      ['b', b],
      ['c', c],
    ]);
    const lastSeen = new Map([
      ['a', NOW - 1 * 60_000],
      ['b', NOW - 20 * 60_000],
      ['c', NOW - 9 * 60_000],
    ]);

    const evicted = reapIdleSessions(transports, lastSeen, new Map(), NOW, TTL);

    expect(evicted).toEqual(['b']);
    expect(b.close).toHaveBeenCalledTimes(1);
    expect(a.close).not.toHaveBeenCalled();
    expect(c.close).not.toHaveBeenCalled();
  });

  it('treats a session with no recorded activity as infinitely idle', () => {
    const orphan = fakeTransport();
    const transports = new Map([['orphan', orphan]]);
    const lastSeen = new Map<string, number>(); // no entry for "orphan"

    const evicted = reapIdleSessions(transports, lastSeen, new Map(), NOW, TTL);

    expect(evicted).toEqual(['orphan']);
    expect(orphan.close).toHaveBeenCalledTimes(1);
  });

  it('evicts nothing when all sessions are within the TTL', () => {
    const a = fakeTransport();
    const transports = new Map([['a', a]]);
    const lastSeen = new Map([['a', NOW - 5 * 60_000]]);

    const evicted = reapIdleSessions(transports, lastSeen, new Map(), NOW, TTL);

    expect(evicted).toEqual([]);
    expect(a.close).not.toHaveBeenCalled();
  });

  it('does not evict a stale session while a long-running request is active', () => {
    const active = fakeTransport();
    const transports = new Map([['search-eval', active]]);
    const lastSeen = new Map([['search-eval', NOW - 20 * 60_000]]);
    const activeRequests = new Map([['search-eval', 1]]);

    const evicted = reapIdleSessions(transports, lastSeen, activeRequests, NOW, TTL);

    expect(evicted).toEqual([]);
    expect(active.close).not.toHaveBeenCalled();
  });
});

describe('active request tracking', () => {
  it('keeps a concurrent count and removes the entry at zero', () => {
    const active = new Map<string, number>();
    updateActiveRequests(active, 'session', 1);
    updateActiveRequests(active, 'session', 1);
    expect(active.get('session')).toBe(2);
    updateActiveRequests(active, 'session', -1);
    expect(active.get('session')).toBe(1);
    updateActiveRequests(active, 'session', -1);
    expect(active.has('session')).toBe(false);
  });
});

describe('SessionEventStore', () => {
  it('replays results written after the client priming event', async () => {
    const store = new SessionEventStore();
    const primingId = await store.storeEvent('stream-a', {} as never);
    const response = { jsonrpc: '2.0', id: 2, result: { ok: true } } as const;
    const responseId = await store.storeEvent('stream-a', response);
    await store.storeEvent('stream-b', { jsonrpc: '2.0', method: 'notifications/progress' });
    const replayed: Array<{ id: string; message: unknown }> = [];

    const streamId = await store.replayEventsAfter(primingId, {
      send: async (id, message) => {
        replayed.push({ id, message });
      },
    });

    expect(streamId).toBe('stream-a');
    expect(replayed).toEqual([{ id: responseId, message: response }]);
    expect(await store.getStreamIdForEventId(primingId)).toBe('stream-a');
  });

  it('bounds retained events and rejects an expired resume cursor', async () => {
    const store = new SessionEventStore(2);
    const expired = await store.storeEvent('stream', {} as never);
    await store.storeEvent('stream', { jsonrpc: '2.0', method: 'notifications/one' });
    await store.storeEvent('stream', { jsonrpc: '2.0', method: 'notifications/two' });

    expect(await store.getStreamIdForEventId(expired)).toBeUndefined();
    expect(await store.replayEventsAfter(expired, { send: vi.fn() })).toBe('');
  });
});

class FakeResponse extends EventEmitter {
  statusCode = 200;
  writableFinished = false;
}

async function deliveryCount(tool: string, outcome: string): Promise<number> {
  const metric = (await register.getMetricsAsJSON()).find(
    (candidate) => candidate.name === 'wqm_mcp_tool_response_delivery_total'
  );
  const sample = metric?.values.find(
    (value) => value.labels['tool'] === tool && value.labels['outcome'] === outcome
  );
  return sample?.value ?? 0;
}

describe('tool response delivery telemetry', () => {
  it('records a completed HTTP response as delivered', async () => {
    const response = new FakeResponse();
    observeToolResponseDelivery(response as never, 'search_eval');
    response.writableFinished = true;
    response.emit('finish');

    expect(await deliveryCount('search_eval', 'delivered')).toBe(1);
    expect(await deliveryCount('search_eval', 'failed')).toBe(0);
  });

  it('records a connection closed before finish as failed exactly once', async () => {
    const response = new FakeResponse();
    observeToolResponseDelivery(response as never, 'search_eval');
    response.emit('error', new Error('socket reset'));
    response.emit('close');

    expect(await deliveryCount('search_eval', 'failed')).toBe(1);
    expect(await deliveryCount('search_eval', 'delivered')).toBe(0);
  });
});

describe('toolNamesFromBody', () => {
  it('extracts tool calls from single and batch JSON-RPC bodies', () => {
    expect(toolNamesFromBody({ method: 'tools/call', params: { name: 'search_eval' } })).toEqual([
      'search_eval',
    ]);
    expect(
      toolNamesFromBody([
        { method: 'notifications/initialized' },
        { method: 'tools/call', params: { name: 'search' } },
        { method: 'tools/call', params: { name: 'search' } },
      ])
    ).toEqual(['search', 'search']);
  });
});

describe('withoutContentLength', () => {
  it('drops Content-Length in any case from a header object, keeping the rest', () => {
    const headers = {
      'content-type': 'text/event-stream',
      'Content-Length': 1998,
      'mcp-session-id': 's',
    };

    expect(withoutContentLength(headers)).toEqual({
      'content-type': 'text/event-stream',
      'mcp-session-id': 's',
    });
    expect(headers['Content-Length']).toBe(1998); // the caller's object is not mutated
  });

  it('drops Content-Length pairs from the flat raw-array form', () => {
    expect(
      withoutContentLength(['Content-Type', 'application/json', 'CONTENT-LENGTH', '42'])
    ).toEqual(['Content-Type', 'application/json']);
  });

  it('returns a status message or a missing argument unchanged', () => {
    expect(withoutContentLength('OK')).toBe('OK');
    expect(withoutContentLength(undefined)).toBeUndefined();
  });
});
