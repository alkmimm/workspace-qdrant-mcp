/**
 * `list` format:"summary" must aggregate over EVERY matching file, not the
 * first page.
 *
 * Measured on this repo (1,924 indexed files, default limit 200): the summary
 * listed .github/ assets/ benchmarks/ docker/ docs/ Formula/ and stopped —
 * `src/` (1,635 files) never appeared, stats.languages lacked rust/typescript,
 * and limit:500 / depth:2 / path:"src" only moved the window. The layout
 * overview the tool description points agents at first was wrong, not partial.
 */
import { describe, it, expect, vi } from 'vitest';
import { ListFilesTool } from '../../src/tools/list-files/index.js';
import type { SqliteStateManager } from '../../src/clients/sqlite-state-manager.js';
import type { ProjectDetector } from '../../src/utils/project-detector.js';
import type { ListTrackedFilesOptions } from '../../src/clients/tracked-files-queries/index.js';

function makeFile(path: string, ext: string, lang: string) {
  return { relativePath: path, fileType: 'code', language: lang, extension: ext, isTest: false };
}

// 700 files in relative-path order: alpha/ (300, sorts first — fills the whole
// default page), then src/rust (250) and src/typescript (150), which a
// page-scoped summary never reached.
const FILES = [
  ...Array.from({ length: 300 }, (_, i) =>
    makeFile(`alpha/doc_${String(i + 1).padStart(4, '0')}.md`, 'md', 'markdown')
  ),
  ...Array.from({ length: 250 }, (_, i) =>
    makeFile(`src/rust/mod_${String(i + 1).padStart(4, '0')}.rs`, 'rs', 'rust')
  ),
  ...Array.from({ length: 150 }, (_, i) =>
    makeFile(`src/typescript/file_${String(i + 1).padStart(4, '0')}.ts`, 'ts', 'typescript')
  ),
];

function makeStateManager(calls: ListTrackedFilesOptions[]): SqliteStateManager {
  return {
    getWatchFolderIdByTenantId: vi.fn().mockReturnValue('watch-1'),
    getProjectById: vi.fn().mockReturnValue({ data: { project_path: '/proj' } }),
    getBaseBranch: vi.fn().mockReturnValue(null),
    listTrackedFiles: vi.fn().mockImplementation((o: ListTrackedFilesOptions) => {
      calls.push(o);
      const limit = o.limit ?? 500;
      let data = FILES;
      const after = o.afterPath;
      if (after !== undefined) {
        const idx = data.findIndex((f) => f.relativePath > after);
        data = idx === -1 ? [] : data.slice(idx);
      }
      return { status: 'ok' as const, data: data.slice(0, limit) };
    }),
    countTrackedFiles: vi.fn().mockReturnValue(FILES.length),
    listSubmodules: vi.fn().mockReturnValue({ data: [] }),
    listProjectComponents: vi.fn().mockReturnValue({ status: 'ok', data: [] }),
    logSearchEvent: vi.fn(),
    updateSearchEvent: vi.fn(),
    updateSearchEventEconomy: vi.fn(),
  } as unknown as SqliteStateManager;
}

function makeDetector(): ProjectDetector {
  return {
    getProjectInfo: vi
      .fn()
      .mockResolvedValue({ projectId: 'proj-1', projectPath: '/proj', name: 'proj' }),
  } as unknown as ProjectDetector;
}

describe('list format:summary aggregates over ALL matching files', () => {
  it('shows every top-level directory at the default limit, with full counts and no cursor', async () => {
    const calls: ListTrackedFilesOptions[] = [];
    const tool = new ListFilesTool(makeStateManager(calls), makeDetector());
    const res = await tool.list({ format: 'summary', depth: 2, branch: 'main' });

    expect(res.success).toBe(true);
    expect(res.listing).toContain('alpha/ (300 files');
    expect(res.listing).toContain('src/ (400 files');
    expect(res.listing).toContain('rust/ (250 files');
    expect(res.listing).toContain('typescript/ (150 files');
    expect(res.next_token).toBeUndefined();
    expect(res.stats.truncated).toBe(false);
    expect(res.stats.files).toBe(700);
    expect(res.stats.totalMatching).toBe(700);
    expect(res.stats.languages).toEqual(['markdown', 'rust', 'typescript']);

    // One full scan (path-only rows), not the default 200-file page.
    expect(calls).toHaveLength(1);
    expect(calls[0]?.limit).toBeGreaterThan(FILES.length);
    expect(calls[0]?.afterPath).toBeUndefined();
  });

  it('ignores a leftover cursor: a summary is one shot over the whole set', async () => {
    const calls: ListTrackedFilesOptions[] = [];
    const tool = new ListFilesTool(makeStateManager(calls), makeDetector());
    const cursor = Buffer.from('alpha/doc_0300.md').toString('base64');
    const res = await tool.list({ format: 'summary', depth: 1, branch: 'main', cursor });
    expect(res.success).toBe(true);
    expect(res.listing).toContain('alpha/ (300 files');
    expect(calls[0]?.afterPath).toBeUndefined();
  });

  it('caps rendered directory entries at limit and says how many were cut', async () => {
    const tool = new ListFilesTool(makeStateManager([]), makeDetector());
    const res = await tool.list({ format: 'summary', depth: 3, limit: 1, branch: 'main' });
    expect(res.success).toBe(true);
    expect(res.stats.truncated).toBe(true);
    expect(res.listing).toMatch(/3 more directories not shown/);
    // Counts still come from the whole set even when the rendering is capped.
    expect(res.stats.files).toBe(700);
  });

  it('carries the read-side project echo', async () => {
    const tool = new ListFilesTool(makeStateManager([]), makeDetector());
    const res = await tool.list({ format: 'summary', branch: 'main' });
    expect(res.project_id).toBe('proj-1');
    expect(res.project_source).toBe('cwd');
    // projectPath was already a field; the echo does not duplicate it.
    expect(res.projectPath).toBe('/proj');
  });

  it('keeps tree paged (the split is deliberate: tree/flat enumerate, summary aggregates)', async () => {
    const tool = new ListFilesTool(makeStateManager([]), makeDetector());
    const res = await tool.list({ format: 'tree', depth: 1, branch: 'main' });
    expect(res.success).toBe(true);
    expect(res.stats.files).toBe(200);
    expect(res.next_token).toBeDefined();
    expect(res.project_id).toBe('proj-1');
  });
});
