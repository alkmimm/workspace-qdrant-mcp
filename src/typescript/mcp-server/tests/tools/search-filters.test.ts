/**
 * Tests for search filter construction (search-filters.ts)
 */

import { describe, it, expect } from 'vitest';
import {
  buildFilter,
  extractGlobPrefix,
  determineCollections,
} from '../../src/tools/search-filters.js';
import type { FilterParams } from '../../src/tools/search-types.js';
import {
  PROJECTS_COLLECTION,
  LIBRARIES_COLLECTION,
  SCRATCHPAD_COLLECTION,
} from '../../src/tools/search-types.js';

/** Build a minimal FilterParams with sensible defaults */
function makeParams(overrides: Partial<FilterParams> = {}): FilterParams {
  return {
    collection: PROJECTS_COLLECTION,
    scope: 'project',
    projectId: undefined,
    branch: undefined,
    fallbackBranch: undefined,
    fileType: undefined,
    libraryName: undefined,
    includeDeleted: false,
    tag: undefined,
    tags: undefined,
    pathGlob: undefined,
    component: undefined,
    basePoints: undefined,
    ...overrides,
  };
}

describe('buildFilter — component filter', () => {
  it('should add component_id filter with exact + prefix matching', () => {
    const filter = buildFilter(makeParams({ component: 'daemon' }));
    expect(filter).not.toBeNull();
    const must = filter!.must as Record<string, unknown>[];
    expect(must).toBeDefined();

    const componentCondition = must.find(
      (c) => (c as Record<string, unknown>).should !== undefined
    ) as Record<string, unknown> | undefined;
    expect(componentCondition).toBeDefined();

    const should = componentCondition!.should as Record<string, unknown>[];
    expect(should).toHaveLength(2);

    // Exact match
    expect(should[0]).toEqual({ key: 'component_id', match: { value: 'daemon' } });
    // Prefix match (component + ".")
    expect(should[1]).toEqual({ key: 'component_id', match: { text: 'daemon.' } });
  });

  it('should handle dot-separated component IDs', () => {
    const filter = buildFilter(makeParams({ component: 'daemon.core' }));
    const must = filter!.must as Record<string, unknown>[];
    const componentCondition = must.find(
      (c) => (c as Record<string, unknown>).should !== undefined
    ) as Record<string, unknown>;

    const should = componentCondition.should as Record<string, unknown>[];
    expect(should[0]).toEqual({ key: 'component_id', match: { value: 'daemon.core' } });
    expect(should[1]).toEqual({ key: 'component_id', match: { text: 'daemon.core.' } });
  });

  it('should not add component filter when component is undefined', () => {
    const filter = buildFilter(makeParams({ component: undefined }));
    // No conditions at all → null
    expect(filter).toBeNull();
  });

  it('should combine component filter with other filters', () => {
    const filter = buildFilter(
      makeParams({
        component: 'mcp-server',
        projectId: 'proj-123',
        scope: 'project',
        branch: 'main',
      })
    );
    expect(filter).not.toBeNull();
    const must = filter!.must as Record<string, unknown>[];
    // Should have: tenant_id, branch, component (at minimum)
    expect(must.length).toBeGreaterThanOrEqual(3);

    // Component condition present
    const componentCondition = must.find(
      (c) => (c as Record<string, unknown>).should !== undefined
    );
    expect(componentCondition).toBeDefined();

    // Branch condition present
    const branchCondition = must.find((c) => (c as Record<string, unknown>).key === 'branch');
    expect(branchCondition).toBeDefined();
  });

  it('includes the base branch as a fallback for feature-branch project searches', () => {
    const filter = buildFilter(makeParams({ branch: 'feature/current', fallbackBranch: 'main' }));
    expect(filter).not.toBeNull();
    const must = filter!.must as Record<string, unknown>[];
    expect(must).toContainEqual({
      should: [
        { key: 'branch', match: { value: 'feature/current' } },
        { key: 'branch', match: { value: 'main' } },
      ],
    });
  });

  it('does not add a branch filter for branch="*" even when fallbackBranch is present', () => {
    const filter = buildFilter(makeParams({ branch: '*', fallbackBranch: 'main' }));
    expect(filter).toBeNull();
  });
});

