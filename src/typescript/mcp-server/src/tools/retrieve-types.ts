/**
 * Retrieve tool types and constants.
 */

// Canonical collection names from native bridge (single source of truth)
import {
  COLLECTION_PROJECTS,
  COLLECTION_LIBRARIES,
  COLLECTION_RULES,
  COLLECTION_SCRATCHPAD,
  FIELD_CONTENT,
} from '../common/native-bridge.js';
import { VECTOR_KEYS, stripServedNoise } from '../common/payload-noise.js';
import type { ProjectSource } from './project-echo.js';
export const PROJECTS_COLLECTION = COLLECTION_PROJECTS;
export const LIBRARIES_COLLECTION = COLLECTION_LIBRARIES;
export const RULES_COLLECTION = COLLECTION_RULES;
export const SCRATCHPAD_COLLECTION = COLLECTION_SCRATCHPAD;

export type RetrieveCollectionType = 'projects' | 'libraries' | 'rules' | 'scratchpad';

export interface RetrieveOptions {
  documentId?: string;
  /** Explicit file locator for exact-search hits. Use with `lineNumber`. */
  filePath?: string;
  /** 1-based line number for an exact-search hit. Requires `filePath`. */
  lineNumber?: number;
  collection?: RetrieveCollectionType;
  filter?: Record<string, string>;
  limit?: number;
  offset?: number;
  projectId?: string;
  libraryName?: string;
  /**
   * Branch to scope projects-collection scroll paths to. Defaults to the
   * caller's current Git branch (widened to the daemon's base branch for files
   * unchanged on a feature branch). Pass `"*"` to read across all branches —
   * the explicit opt-out that restores the old cross-branch behavior. Ignored
   * for scratchpad/libraries/rules reads: those collections are branch-agnostic.
   */
  branch?: string;
  /**
   * Argument names the caller passed that retrieve does not accept (set by
   * the builder). When present, `retrieve()` refuses the call with an
   * explanatory message instead of silently dropping the arguments.
   */
  unknownArgs?: string[];
}

export interface RetrievedDocument {
  id: string;
  content: string;
  metadata: Record<string, unknown>;
  score?: number;
}

export interface RetrieveResponse {
  success: boolean;
  documents: RetrievedDocument[];
  total?: number;
  hasMore?: boolean;
  message?: string;
  /** Short, actionable recovery guidance for the caller. */
  hint?: string;
  /** The project a projects/scratchpad read resolved to and how (`cwd` |
   *  `sticky-cwd` | `projectId`) — the read-side echo shared with search /
   *  grep / list / graph. Absent for libraries/rules and on failures. */
  project_id?: string;
  project_path?: string;
  project_source?: ProjectSource;
}

export interface RetrieveToolConfig {
  qdrantUrl: string;
  qdrantApiKey?: string;
  qdrantTimeout?: number;
}

/** Map collection type to canonical Qdrant collection name. */
export function getCollectionName(collection: RetrieveCollectionType): string {
  switch (collection) {
    case 'projects':
      return PROJECTS_COLLECTION;
    case 'libraries':
      return LIBRARIES_COLLECTION;
    case 'rules':
      return RULES_COLLECTION;
    case 'scratchpad':
      return SCRATCHPAD_COLLECTION;
    default:
      return PROJECTS_COLLECTION;
  }
}

/**
 * Surface-specific drops layered on top of the shared served-payload noise set
 * ({@link ../common/payload-noise.ts}): the content field `retrieve` already
 * lifts into `document.content`, plus the raw vectors.
 */
const RETRIEVE_EXTRA_DROP_KEYS: readonly string[] = [FIELD_CONTENT, ...VECTOR_KEYS];

/**
 * Extract metadata from a payload for serving, dropping content, vectors, the
 * daemon's ranking aids, ingest plumbing, and provably-redundant duplicates.
 *
 * Shares {@link stripServedNoise} with the `search` shaping path (CLAUDE.md
 * shared-behavior rule) so the two read surfaces cannot drift: a `retrieve`
 * response used to ship the same file_hash / base_point / idf_epoch /
 * absolute_path plumbing that `search` had already been trimming.
 */
export function extractMetadata(
  payload: Record<string, unknown> | null | undefined
): Record<string, unknown> {
  if (!payload) return {};
  return stripServedNoise(payload, RETRIEVE_EXTRA_DROP_KEYS);
}
