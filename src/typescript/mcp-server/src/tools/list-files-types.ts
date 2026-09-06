/**
 * Types and constants for the list MCP tool.
 */
import type { ProjectSource } from './project-echo.js';

// ── Constants ────────────────────────────────────────────────────────────

export const DEFAULT_DEPTH = 3;
export const MAX_DEPTH = 10;
export const DEFAULT_LIMIT = 200;
export const MAX_LIMIT = 500;

// ── Input types ──────────────────────────────────────────────────────────

export type ListFormat = 'tree' | 'summary' | 'flat';

export interface ListOptions {
  path?: string;
  depth?: number;
  format?: ListFormat;
  fileType?: string;
  language?: string;
  extension?: string;
  pattern?: string;
  /** Glob on the file path to EXCLUDE (hard filter, opposite of `pattern`).
   *  Floats: "old_project/**" hides that dir at the repo root and any depth. */
  pathExclude?: string;
  includeTests?: boolean;
  limit?: number;
  projectId?: string;
  /** Filter by branch name. Defaults to the current Git branch; "*" searches all branches. */
  branch?: string;
  /**
   * Default/base branch to fall back to for files unchanged on `branch` (set
   * internally for feature-branch views; not a caller-facing argument).
   */
  fallbackBranch?: string;
  /** Filter by component (dot-separated ID or prefix, e.g. "daemon" or "daemon.core") */
  component?: string;
  /** Opaque pagination cursor from a previous response's next_token */
  cursor?: string;
  /** Page size for cursor-based pagination (falls back to limit) */
  pageSize?: number;
  /** Cap on the rendered listing chars (default: the shared search/grep
   *  budget, ~24k). Trailing page entries are dropped (>=1 kept), reported
   *  via `budget_truncated`, and next_token resumes at the first dropped
   *  entry — lossless. 0 disables. */
  maxResponseBytes?: number;
}

// ── Internal tree types ──────────────────────────────────────────────────

export interface FolderNode {
  name: string;
  children: Map<string, FolderNode>;
  files: FileLeaf[];
  /** If set, this folder is a submodule root — do not expand children */
  submodule?: SubmoduleMarker;
  /** Total file count in this subtree (computed during tree build) */
  totalFiles: number;
}

export interface FileLeaf {
  name: string;
  extension: string | null;
  language: string | null;
  isTest: boolean;
}

export interface SubmoduleMarker {
  repoName: string;
}

// ── Output types ─────────────────────────────────────────────────────────

export interface ComponentSummary {
  id: string;
  basePath: string;
  source: 'cargo' | 'npm' | 'directory';
}

export interface ListStats {
  files: number;
  folders: number;
  languages: string[];
  truncated: boolean;
  totalMatching: number;
  /** Detected project components (when available) */
  components?: ComponentSummary[];
}

export interface ListResponse {
  success: boolean;
  projectPath: string | null;
  /** Tenant id of the project listed and how it was resolved (`cwd` |
   *  `sticky-cwd` | `projectId`) — the read-side echo shared with search /
   *  grep / retrieve / graph. Absent on failure responses. */
  project_id?: string;
  project_source?: ProjectSource;
  basePath: string;
  format: ListFormat;
  listing: string;
  stats: ListStats;
  message?: string;
  /** Attached when the listing matched NOTHING: which filters were active and
   *  what the glob syntax accepts, so a zero result is not mistaken for an
   *  unindexed project (see `listNoMatchMessage`). */
  hint?: string;
  /** Opaque cursor for fetching the next page; absent when no more pages */
  next_token?: string;
  /** Attached only when the response byte budget dropped trailing page
   *  entries: `dropped` is how many were cut (>=1 always kept). next_token
   *  resumes at the first dropped entry, so nothing is skipped. */
  budget_truncated?: { dropped: number };
}