describe('buildFilter — branch scoping is projects-only', () => {
  it('drops the branch filter for the scratchpad collection (notes are pinned to "main")', () => {
    const filter = buildFilter(
      makeParams({
        collection: SCRATCHPAD_COLLECTION,
        projectId: 'proj-123',
        branch: 'feature/current',
      })
    );
    expect(filter).not.toBeNull();
    const must = filter!.must as Record<string, unknown>[];
    // Tenant scoping stays; the session-branch condition must NOT be applied —
    // it silently emptied every off-main semantic scratchpad search.
    expect(must).toContainEqual({ key: 'tenant_id', match: { value: 'proj-123' } });
    expect(JSON.stringify(must)).not.toContain('"branch"');
  });

  it('drops the branch filter (and its fallback widening) for libraries', () => {
    const filter = buildFilter(
      makeParams({
        collection: LIBRARIES_COLLECTION,
        branch: 'feature/current',
        fallbackBranch: 'main',
      })
    );
    expect(JSON.stringify(filter)).not.toContain('"branch"');
  });

  it('keeps the branch filter for the projects collection', () => {
    const filter = buildFilter(makeParams({ branch: 'feature/current' }));
    expect(filter).not.toBeNull();
    const must = filter!.must as Record<string, unknown>[];
    expect(must).toContainEqual({ key: 'branch', match: { value: 'feature/current' } });
  });
});

describe('buildFilter — existing filters preserved', () => {
  it('should match fileType against both legacy file_type and document_type payloads', () => {
    const filter = buildFilter(makeParams({ fileType: 'code' }));
    expect(filter).not.toBeNull();
    const must = filter!.must as Record<string, unknown>[];
    expect(must).toContainEqual({
      should: [
        { key: 'file_type', match: { value: 'code' } },
        { key: 'document_type', match: { value: 'code' } },
      ],
    });
  });

  it('should add tag filter', () => {
    const filter = buildFilter(makeParams({ tag: 'error-handling' }));
    expect(filter).not.toBeNull();
    const must = filter!.must as Record<string, unknown>[];
    const tagCondition = must.find((c) => (c as Record<string, unknown>).key === 'concept_tags');
    expect(tagCondition).toBeDefined();
  });

  it('should add multi-tag filter as should condition', () => {
    const filter = buildFilter(
      makeParams({
        tags: ['async', 'error-handling'],
      })
    );
    expect(filter).not.toBeNull();
    const must = filter!.must as Record<string, unknown>[];
    const shouldCondition = must.find((c) => (c as Record<string, unknown>).should !== undefined);
    expect(shouldCondition).toBeDefined();
    const should = (shouldCondition as Record<string, unknown>).should as unknown[];
    expect(should).toHaveLength(2);
  });

  it('should add pathGlob prefix filter', () => {
    const filter = buildFilter(makeParams({ pathGlob: 'src/tools/**/*.ts' }));
    expect(filter).not.toBeNull();
    const must = filter!.must as Record<string, unknown>[];
    const pathCondition = must.find((c) => (c as Record<string, unknown>).key === 'file_path');
    expect(pathCondition).toBeDefined();
  });
  it('should add pathGlob text hint for leading glob filters', () => {
    const filter = buildFilter(makeParams({ pathGlob: '**/integrator-routes/**' }));
    expect(filter).not.toBeNull();
    const must = filter!.must as Record<string, unknown>[];
    const pathCondition = must.find((c) => (c as Record<string, unknown>).key === 'file_path');
    expect(pathCondition).toEqual({ key: 'file_path', match: { text: 'integrator-routes' } });
  });

  it('should exclude deleted for libraries collection', () => {
    const filter = buildFilter(
      makeParams({
        collection: LIBRARIES_COLLECTION,
        includeDeleted: false,
      })
    );
    expect(filter).not.toBeNull();
    const mustNot = filter!.must_not as Record<string, unknown>[];
    expect(mustNot).toBeDefined();
    const deletedCondition = mustNot.find((c) => (c as Record<string, unknown>).key === 'deleted');
    expect(deletedCondition).toBeDefined();
  });
});

describe('extractGlobPrefix', () => {
  it('should extract prefix before first metachar', () => {
    expect(extractGlobPrefix('src/**/*.rs')).toBe('src/');
    expect(extractGlobPrefix('src/tools/**/*.ts')).toBe('src/tools/');
  });

  it('should return empty for leading metachar', () => {
    expect(extractGlobPrefix('**/*.rs')).toBe('');
    expect(extractGlobPrefix('*.ts')).toBe('');
  });

  it('should return full string for no metachar', () => {
    expect(extractGlobPrefix('src/tools/search.ts')).toBe('src/tools/search.ts');
  });
});

describe('determineCollections', () => {
  it('should return explicit collection', () => {
    expect(determineCollections('my_collection', 'project', false)).toEqual(['my_collection']);
  });

  it('should return projects for project scope without libraries', () => {
    const result = determineCollections(undefined, 'project', false);
    expect(result).toEqual([PROJECTS_COLLECTION]);
  });

  it('should include libraries for project scope with includeLibraries', () => {
    const result = determineCollections(undefined, 'project', true);
    expect(result).toContain(PROJECTS_COLLECTION);
    expect(result).toContain(LIBRARIES_COLLECTION);
  });
});
