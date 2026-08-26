/**
 * Ordering contract for the search_events write pair.
 *
 * Both writes are fire-and-forget gRPC calls and every update addresses its
 * row by `event_id`, so an update that reaches the daemon before the insert
 * matches nothing and is silently dropped. Ordering used to be implicit —
 * guaranteed only by however long the instrumented operation took — which
 * held for every I/O-bound tool and broke the moment a same-tick tool (the
 * static `help` lookup) joined the lane: 0 of 2 live rows carried
 * latency_ms/result_count, against ~100% for every other op.
 */

import { describe, it, expect, vi } from 'vitest';

import {
  logSearchEvent,
  updateSearchEvent,
  updateSearchEventEconomy,
} from '../../src/clients/search-event-queries.js';
import type { DaemonClient } from '../../src/clients/daemon-client.js';

/** Daemon double recording call order, with a controllable insert delay. */
function makeDaemon(insertDelayMs: number) {
  const order: string[] = [];
  const client = {
    logSearchEvent: vi.fn(async () => {
      await new Promise((r) => setTimeout(r, insertDelayMs));
      order.push('insert');
    }),
    updateSearchEvent: vi.fn(async () => {
      order.push('update');
    }),
    updateSearchEventEconomy: vi.fn(async () => {
      order.push('economy');
    }),
  } as unknown as DaemonClient;
  return { client, order };
}

const baseEvent = {
  id: 'evt-1',
  actor: 'claude',
  tool: 'mcp_qdrant',
  op: 'help',
};

/** Let every queued microtask/timer settle. */
async function drain(ms = 20): Promise<void> {
  await new Promise((r) => setTimeout(r, ms));
}

describe('search_events write ordering', () => {
  it('sends the update only after a slow insert has landed (same-tick caller)', async () => {
    // The `help` shape: the instrumented work resolves synchronously, so the
    // update is dispatched in the same tick as the insert.
    const { client, order } = makeDaemon(10);

    logSearchEvent(client, baseEvent);
    updateSearchEvent(client, 'evt-1', { resultCount: 1, latencyMs: 0 });

    await drain(50);
    expect(order).toEqual(['insert', 'update']);
  });

  it('orders the economy sidecar after the insert too', async () => {
    const { client, order } = makeDaemon(10);

    logSearchEvent(client, baseEvent);
    updateSearchEventEconomy(client, 'evt-1', {
      bytesIn: 10,
      bytesOut: 5,
      hitsTruncated: 0,
      shapeMode: 'none',
    });

    await drain(50);
    expect(order).toEqual(['insert', 'economy']);
  });

  it('still sends the update when the insert fails (never stranded)', async () => {
    const order: string[] = [];
    const client = {
      logSearchEvent: vi.fn(async () => {
        await new Promise((r) => setTimeout(r, 5));
        order.push('insert-failed');
        throw new Error('daemon down');
      }),
      updateSearchEvent: vi.fn(async () => {
        order.push('update');
      }),
    } as unknown as DaemonClient;

    logSearchEvent(client, baseEvent);
    updateSearchEvent(client, 'evt-1', { resultCount: 0, latencyMs: 1 });

    await drain(50);
    expect(order).toEqual(['insert-failed', 'update']);
  });

  it('does not delay an update whose insert already settled', async () => {
    const { client, order } = makeDaemon(0);

    logSearchEvent(client, baseEvent);
    await drain(10); // insert lands first
    updateSearchEvent(client, 'evt-1', { resultCount: 1, latencyMs: 2 });

    await drain(10);
    expect(order).toEqual(['insert', 'update']);
  });

  it('sends an update for an unknown event id immediately', async () => {
    // No insert was made in this process (e.g. a retried/foreign id) — the
    // update must still go out rather than wait for a promise that never comes.
    const { client, order } = makeDaemon(0);

    updateSearchEvent(client, 'never-inserted', { resultCount: 0, latencyMs: 1 });

    await drain(10);
    expect(order).toEqual(['update']);
  });
});
