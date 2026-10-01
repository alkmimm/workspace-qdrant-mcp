/**
 * Cross-mode field parity: a keyword/exact hit must carry the same metadata
 * field NAMES a semantic hit carries.
 *
 * It did not. A semantic hit arrived with `relative_path`, `language` and
 * `branch`; the same file found with `exact: true` arrived with only
 * `file_path`, `line_number` and the context fields. So an agent that read
 * `relative_path` — the documented way to get a repo-relative path — silently
 * got `undefined` the moment it switched modes, with no error to notice. The
 * daemon already knows both fields; they come from the one `tracked_files`
 * lookup the surface was already paying for to annotate `is_test`.
 */

import { describe, it, expect, vi } from 'vitest';
import { searchExact } from '../../src/tools/search-exact.js';
import type { DaemonClient } from '../../src/clients/daemon-client.js';
import type { SqliteStateManager } from '../../src/clients/sqlite-state-manager.js';
import type { ProjectDetector } from '../../src/utils/project-detector.js';
import type { SearchOptions } from '../../src/tools/search-types.js';
import type { TrackedFileAnnotation } from '../../src/clients/tracked-files-queries/index.js';

vi.mock('../../src/utils/git-utils.js', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../src/utils/git-utils.js')>();
  return { ...actual, getCurrentBranch: vi.fn().mockReturnValue('main') };
});

const ABS = '/repo/src/rust/daemon/core/src/text_search/dedup.rs';
const REL = 'src/rust/daemon/core/src/text_search/dedup.rs';

function daemon(): DaemonClient {
  return {
    textSearch: vi.fn().mockResolvedValue({
      matches: [
        {
          file_path: ABS,
          line_number: 42,
          content: 'pub fn accept(&mut self) -> bool {',
          context_before: [],
          context_after: [],
          branch: 'main',
        },
      ],
      total_matches: 1,
      truncated: false,
    }),
    logSearchEvent: vi.fn().mockResolvedValue(undefined),
    updateSearchEvent: vi.fn().mockResolvedValue(undefined),
    updateSearchEventEconomy: vi.fn().mockResolvedValue(undefined),
  } as unknown as DaemonClient;
}

function stateManager(annotation: TrackedFileAnnotation | undefined): SqliteStateManager {
  const annotations = new Map<string, TrackedFileAnnotation>();
  if (annotation) annotations.set(ABS, annotation);
  return {
    logSearchEvent: vi.fn(),
    updateSearchEvent: vi.fn(),
    updateSearchEventEconomy: vi.fn(),
    getProjectById: vi.fn().mockReturnValue({ data: { project_path: '/repo' } }),
    getWatchFolderIdByTenantId: vi.fn().mockReturnValue('watch-a'),
    getBaseBranch: vi.fn().mockReturnValue(null),
    getFileAnnotationsByFilePaths: vi.fn().mockReturnValue(annotations),
  } as unknown as SqliteStateManager;
}

const detector = {
  findProjectRoot: vi.fn().mockReturnValue('/repo'),
  getProjectInfo: vi.fn().mockResolvedValue({ projectId: 'tenant-a', projectPath: '/repo' }),
} as unknown as ProjectDetector;

const qdrant = { scroll: vi.fn().mockResolvedValue({ points: [] }) } as unknown as Parameters<
  typeof searchExact
>[0];

function options(): SearchOptions {
  return { query: 'accept', scope: 'project', projectId: 'tenant-a', exact: true, cwd: '/repo' };
}

describe('exact search — metadata parity with the semantic lane', () => {
  it('carries relative_path, the field name the semantic lane uses', async () => {
    const res = await searchExact(
      qdrant,
      daemon(),
      stateManager({ relativePath: REL, isTest: false, language: 'rust' }),
      detector,
      options()
    );

    expect(res.results[0]!.metadata['relative_path']).toBe(REL);
  });

  it('carries language', async () => {
    const res = await searchExact(
      qdrant,
      daemon(),
      stateManager({ relativePath: REL, isTest: false, language: 'rust' }),
      detector,
      options()
    );

    expect(res.results[0]!.metadata['language']).toBe('rust');
  });

  it('keeps file_path and line_number — the exact lane is additive, not renamed', async () => {
    const res = await searchExact(
      qdrant,
      daemon(),
      stateManager({ relativePath: REL, isTest: false, language: 'rust' }),
      detector,
      options()
    );

    expect(res.results[0]!.metadata['file_path']).toBe(ABS);
    expect(res.results[0]!.metadata['line_number']).toBe(42);
  });

  it('still marks is_test from the same single lookup', async () => {
    const res = await searchExact(
      qdrant,
      daemon(),
      stateManager({ relativePath: REL, isTest: true, language: 'rust' }),
      detector,
      options()
    );

    expect(res.results[0]!.is_test).toBe(true);
  });

  it('omits language when the daemon does not know it, rather than inventing one', async () => {
    const res = await searchExact(
      qdrant,
      daemon(),
      stateManager({ relativePath: REL, isTest: false, language: null }),
      detector,
      options()
    );

    expect(res.results[0]!.metadata['relative_path']).toBe(REL);
    expect(res.results[0]!.metadata['language']).toBeUndefined();
  });

  it('returns the hit unharmed when no annotation covers the path', async () => {
    const res = await searchExact(qdrant, daemon(), stateManager(undefined), detector, options());

    expect(res.results).toHaveLength(1);
    expect(res.results[0]!.metadata['file_path']).toBe(ABS);
    expect(res.results[0]!.metadata['relative_path']).toBeUndefined();
  });
});
