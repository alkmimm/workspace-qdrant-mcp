/**
 * Per-branch views of the daemon-owned `tracked_files` authority.
 *
 * One row per content generation of a path, each tagged with the branches
 * holding it (`branches` JSON set). A branch "has" a path when any generation
 * of it carries the branch tag.
 */

import type { Database as DatabaseType } from 'better-sqlite3';

/** SQLite's default bound-parameter ceiling is 999; stay well under it. */
const CHUNK = 400;

/**
 * Which of `relativePaths` the index holds under `branch` (any generation).
 *
 * The trunk fill-in of a branch-scoped read must not supply a path the branch
 * already owns: the branch's own generation is the truth for it even when that
 * generation did not match the query — otherwise a pattern found only in the
 * trunk's older copy surfaces as a hit on the branch. Best-effort: any failure
 * yields an empty set (the caller's git check still applies).
 */
export function getPathsTrackedOnBranch(
  db: DatabaseType | null,
  watchFolderId: string,
  branch: string,
  relativePaths: readonly string[]
): Set<string> {
  const out = new Set<string>();
  if (!db || relativePaths.length === 0) return out;
  const unique = [...new Set(relativePaths)];
  try {
    for (let i = 0; i < unique.length; i += CHUNK) {
      const chunk = unique.slice(i, i + CHUNK);
      const placeholders = chunk.map(() => '?').join(',');
      const rows = db
        .prepare(
          `SELECT DISTINCT tf.relative_path FROM tracked_files tf
             WHERE tf.watch_folder_id = ?
               AND tf.relative_path IN (${placeholders})
               AND EXISTS (SELECT 1 FROM json_each(tf.branches) WHERE value = ?)`
        )
        .all(watchFolderId, ...chunk, branch) as Array<{ relative_path: string }>;
      for (const row of rows) out.add(row.relative_path);
    }
  } catch {
    out.clear();
  }
  return out;
}

/** Distinct paths the index holds per branch for one watch folder. */
export interface BranchFileCount {
  branch: string;
  files: number;
}

/**
 * Distinct tracked paths per branch, largest first. This is what the index can
 * serve for each branch — the denominator a coverage report compares against
 * the branch's checkout. Best-effort: any failure yields an empty list.
 */
export function getTrackedFileCountsByBranch(
  db: DatabaseType | null,
  watchFolderId: string
): BranchFileCount[] {
  if (!db) return [];
  try {
    return db
      .prepare(
        `SELECT je.value AS branch, COUNT(DISTINCT tf.relative_path) AS files
           FROM tracked_files tf, json_each(tf.branches) je
          WHERE tf.watch_folder_id = ? AND je.value IS NOT NULL
          GROUP BY je.value
          ORDER BY files DESC, branch ASC`
      )
      .all(watchFolderId) as BranchFileCount[];
  } catch {
    return [];
  }
}
