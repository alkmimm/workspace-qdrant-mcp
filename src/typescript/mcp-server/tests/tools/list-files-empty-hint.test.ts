/**
 * A `list` that matches nothing must say so.
 *
 * Before this the tool answered a zero-row filter with `listing: ""`,
 * `files: 0` and no other signal — identical to an unindexed project, whatever
 * the cause. That shape is what let a braced `pattern` fail silently for as
 * long as it did: `**\/*.{rs,ts}` returned zero files while the same glob
 * matched 28 times through `grep` (2026-09-05).
 */

import { describe, it, expect, vi } from 'vitest';

import { ListFilesTool } from '../../src/tools/list-files/index.js';
import type { SqliteStateManager } from '../../src/clients/sqlite-state-manager.js';
import type { ProjectDetector } from '../../src/utils/project-detector.js';

function makeFile(path: string, ext: string, lang: string) {
  return { relativePath: path, fileType: 'code', language: lang, extension: ext, isTest: false };
}

function makeStateManager(files: ReturnType<typeof makeFile>[]): SqliteStateManager {
  return {
    getWatchFolderIdByTenantId: vi.fn().mockReturnValue('watch-1'),
    getProjectById: vi.fn().mockReturnValue({ data: { project_path: '/proj' } }),
    getBaseBranch: vi.fn().mockReturnValue(null),
    listTrackedFiles: vi.fn().mockReturnValue({ status: 'ok' as const, data: files }),
    countTrackedFiles: vi.fn().mockReturnValue(files.length),
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

function makeTool(files: ReturnType<typeof makeFile>[]): ListFilesTool {
  return new ListFilesTool(makeStateManager(files), makeDetector());
}

describe('list empty-result hint', () => {
  it('names the active filters and the glob syntax when a filter selects nothing', async () => {
    const res = await makeTool([]).list({ pattern: '**/*.{rs,ts}', branch: 'main' });

    expect(res.success).toBe(true);
    expect(res.stats.files).toBe(0);
    expect(res.hint).toBeDefined();
    expect(res.hint).toContain('pattern:"**/*.{rs,ts}"');
    // The syntax the caller most likely got wrong, spelled out.
    expect(res.hint).toContain('{a,b}');
    expect(res.hint).toContain('branch:"*"');
  });

  it('lists every narrowing filter that was applied', async () => {
    const res = await makeTool([]).list({
      path: 'src',
      pattern: '*.rs',
      pathExclude: 'vendor',
      component: 'daemon.core',
      fileType: 'code',
      language: 'rust',
      extension: 'rs',
      includeTests: false,
      branch: 'main',
    });

    for (const fragment of [
      'path:"src"',
      'pattern:"*.rs"',
      'pathExclude:"vendor"',
      'component:"daemon.core"',
      'fileType:"code"',
      'language:"rust"',
      'extension:"rs"',
      'includeTests:false',
    ]) {
      expect(res.hint, fragment).toContain(fragment);
    }
  });

  it('reports an unfiltered empty listing as an empty index, not a filter mismatch', async () => {
    const res = await makeTool([]).list({ branch: 'main' });

    expect(res.hint).toContain('No indexed files');
    // Branch is a DEFAULT, not a caller filter: counting it as one would mask
    // this verdict behind the filter-mismatch wording.
    expect(res.hint).not.toContain('filters:');
  });

  it('attaches the hint on the summary path too', async () => {
    const res = await makeTool([]).list({ format: 'summary', pattern: 'nope', branch: 'main' });

    expect(res.format).toBe('summary');
    expect(res.hint).toContain('pattern:"nope"');
  });

  it('says nothing when the listing has results', async () => {
    const res = await makeTool([makeFile('src/main.rs', 'rs', 'rust')]).list({
      pattern: '**/*.rs',
      branch: 'main',
    });

    expect(res.stats.files).toBe(1);
    expect(res.hint).toBeUndefined();
  });
});
