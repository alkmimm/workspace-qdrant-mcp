/**
 * Best-effort `is_test` annotation for the FTS-backed read surfaces.
 *
 * Semantic/hybrid hits get the flag from the Qdrant payload's ingest tags
 * (deriveIsTest in search-shaping.ts); FTS rows — grep matches and
 * exact-search hits — carry no tags, so the daemon's verdict is read back
 * from `tracked_files.is_test` by ABSOLUTE file path: same classifier
 * (`is_test_file()`), second store. Shared by grep.ts and search-exact.ts so
 * the two FTS surfaces cannot drift from each other.
 *
 * Project-scoped only: a cross-tenant (scope:"all") sweep has no single watch
 * folder to consult. Any failure yields an empty map — annotation must never
 * fail or meaningfully slow a search.
 */

import type { SqliteStateManager } from '../clients/sqlite-state-manager.js';
import type { TrackedFileAnnotation } from '../clients/tracked-files-queries/index.js';

export type { TrackedFileAnnotation };

/** The two state-manager reads the lookup needs (narrow for easy test mocks). */
export type TestFlagStateReader = Pick<
  SqliteStateManager,
  'getWatchFolderIdByTenantId' | 'getIsTestByFilePaths'
>;

/** Same shape for the richer lookup below. */
export type FileAnnotationStateReader = Pick<
  SqliteStateManager,
  'getWatchFolderIdByTenantId' | 'getFileAnnotationsByFilePaths'
>;

/** Absolute path → is_test for the tenant's watch folder; empty map when the
 *  tenant/state manager is unavailable or the lookup fails. */
export function lookupTestFlags(
  stateManager: TestFlagStateReader | undefined,
  tenantId: string | undefined,
  filePaths: readonly string[]
): Map<string, boolean> {
  if (!stateManager || !tenantId || filePaths.length === 0) return new Map();
  try {
    const watchFolderId = stateManager.getWatchFolderIdByTenantId(tenantId);
    if (!watchFolderId) return new Map();
    return stateManager.getIsTestByFilePaths(watchFolderId, [...new Set(filePaths)]);
  } catch {
    return new Map();
  }
}

/**
 * Absolute path → `relative_path` + `is_test` + `language`, for the FTS surfaces
 * that must present the same metadata fields as a semantic hit.
 *
 * Same contract as {@link lookupTestFlags} — project-scoped, best-effort, empty
 * map on any failure — and the same single query underneath, so a keyword hit
 * costs no extra round trip to gain the fields.
 */
export function lookupFileAnnotations(
  stateManager: FileAnnotationStateReader | undefined,
  tenantId: string | undefined,
  filePaths: readonly string[]
): Map<string, TrackedFileAnnotation> {
  if (!stateManager || !tenantId || filePaths.length === 0) return new Map();
  try {
    const watchFolderId = stateManager.getWatchFolderIdByTenantId(tenantId);
    if (!watchFolderId) return new Map();
    return stateManager.getFileAnnotationsByFilePaths(watchFolderId, [...new Set(filePaths)]);
  } catch {
    return new Map();
  }
}
