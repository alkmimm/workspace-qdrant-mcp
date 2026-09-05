/**
 * `grep` carries the read-side project echo on both of its success shapes
 * (matches and countOnly), labelled from the request's cwd provenance.
 */
import { describe, it, expect, vi } from 'vitest';
import { GrepTool } from '../../src/tools/grep.js';
import type { DaemonClient } from '../../src/clients/daemon-client.js';
import type { ProjectDetector } from '../../src/utils/project-detector.js';
import { runWithRequestContext } from '../../src/utils/request-context.js';

/** Daemon emulator: two matches; every other method resolves as a no-op. */
function daemon(): DaemonClient {
  const textSearch = vi.fn().mockResolvedValue({
    matches: [
      {
        file_path: 'src/a.ts',
        line_number: 1,
        content: 'match a',
        context_before: [],
        context_after: [],
      },
      {
        file_path: 'src/b.ts',
        line_number: 2,
        content: 'match b',
        context_before: [],
        context_after: [],
      },
    ],
    total_matches: 2,
    truncated: false,
  });
  const target: Record<string, unknown> = { textSearch };
  return new Proxy(target, {
    get(t: Record<string, unknown>, prop: string | symbol) {
      if (typeof prop === 'string' && prop in t) return t[prop];
      if (prop === 'then' || typeof prop === 'symbol') return undefined;
      return () => Promise.resolve(undefined);
    },
  }) as unknown as DaemonClient;
}

const detector = {
  getProjectInfo: vi.fn().mockResolvedValue({ projectId: 't1', projectPath: '/p' }),
} as unknown as ProjectDetector;

describe('grep read-side project echo', () => {
  it('names the cwd-resolved project on a match page', async () => {
    const tool = new GrepTool(daemon(), detector);
    const res = await tool.grep({ pattern: 'match', scope: 'project', branch: 'main' });
    expect(res.success).toBe(true);
    expect(res.project_id).toBe('t1');
    expect(res.project_path).toBe('/p');
    expect(res.project_source).toBe('cwd');
  });

  it('keeps the echo on countOnly and labels a sticky cwd', async () => {
    const tool = new GrepTool(daemon(), detector);
    const res = await runWithRequestContext({ hostCwd: '/p', cwdSource: 'sticky' }, () =>
      tool.grep({ pattern: 'match', scope: 'project', branch: 'main', countOnly: true })
    );
    expect(res.success).toBe(true);
    expect(res.matches).toBeUndefined();
    expect(res.total_matches).toBe(2);
    expect(res.project_id).toBe('t1');
    expect(res.project_source).toBe('sticky-cwd');
  });

  it('has no echo on a cross-project sweep (scope all)', async () => {
    const tool = new GrepTool(daemon(), {} as unknown as ProjectDetector);
    const res = await tool.grep({ pattern: 'match', scope: 'all' });
    expect(res.success).toBe(true);
    expect(res.project_id).toBeUndefined();
    expect(res.project_source).toBeUndefined();
  });
});
