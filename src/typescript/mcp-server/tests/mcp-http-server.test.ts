/**
 * Tests for the idle-session reaper that reclaims leaked stateful sessions,
 * and for the header filter behind the chunked-only MCP response framing.
 *
 * A client that loses its SSE stream re-initializes with a NEW session id and
 * never DELETEs the old one, so without eviction each reconnect would leak a
 * transport + MCP server instance (and its daemon gRPC connection).
 */

import { describe, it, expect, vi } from 'vitest';
import type { StreamableHTTPServerTransport } from '@modelcontextprotocol/sdk/server/streamableHttp.js';
import { reapIdleSessions, withoutContentLength } from '../src/mcp-http-server.js';

function fakeTransport(): StreamableHTTPServerTransport {
  return {
    close: vi.fn().mockResolvedValue(undefined),
  } as unknown as StreamableHTTPServerTransport;
}

const NOW = 1_000_000_000_000;
const TTL = 10 * 60_000; // 10 min

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

    const evicted = reapIdleSessions(transports, lastSeen, NOW, TTL);

    expect(evicted).toEqual(['b']);
    expect(b.close).toHaveBeenCalledTimes(1);
    expect(a.close).not.toHaveBeenCalled();
    expect(c.close).not.toHaveBeenCalled();
  });

  it('treats a session with no recorded activity as infinitely idle', () => {
    const orphan = fakeTransport();
    const transports = new Map([['orphan', orphan]]);
    const lastSeen = new Map<string, number>(); // no entry for "orphan"

    const evicted = reapIdleSessions(transports, lastSeen, NOW, TTL);

    expect(evicted).toEqual(['orphan']);
    expect(orphan.close).toHaveBeenCalledTimes(1);
  });

  it('evicts nothing when all sessions are within the TTL', () => {
    const a = fakeTransport();
    const transports = new Map([['a', a]]);
    const lastSeen = new Map([['a', NOW - 5 * 60_000]]);

    const evicted = reapIdleSessions(transports, lastSeen, NOW, TTL);

    expect(evicted).toEqual([]);
    expect(a.close).not.toHaveBeenCalled();
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
