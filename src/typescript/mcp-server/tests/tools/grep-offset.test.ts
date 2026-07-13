/**
 * Grep offset pagination (field feedback): stable client-side windows over the
 * daemon's deterministic (file, line) ordering, with `next_offset` teaching the
 * continuation in-band. Grep was the only read surface with no paging — a
 * budget-truncated sweep (72 of 312 matches shipped) had no way to fetch the
 * tail short of raising every cap and refetching the whole set.
 */

import { describe, it, expect, vi } from 'vitest';
import { GrepTool } from '../../src/tools/grep.js';
import type { DaemonClient } from '../../src/clients/daemon-client.js';
import type { ProjectDetector } from '../../src/utils/project-detector.js';

interface DaemonRequest {
  max_results: number;
  tenant_id?: string;
  branch?: string;
}

/** Daemon emulator: `total` unique matches in deterministic order; honors the
 *  request's max_results cap and reports the true total + truncated flag,
 *  exactly like the FTS service. Every other method resolves as a no-op. */
function daemonWithMatches(
  total: number,
  contentFor: (i: number) => string = (i) => `match ${i}`
): { daemon: DaemonClient; textSearch: ReturnType<typeof vi.fn> } {
  const all = Array.from({ length: total }, (_, i) => ({
    file_path: `src/f${i}.ts`,
    line_number: 1,
    content: contentFor(i),
    context_before: [],
    context_after: [],
  }));
  const textSearch = vi.fn().mockImplementation((req: DaemonRequest) => {
    const cap = req.max_results > 0 ? req.max_results : all.length;
    return Promise.resolve({
      matches: all.slice(0, cap),
      total_matches: all.length,
      truncated: all.length > cap,
    });
  });
  const target: Record<string, unknown> = { textSearch };
  const daemon = new Proxy(target, {
    get(t: Record<string, unknown>, prop: string | symbol) {
      if (typeof prop === 'string' && prop in t) return t[prop];
      if (prop === 'then' || typeof prop === 'symbol') return undefined;
      return () => Promise.resolve(undefined);
    },
  }) as unknown as DaemonClient;
  return { daemon, textSearch };
}

const detector = {} as unknown as ProjectDetector; // scope:"all" never touches it

describe('GrepTool — offset pagination', () => {
  it('returns the [offset, offset+maxResults) window, fetching deep enough to cover it', async () => {
    const { daemon, textSearch } = daemonWithMatches(7);
    const tool = new GrepTool(daemon, detector);

    const res = await tool.grep({ pattern: 'match', scope: 'all', maxResults: 3, offset: 3 });

    expect(res.matches.map((m) => m.content)).toEqual(['match 3', 'match 4', 'match 5']);
    expect(res.next_offset).toBe(6);
    expect(res.total_matches).toBe(7);
    // The daemon request must reach offset + maxResults deep.
    expect((textSearch.mock.calls[0]![0] as DaemonRequest).max_results).toBe(6);
  });

  it('final page: no next_offset, total_matches reports the full deduped set', async () => {
    const { daemon } = daemonWithMatches(7);
    const tool = new GrepTool(daemon, detector);

    const res = await tool.grep({ pattern: 'match', scope: 'all', maxResults: 3, offset: 6 });

    expect(res.matches.map((m) => m.content)).toEqual(['match 6']);
    expect(res.next_offset).toBeUndefined();
    expect(res.truncated).toBe(false);
    expect(res.total_matches).toBe(7);
  });

  it('consecutive pages tile the match list without skips or duplicates', async () => {
    const { daemon } = daemonWithMatches(5);
    const tool = new GrepTool(daemon, detector);

    const page1 = await tool.grep({ pattern: 'match', scope: 'all', maxResults: 2 });
    const page2 = await tool.grep({
      pattern: 'match',
      scope: 'all',
      maxResults: 2,
      offset: page1.next_offset!,
    });
    const page3 = await tool.grep({
      pattern: 'match',
      scope: 'all',
      maxResults: 2,
      offset: page2.next_offset!,
    });

    const seen = [...page1.matches, ...page2.matches, ...page3.matches].map((m) => m.content);
    expect(seen).toEqual(['match 0', 'match 1', 'match 2', 'match 3', 'match 4']);
    expect(page3.next_offset).toBeUndefined();
  });

  it('offset at/beyond the end returns an empty page with a boundary message — no widen, no misdiagnosis', async () => {
    const { daemon, textSearch } = daemonWithMatches(2);
    const tool = new GrepTool(daemon, detector);

    const res = await tool.grep({ pattern: 'match', scope: 'all', maxResults: 5, offset: 10 });

    expect(res.matches).toHaveLength(0);
    expect(res.message).toMatch(/beyond the end/i);
    expect(res.next_offset).toBeUndefined();
    // No auto-widen retry fired for the empty PAGE (it is not an empty result).
    expect(textSearch).toHaveBeenCalledTimes(1);
  });

  it('a budget-dropped tail sets next_offset at the first dropped match, and the next page resumes there', async () => {
    const { daemon } = daemonWithMatches(4, () => 'q'.repeat(400));
    const tool = new GrepTool(daemon, detector);

    const page1 = await tool.grep({
      pattern: 'q',
      scope: 'all',
      maxResults: 4,
      maxResponseBytes: 900,
    });
    expect(page1.matches).toHaveLength(2); // 400 + 400 fits; the 3rd would exceed 900
    expect(page1.budget_truncated).toEqual({ dropped: 2 });
    expect(page1.next_offset).toBe(2);
    expect(page1.message).toMatch(/next_offset/);

    const page2 = await tool.grep({
      pattern: 'q',
      scope: 'all',
      maxResults: 4,
      maxResponseBytes: 900,
      offset: page1.next_offset!,
    });
    expect(page2.matches.map((m) => m.file)).toEqual(['src/f2.ts', 'src/f3.ts']);
    expect(page2.next_offset).toBeUndefined();
  });
});
