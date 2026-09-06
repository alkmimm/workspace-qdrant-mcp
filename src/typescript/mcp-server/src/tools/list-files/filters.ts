/**
 * File filtering utilities for the list tool.
 *
 * Path filtering itself is NOT here: `list` pushes `pattern`/`pathExclude`
 * down to SQLite as a `GLOB` clause (`pushGlobClause` in
 * `clients/tracked-files-queries/tracked-files.ts`), so filtering happens in
 * the query, not over a fetched page. This module used to also carry a
 * `filterByGlob`/`globToRegex` pair — a FOURTH glob engine that nothing
 * called, with semantics that matched neither the SQL clause, the shared
 * `utils/path-glob.ts` matcher, nor the daemon's `normalize_path_glob`. It was
 * removed rather than fixed: a dead engine only exists to be adopted by
 * accident. Anything needing to match a path in TypeScript belongs in
 * `utils/path-glob.ts`.
 */

import type { FolderNode } from '../list-files-types.js';

// ── Folder counting ───────────────────────────────────────────────────────

export function countFolders(node: FolderNode): number {
  let count = 0;
  for (const child of node.children.values()) {
    count += 1 + countFolders(child);
  }
  return count;
}
