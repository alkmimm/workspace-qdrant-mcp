/**
 * Effectiveness signals (spec 20 §1.2): followup/escalation classification
 * at the event-write boundary.
 *
 * Covers the pure tracker (windows, term overlap, session isolation, ref
 * matching, ring eviction) and the `logSearchEvent` integration (session_id
 * defaulting, op reclassification to 'followup', parent_event_id linkage).
 */

import { describe, it, expect } from 'vitest';
import {
  EffectivenessTracker,
  queryTokens,
  FOLLOWUP_WINDOW_MS,
  ESCALATION_WINDOW_MS,
} from '../../src/clients/effectiveness-signals.js';
import { logSearchEvent } from '../../src/clients/search-event-queries.js';
import type { DaemonClient } from '../../src/clients/daemon-client.js';

const T0 = 1_750_000_000_000;

describe('queryTokens', () => {
  it('lowercases and splits on non-identifier characters', () => {
    expect(queryTokens('WAL checkpoint file-size')).toEqual(
      new Set(['wal', 'checkpoint', 'file', 'size'])
    );
  });

  it('drops tokens shorter than 3 chars', () => {
    expect(queryTokens('go to fn db_pool')).toEqual(new Set(['db_pool']));
  });
});

describe('EffectivenessTracker followup classification', () => {
  it('returns undefined for the first query of a session', () => {
    const t = new EffectivenessTracker();
    expect(t.noteQuery('e1', 's1', 'queue lease recovery', T0)).toBeUndefined();
  });

  it('links a same-session overlapping query inside the window', () => {
    const t = new EffectivenessTracker();
    t.noteQuery('e1', 's1', 'queue lease recovery', T0);
    expect(t.noteQuery('e2', 's1', 'stale lease reset', T0 + 30_000)).toBe('e1');
  });

  it('does not link across sessions', () => {
    const t = new EffectivenessTracker();
    t.noteQuery('e1', 's1', 'queue lease recovery', T0);
    expect(t.noteQuery('e2', 's2', 'queue lease recovery', T0 + 1_000)).toBeUndefined();
  });

  it('does not link outside FOLLOWUP_WINDOW', () => {
    const t = new EffectivenessTracker();
    t.noteQuery('e1', 's1', 'queue lease recovery', T0);
    expect(
      t.noteQuery('e2', 's1', 'queue lease recovery', T0 + FOLLOWUP_WINDOW_MS + 1)
    ).toBeUndefined();
  });

  it('does not link disjoint queries', () => {
    const t = new EffectivenessTracker();
    t.noteQuery('e1', 's1', 'embedding provider fallback', T0);
    expect(t.noteQuery('e2', 's1', 'grpc channel timeout', T0 + 1_000)).toBeUndefined();
  });

  it('chains refinements to the most recent hop, not the root', () => {
    const t = new EffectivenessTracker();
    t.noteQuery('e1', 's1', 'branch prune deletes', T0);
    expect(t.noteQuery('e2', 's1', 'branch prune skip', T0 + 10_000)).toBe('e1');
    expect(t.noteQuery('e3', 's1', 'prune preserve guard', T0 + 20_000)).toBe('e2');
  });
});

describe('EffectivenessTracker escalation origin', () => {
  it('finds the search that returned the retrieved ref', () => {
    const t = new EffectivenessTracker();
    t.noteQuery('e1', 's1', 'write actor exec tracking', T0);
    t.noteHits('e1', ['doc-123', '/repo/src/a.rs', 'src/a.rs']);
    expect(t.findOrigin('s1', ['doc-123'], T0 + 60_000)).toBe('e1');
    expect(t.findOrigin('s1', ['src/a.rs'], T0 + 60_000)).toBe('e1');
  });

  it('returns undefined for unknown refs, other sessions, or outside the window', () => {
    const t = new EffectivenessTracker();
    t.noteQuery('e1', 's1', 'write actor exec tracking', T0);
    t.noteHits('e1', ['doc-123']);
    expect(t.findOrigin('s1', ['doc-999'], T0 + 1_000)).toBeUndefined();
    expect(t.findOrigin('s2', ['doc-123'], T0 + 1_000)).toBeUndefined();
    expect(t.findOrigin('s1', ['doc-123'], T0 + ESCALATION_WINDOW_MS + 1)).toBeUndefined();
    expect(t.findOrigin('s1', [], T0 + 1_000)).toBeUndefined();
  });

  it('prefers the most recent search that returned the ref', () => {
    const t = new EffectivenessTracker();
    t.noteQuery('e1', 's1', 'alpha module parser', T0);
    t.noteHits('e1', ['doc-1']);
    t.noteQuery('e2', 's1', 'beta module lexer', T0 + 5_000);
    t.noteHits('e2', ['doc-1']);
    expect(t.findOrigin('s1', ['doc-1'], T0 + 10_000)).toBe('e2');
  });

  it('evicts the oldest entries past the ring capacity', () => {
    const t = new EffectivenessTracker();
    t.noteQuery('e0', 's1', 'first ever query', T0);
    t.noteHits('e0', ['doc-first']);
    for (let i = 1; i <= 100; i++) {
      // Disjoint tokens so nothing classifies as followup.
      t.noteQuery(`e${i}`, 's1', `zz${i}xx`, T0 + i);
    }
    // e0 fell out of the 100-entry ring: no origin found despite the window.
    expect(t.findOrigin('s1', ['doc-first'], T0 + 200)).toBeUndefined();
  });
});

