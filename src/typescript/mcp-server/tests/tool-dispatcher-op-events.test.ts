/**
 * search_events telemetry for the rules/scratchpad ops (field feedback
 * 2026-07-15): reported "rules timeouts" were invisible in telemetry because
 * these tools emitted no events at all — only the JSONL logs saved the
 * investigation. routeTool now logs the same start/finish records the read
 * tools emit, and an instrumentation failure must never break the call.
 */

import { describe, it, expect, vi } from 'vitest';
import { routeTool, countOpResults } from '../src/tool-dispatcher.js';
import type { ServerComponents } from '../src/server-factory.js';
import type { SessionState } from '../src/server-types.js';

function makeDaemon(): {
  logSearchEvent: ReturnType<typeof vi.fn>;
  updateSearchEvent: ReturnType<typeof vi.fn>;
  updateSearchEventEconomy: ReturnType<typeof vi.fn>;
} {
  return {
    logSearchEvent: vi.fn().mockResolvedValue(undefined),
    updateSearchEvent: vi.fn().mockResolvedValue(undefined),
    updateSearchEventEconomy: vi.fn().mockResolvedValue(undefined),
  };
}

function makeComponents(daemon: ReturnType<typeof makeDaemon>): ServerComponents {
  return {
    daemonClient: daemon,
    rulesTool: {
      execute: vi.fn().mockResolvedValue({ success: true, action: 'list', rules: [{}, {}, {}] }),
    },
    scratchpadTool: {
      execute: vi.fn().mockResolvedValue({ success: true, action: 'list', entries: [{}] }),
    },
  } as unknown as ServerComponents;
}

const session = {} as SessionState;

describe('routeTool — rules/scratchpad search_events telemetry', () => {
  it('logs start and finish records for a rules call', async () => {
    const daemon = makeDaemon();
    const result = await routeTool(
      'rules',
      { action: 'list', projectId: 'p-a' },
      makeComponents(daemon),
      session
    );

    expect((result as { success: boolean }).success).toBe(true);
    expect(daemon.logSearchEvent).toHaveBeenCalledTimes(1);
    const started = daemon.logSearchEvent.mock.calls[0]?.[0] as Record<string, unknown>;
    expect(started['op']).toBe('rules');
    expect(started['query_text']).toBe('list');
    expect(started['project_id']).toBe('p-a');

    expect(daemon.updateSearchEvent).toHaveBeenCalledTimes(1);
    const finished = daemon.updateSearchEvent.mock.calls[0]?.[0] as Record<string, unknown>;
    expect(finished['event_id']).toBe(started['id']);
    expect(finished['result_count']).toBe(3); // rules array length
    expect(daemon.updateSearchEventEconomy).toHaveBeenCalledTimes(1);
  });

  it('logs an error outcome when the tool throws, and rethrows', async () => {
    const daemon = makeDaemon();
    const components = makeComponents(daemon);
    (components.rulesTool.execute as ReturnType<typeof vi.fn>).mockRejectedValue(
      new Error('boom')
    );

    await expect(
      routeTool('rules', { action: 'add' }, components, session)
    ).rejects.toThrow('boom');

    const finished = daemon.updateSearchEvent.mock.calls[0]?.[0] as Record<string, unknown>;
    expect(finished['outcome']).toBe('error');
    expect(finished['result_count']).toBe(0);
  });

  it('instruments scratchpad with op "scratchpad"', async () => {
    const daemon = makeDaemon();
    await routeTool('scratchpad', { action: 'list' }, makeComponents(daemon), session);

    const started = daemon.logSearchEvent.mock.calls[0]?.[0] as Record<string, unknown>;
    expect(started['op']).toBe('scratchpad');
    const finished = daemon.updateSearchEvent.mock.calls[0]?.[0] as Record<string, unknown>;
    expect(finished['result_count']).toBe(1); // entries array length
  });
});

describe('countOpResults', () => {
  it('prefers list-shaped fields, falls back to success, then 0', () => {
    expect(countOpResults({ success: true, rules: [1, 2] })).toBe(2);
    expect(countOpResults({ success: true, entries: [] })).toBe(0);
    expect(countOpResults({ success: true, action: 'update' })).toBe(1);
    expect(countOpResults({ success: false })).toBe(0);
    expect(countOpResults(undefined)).toBe(0);
    expect(countOpResults('text')).toBe(0);
  });
});
