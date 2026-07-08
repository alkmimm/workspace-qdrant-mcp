/**
 * Search filter construction and collection determination.
 */

import type { SearchScope, FilterParams } from './search-types.js';
import {
  PROJECTS_COLLECTION,
  LIBRARIES_COLLECTION,
  SCRATCHPAD_COLLECTION,
} from './search-types.js';
import {
  FIELD_TENANT_ID,
  FIELD_BASE_POINT,
  FIELD_BRANCH,
  FIELD_FILE_TYPE,
  FIELD_LIBRARY_NAME,
  FIELD_CONCEPT_TAGS,
  FIELD_FILE_PATH,
  FIELD_DELETED,
} from '../common/native-bridge.js';

/**
 * Extract the deterministic path prefix from a glob pattern.
 * Returns everything before the first glob metacharacter (* ? [ {).
 * Example: "src/**\/*.rs" → "src/", "**\/*.rs" → ""
 */
export function extractGlobPrefix(glob: string): string {
  const metaChars = /[*?[{]/;
  const match = metaChars.exec(glob);
  if (!match) {
    // No glob chars — the whole string is a literal path
    return glob;
  }
  // Take everything up to the first metachar,
  // then trim to the last path separator for a clean prefix
  const beforeMeta = glob.slice(0, match.index);
  const lastSlash = beforeMeta.lastIndexOf('/');
  return lastSlash >= 0 ? beforeMeta.slice(0, lastSlash + 1) : '';
}

/**
 * Determine which collections to search based on scope.
 */
export function determineCollections(
  collection: string | undefined,
  scope: SearchScope,
  includeLibraries: boolean
): string[] {
  if (collection) return [collection];

  switch (scope) {
    case 'project':
      return includeLibraries ? [PROJECTS_COLLECTION, LIBRARIES_COLLECTION] : [PROJECTS_COLLECTION];
    case 'global':
      return [PROJECTS_COLLECTION];
    case 'all':
      return [PROJECTS_COLLECTION, LIBRARIES_COLLECTION, SCRATCHPAD_COLLECTION];
    default:
      return [PROJECTS_COLLECTION];
  }
}

// ── Filter condition builders ─────────────────────────────────────────────

function buildProjectCondition(params: FilterParams): Record<string, unknown> | null {
  if (params.scope !== 'project' || !params.projectId) return null;
  return { key: FIELD_TENANT_ID, match: { value: params.projectId } };
}

function buildBasePointCondition(params: FilterParams): Record<string, unknown> | null {
  if (!params.basePoints || params.basePoints.length === 0) return null;
  return { key: FIELD_BASE_POINT, match: { any: params.basePoints } };
}

/**
 * Build the Qdrant branch-scope clause shared by every project-scoped reader.
 *
 * `branch` unset or `"*"` → no clause (cross-branch). Otherwise scope to
 * `branch`, widening with a `should` to also accept `fallbackBranch` (the base
 * branch the daemon tags unchanged files under — see {@link resolveFallbackBranch}).
 * Exported so `retrieve` (and any other Qdrant reader) reuses the exact same
 * clause instead of re-deriving branch semantics that could drift from search.
 */
export function branchFilterClause(
  branch: string | undefined,
  fallbackBranch: string | undefined
): Record<string, unknown> | null {
  if (!branch || branch === '*') return null;
  if (fallbackBranch && fallbackBranch !== branch) {
    return {
      should: [
        { key: FIELD_BRANCH, match: { value: branch } },
        { key: FIELD_BRANCH, match: { value: fallbackBranch } },
      ],
    };
  }
  return { key: FIELD_BRANCH, match: { value: branch } };
}

function buildBranchCondition(params: FilterParams): Record<string, unknown> | null {
  return branchFilterClause(params.branch, params.fallbackBranch);
}

function buildFileTypeCondition(params: FilterParams): Record<string, unknown> | null {
  if (!params.fileType) return null;
  return {
    should: [
      { key: FIELD_FILE_TYPE, match: { value: params.fileType } },
      { key: 'document_type', match: { value: params.fileType } },
    ],
  };
}

function buildLibraryNameCondition(params: FilterParams): Record<string, unknown> | null {
  if (params.collection !== LIBRARIES_COLLECTION || !params.libraryName) return null;
  return { key: FIELD_LIBRARY_NAME, match: { value: params.libraryName } };
}

function buildTagConditions(params: FilterParams): Record<string, unknown>[] {
  const conditions: Record<string, unknown>[] = [];

  if (params.tag) {
    conditions.push({ key: FIELD_CONCEPT_TAGS, match: { value: params.tag } });
  }

  if (params.tags && params.tags.length > 0) {
    const tagShouldConditions = params.tags.map((t) => ({
      key: FIELD_CONCEPT_TAGS,
      match: { value: t },
    }));
    conditions.push({ should: tagShouldConditions });
  }

  return conditions;
}

function buildComponentCondition(params: FilterParams): Record<string, unknown> | null {
  if (!params.component) return null;
  return {
    should: [
      { key: 'component_id', match: { value: params.component } },
      { key: 'component_id', match: { text: `${params.component}.` } },
    ],
  };
}

function extractGlobTextHint(glob: string): string {
  const normalized = glob.replace(/\\/g, '/');
  const literalSegments = normalized
    .split('/')
    .filter((segment) => segment.length > 0 && !/[*?[{]/.test(segment));
  literalSegments.sort((a, b) => b.length - a.length);
  return literalSegments[0] ?? '';
}

function buildPathGlobCondition(params: FilterParams): Record<string, unknown> | null {
  if (!params.pathGlob) return null;
  const prefix = extractGlobPrefix(params.pathGlob);
  const textHint = prefix || extractGlobTextHint(params.pathGlob);
  if (!textHint) return null;
  return { key: FIELD_FILE_PATH, match: { text: textHint } };
}

function buildMustConditions(params: FilterParams): Record<string, unknown>[] {
  const conditions: Record<string, unknown>[] = [];

  const projectCond = buildProjectCondition(params);
  if (projectCond) conditions.push(projectCond);

  const basePointCond = buildBasePointCondition(params);
  if (basePointCond) conditions.push(basePointCond);

  const branchCond = buildBranchCondition(params);
  if (branchCond) conditions.push(branchCond);

  const fileTypeCond = buildFileTypeCondition(params);
  if (fileTypeCond) conditions.push(fileTypeCond);

  const libNameCond = buildLibraryNameCondition(params);
  if (libNameCond) conditions.push(libNameCond);

  conditions.push(...buildTagConditions(params));

  const componentCond = buildComponentCondition(params);
  if (componentCond) conditions.push(componentCond);

  const pathGlobCond = buildPathGlobCondition(params);
  if (pathGlobCond) conditions.push(pathGlobCond);

  return conditions;
}

function buildMustNotConditions(params: FilterParams): Record<string, unknown>[] {
  if (params.collection !== LIBRARIES_COLLECTION) return [];
  return [{ key: FIELD_DELETED, match: { value: true } }];
}

/**
 * Build Qdrant filter based on search parameters.
 */
export function buildFilter(params: FilterParams): Record<string, unknown> | null {
  const mustConditions = buildMustConditions(params);
  const mustNotConditions = buildMustNotConditions(params);

  if (mustConditions.length === 0 && mustNotConditions.length === 0) {
    return null;
  }

  const filter: Record<string, unknown> = {};
  if (mustConditions.length > 0) filter.must = mustConditions;
  if (mustNotConditions.length > 0) filter.must_not = mustNotConditions;
  return filter;
}