describe('logSearchEvent effectiveness integration', () => {
  interface CapturedRequest {
    id: string;
    op: string;
    session_id?: string;
    parent_event_id?: string;
  }

  function captureClient(sink: CapturedRequest[]): DaemonClient {
    return {
      logSearchEvent: (req: CapturedRequest): Promise<void> => {
        sink.push(req);
        return Promise.resolve();
      },
    } as unknown as DaemonClient;
  }

  it('defaults session_id when the caller passes none', () => {
    const sink: CapturedRequest[] = [];
    logSearchEvent(captureClient(sink), {
      id: 'evt-session-default',
      actor: 'claude',
      tool: 'mcp_qdrant',
      op: 'list',
    });
    expect(sink).toHaveLength(1);
    expect(sink[0]!.session_id).toBeTruthy();
  });

  it('links an overlapping repeat search via parent_event_id and PRESERVES its op', () => {
    const sink: CapturedRequest[] = [];
    const client = captureClient(sink);
    // Distinctive tokens: the module-level tracker is shared across tests.
    logSearchEvent(client, {
      id: 'evt-fu-origin',
      actor: 'claude',
      tool: 'mcp_qdrant',
      op: 'search',
      queryText: 'qqfollowupzz integration probe',
    });
    logSearchEvent(client, {
      id: 'evt-fu-repeat',
      actor: 'claude',
      tool: 'mcp_qdrant',
      op: 'search',
      queryText: 'qqfollowupzz probe again',
    });
    expect(sink[0]!.op).toBe('search');
    // op is event identity — op-keyed analytics need the full census. The
    // followup signal travels on the parent link alone (view derives it).
    expect(sink[1]!.op).toBe('search');
    expect(sink[1]!.parent_event_id).toBe('evt-fu-origin');
  });

  it('does not link queries that share only stopwords or short common tokens', () => {
    const sink: CapturedRequest[] = [];
    const client = captureClient(sink);
    logSearchEvent(client, {
      id: 'evt-stop-a',
      actor: 'claude',
      tool: 'mcp_qdrant',
      op: 'search',
      queryText: 'where does the daemon store the qqstopzza',
    });
    logSearchEvent(client, {
      id: 'evt-stop-b',
      actor: 'claude',
      tool: 'mcp_qdrant',
      op: 'search',
      queryText: 'where is the qqstopzzb parsed for this',
    });
    expect(sink[1]!.parent_event_id).toBeUndefined();
  });

  it('ignores non-claude actors entirely (no link, no tracker pollution)', () => {
    const sink: CapturedRequest[] = [];
    const client = captureClient(sink);
    logSearchEvent(client, {
      id: 'evt-bench-a',
      actor: 'benchmark',
      tool: 'mcp_qdrant',
      op: 'search',
      queryText: 'qqbenchzz sweep case alpha',
    });
    logSearchEvent(client, {
      id: 'evt-bench-b',
      actor: 'benchmark',
      tool: 'mcp_qdrant',
      op: 'search',
      queryText: 'qqbenchzz sweep case beta',
    });
    // Benchmark traffic is never classified...
    expect(sink[1]!.parent_event_id).toBeUndefined();
    // ...and never recorded: an agent query overlapping it finds no origin.
    logSearchEvent(client, {
      id: 'evt-bench-claude',
      actor: 'claude',
      tool: 'mcp_qdrant',
      op: 'search',
      queryText: 'qqbenchzz sweep case gamma',
    });
    expect(sink[2]!.parent_event_id).toBeUndefined();
  });

  it('gives grep the parent link but keeps op=grep', () => {
    const sink: CapturedRequest[] = [];
    const client = captureClient(sink);
    logSearchEvent(client, {
      id: 'evt-grep-origin',
      actor: 'claude',
      tool: 'mcp_qdrant',
      op: 'search',
      queryText: 'qqgrepzz lineage probe',
    });
    logSearchEvent(client, {
      id: 'evt-grep-next',
      actor: 'claude',
      tool: 'mcp_qdrant',
      op: 'grep',
      queryText: 'qqgrepzz',
    });
    expect(sink[1]!.op).toBe('grep');
    expect(sink[1]!.parent_event_id).toBe('evt-grep-origin');
  });

  it('never overrides an explicit caller parentEventId', () => {
    const sink: CapturedRequest[] = [];
    const client = captureClient(sink);
    logSearchEvent(client, {
      id: 'evt-explicit-origin',
      actor: 'claude',
      tool: 'mcp_qdrant',
      op: 'search',
      queryText: 'qqexplicitzz parent probe',
    });
    logSearchEvent(client, {
      id: 'evt-explicit-child',
      actor: 'claude',
      tool: 'mcp_qdrant',
      op: 'search',
      queryText: 'qqexplicitzz parent probe again',
      parentEventId: 'evt-hand-picked',
    });
    expect(sink[1]!.parent_event_id).toBe('evt-hand-picked');
  });
});
