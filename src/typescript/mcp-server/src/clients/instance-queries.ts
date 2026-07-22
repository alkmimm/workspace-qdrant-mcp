/**
 * Instance-aware search queries for SqliteStateManager.
 *
 * Provides base_point filtering for multi-clone project scenarios (Task 15).
 */

import type { Database as DatabaseType } from 'better-sqlite3';

/**
 * Get the watch_id for a project by its tenant_id.
 * Returns null if not found or database unavailable.
 */
export function getWatchFolderIdByTenantId(
  db: DatabaseType | null,
  tenantId: string,
): string | null {
  if (!db) return null;

  try {
    const row = db.prepare(
      `SELECT watch_id FROM watch_folders
       WHERE tenant_id = ? AND collection = 'projects' AND parent_watch_id IS NULL
       LIMIT 1`
    ).get(tenantId) as { watch_id: string } | undefined;

    return row?.watch_id ?? null;
  } catch {
    return null;
  }
}

/**
 * Count the independent *clone instances* of a project (same tenant_id) that
 * base_point narrowing would have to disambiguate.
 *
 * Only genuinely separate working copies count. A linked git **worktree**
 * (`is_worktree = 1`) is deliberately EXCLUDED: git forbids checking out the
 * same branch in two worktrees of one repo, so the branch filter already
 * isolates a worktree's results from the main tree's — base_point narrowing
 * between them is redundant. Counting worktrees here made every semantic
 * search of a repo that merely *has* a worktree enumerate one base_point per
 * tracked file, blow past {@link BASE_POINTS_FILTER_CAP}, and falsely report
 * `status: uncertain` (F-014). Rows predating the `is_worktree` column (NULL)
 * are treated as non-worktree clones, preserving the pre-column behaviour.
 *
 * When the result is <= 1 there is no instance ambiguity the tenant+branch
 * filter can't already resolve, so per-file base_point narrowing is skipped.
 * Only when 2+ genuine clones share a tenant_id does base_point filtering
 * actually disambiguate instances.
 *
 * Returns 0 when the database is unavailable.
 */
export function countCloneInstancesByTenantId(
  db: DatabaseType | null,
  tenantId: string,
): number {
  if (!db) return 0;

  try {
    const row = db.prepare(
      `SELECT COUNT(*) AS n FROM watch_folders
       WHERE tenant_id = ? AND collection = 'projects' AND parent_watch_id IS NULL
         AND COALESCE(is_worktree, 0) = 0`
    ).get(tenantId) as { n: number } | undefined;

    return row?.n ?? 0;
  } catch {
    return 0;
  }
}

/**
 * Get all distinct base_point values for files tracked under a watch folder
 * (and optionally its submodules via junction table).
 *
 * Used to filter Qdrant search results to the correct instance in
 * multi-clone scenarios.
 */
export function getActiveBasePoints(
  db: DatabaseType | null,
  watchFolderId: string,
  includeSubmodules = false,
): string[] {
  if (!db) return [];

  try {
    let sql: string;
    if (includeSubmodules) {
      sql = `SELECT DISTINCT base_point FROM tracked_files
             WHERE base_point IS NOT NULL AND (
                 watch_folder_id = ?
                 OR watch_folder_id IN (
                     SELECT child_watch_id FROM watch_folder_submodules
                     WHERE parent_watch_id = ?
                 )
             )`;
      return (db.prepare(sql).all(watchFolderId, watchFolderId) as Array<{ base_point: string }>)
        .map(r => r.base_point);
    } else {
      sql = `SELECT DISTINCT base_point FROM tracked_files
             WHERE base_point IS NOT NULL AND watch_folder_id = ?`;
      return (db.prepare(sql).all(watchFolderId) as Array<{ base_point: string }>)
        .map(r => r.base_point);
    }
  } catch {
    return [];
  }
}
