/**
 * Regression (emnify-sms-sender, 2026-10-03): a grep scoped to a feature branch
 * must not fill in, from the trunk, files the branch deleted or changed.
 *
 * `grep "^'use server'" branch:"feat/fase-5-sincronizacao"` returned
 * `app/lib/actions/endpoint.ts` and `Message.ts` (deleted on the branch) and the
 * trunk's `app/(user)/message/page.tsx` line 1, whose branch version starts with
 * an import — a security sweep ("every server action calls currentActor")
 * reported phantom files.
 */

import { describe, it, expect, vi } from 'vitest';
import { GrepTool } from '../../src/tools/grep.js';
import type { DaemonClient } from '../../src/clients/daemon-client.js';
import type { SqliteStateManager } from '../../src/clients/sqlite-state-manager.js';
import type { ProjectDetector } from '../../src/utils/project-detector.js';

const BRANCH = 'feat/fase-5-sincronizacao';

vi.mock('../../src/utils/git-utils.js', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../src/utils/git-utils.js')>();
  return { ...actual, getCurrentBranch: vi.fn().mockReturnValue('feat/fase-5-sincronizacao') };
});

// git: what differs between the develop and feat/fase-5 tips.
vi.mock('../../src/utils/git-branch-diff.js', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../src/utils/git-branch-diff.js')>();
  return {
    ...actual,
    getPathsChangedBetween: vi
      .fn()
      .mockResolvedValue(
        new Set([
          'app/lib/actions/endpoint.ts',
          'app/lib/actions/Message.ts',
          'app/(user)/message/page.tsx',
          'app/lib/actions/sim-cards.ts',
        ])
      ),
  };
});

interface Row {
  file: string;
  line: number;
  content: string;
}

function daemonWithBranches(byBranch: Record<string, Row[]>): DaemonClient {
  const textSearch = vi.fn().mockImplementation((req: { branch?: string; max_results: number }) => {
    const rows = byBranch[req.branch ?? '*'] ?? [];
    return Promise.resolve({
      matches: rows.map((r) => ({
        file_path: r.file,
        line_number: r.line,
        content: r.content,
        context_before: [],
        context_after: [],
        branch: req.branch,
      })),
      total_matches: rows.length,
      truncated: false,
    });
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

function stateManager(trackedOnBranch: string[]): SqliteStateManager {
  return {
    logSearchEvent: vi.fn(),
    updateSearchEvent: vi.fn(),
    updateSearchEventEconomy: vi.fn(),
    getProjectById: vi.fn().mockReturnValue({ data: { project_path: '/repo' } }),
    getWatchFolderIdByTenantId: vi.fn().mockReturnValue('watch-a'),
    getBaseBranch: vi.fn().mockReturnValue('develop'),
    getIsTestByFilePaths: vi.fn().mockReturnValue(new Map()),
    getFileAnnotationsByFilePaths: vi.fn().mockReturnValue(new Map()),
    getPathsTrackedOnBranch: vi
      .fn()
      .mockImplementation(
        (_wf: string, _b: string, paths: readonly string[]) =>
          new Set(paths.filter((p) => trackedOnBranch.includes(p)))
      ),
  } as unknown as SqliteStateManager;
}

const detector = {
  findProjectRoot: vi.fn().mockReturnValue('/repo'),
  getProjectInfo: vi.fn().mockResolvedValue({ projectId: 'tenant-a', projectPath: '/repo' }),
} as unknown as ProjectDetector;

const useServer = (file: string): Row => ({ file, line: 1, content: "'use server';" });

describe('grep trunk fill-in on a feature branch', () => {
  it('never resurrects a deleted file nor serves the trunk copy of a changed one', async () => {
    const daemon = daemonWithBranches({
      [BRANCH]: [useServer('/repo/app/lib/actions/sim-cards.ts')],
      develop: [
        useServer('/repo/app/lib/actions/endpoint.ts'), // deleted on the branch
        useServer('/repo/app/lib/actions/Message.ts'), // deleted on the branch
        useServer('/repo/app/(user)/message/page.tsx'), // changed: branch starts with an import
        useServer('/repo/app/lib/actions/users.ts'), // unchanged on the branch
      ],
    });
    const tool = new GrepTool(
      daemon,
      detector,
      stateManager(['app/(user)/message/page.tsx', 'app/lib/actions/sim-cards.ts'])
    );

    const res = await tool.grep({ pattern: "^'use server'", regex: true, cwd: '/repo' });

    expect(res.matches.map((m) => m.file)).toEqual([
      '/repo/app/lib/actions/sim-cards.ts',
      '/repo/app/lib/actions/users.ts',
    ]);
  });

  it('does not bring a deleted file back through the empty-result widen to "*"', async () => {
    // The only match lives in a file the branch deleted: the fill-in refuses
    // it, the result is empty, and the auto-widen must not resurrect it.
    const deletedOnly = [useServer('/repo/app/lib/actions/endpoint.ts')];
    const daemon = daemonWithBranches({
      [BRANCH]: [],
      develop: deletedOnly,
      '*': deletedOnly,
    });
    const tool = new GrepTool(daemon, detector, stateManager([]));

    const res = await tool.grep({ pattern: "^'use server'", regex: true, cwd: '/repo' });

    expect(res.matches).toEqual([]);
    expect(res.message).toContain('withheld');
  });

  it('still fills in a file the branch holds no generation of and git says is unchanged', async () => {
    const daemon = daemonWithBranches({
      [BRANCH]: [],
      develop: [useServer('/repo/app/lib/actions/users.ts')],
    });
    const tool = new GrepTool(daemon, detector, stateManager([]));

    const res = await tool.grep({ pattern: "^'use server'", regex: true, cwd: '/repo' });

    expect(res.matches.map((m) => m.file)).toEqual(['/repo/app/lib/actions/users.ts']);
  });
});
