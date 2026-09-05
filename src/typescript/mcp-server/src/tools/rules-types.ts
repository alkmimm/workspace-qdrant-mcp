import type { ProjectSource } from './project-echo.js';
/**
 * Rules tool types, interfaces, and constants.
 */

// Canonical rules collection name from native bridge (single source of truth)
import { COLLECTION_RULES } from '../common/native-bridge.js';
export const RULES_COLLECTION = COLLECTION_RULES;
export const RULES_BASENAME = 'rules';

export type RuleAction = 'add' | 'update' | 'remove' | 'list';
export type RuleScope = 'global' | 'project';

export interface Rule {
  id: string;
  label?: string;
  /** Full rule text. Optional because summary-mode `list` omits it in favour
   *  of `preview` + `content_length`; always present in full-content mode. */
  content?: string;
  /** Summary-mode (list) only: leading slice of `content`. */
  preview?: string;
  /** Summary-mode (list) only: total `content` length in chars. */
  content_length?: number;
  scope: RuleScope;
  projectId?: string;
  /**
   * Human-readable owner of the rule: the owning project's tenant_id for
   * `scope: "project"` rules, or `"global"` for global rules. Always set on
   * `list` output so an agent can tell which project a rule belongs to even
   * when a list spans multiple tenants (e.g. the current project couldn't be
   * detected and the scroll fell back to all rules).
   */
  owner?: string;
  title?: string;
  tags?: string[];
  priority?: number;
  createdAt?: string;
  updatedAt?: string;
}

/**
 * Fetch cap for EVERY rules listing (MCP surface, system-prompt injection,
 * seeder dedup, admin REST) — the single default `listRules` applies.
 *
 * One home, one value, deliberately: an earlier draft raised only the MCP
 * surface and kept core at 50 "to protect internal callers", but the internal
 * callers are precisely the ones a partial listing hurts most. The seeder
 * builds its duplicate-prevention guards from this listing — a truncated set
 * re-adds an edited default as a duplicate-label row on every boot — and the
 * admin UI is the surface where a human curates the complete set. Measured
 * 2026-08-12: 61 rules live; the old 50 cap made 11 of them invisible with the
 * drop accounting self-consistent (46 kept + 4 dropped = 50).
 */
export const RULES_LIST_FETCH_LIMIT = 200;

/**
 * Byte budget for `rules` list responses on the MCP surface only.
 *
 * Higher than the shared ~24k of search/grep/scratchpad because this is the
 * session-start "load the conventions" call: a dropped rule is not a paging
 * round-trip, it is a convention the agent silently never learns. Internal
 * callers pass no budget and get everything.
 */
export const DEFAULT_RULES_MAX_RESPONSE_BYTES = 40000;

export interface RuleOptions {
  action: RuleAction;
  content?: string;
  label?: string;
  scope?: RuleScope;
  projectId?: string;
  title?: string;
  tags?: string[];
  priority?: number;
  limit?: number;
  /**
   * For list: return summary rules (preview + content_length) instead of full
   * bodies. Undefined/false → full content (internal callers rely on this); the
   * MCP tool surface defaults it to true.
   */
  summary?: boolean;
  /**
   * For list: cap on total response chars. Trailing rules are dropped (>=1
   * kept) and `next_cursor` resumes at the first dropped rule. 0/undefined
   * disables — internal callers get everything.
   */
  maxResponseBytes?: number;
  /** For list: opaque pagination cursor from a prior response's `next_cursor`. */
  cursor?: string;
  /**
   * For list with `scope: "project"`: also return global rules, which apply to
   * every project. Undefined/false → project rules only, because internal
   * callers (agent-rules' system-prompt injection) issue a SEPARATE global list
   * and would otherwise inject each global rule twice. The MCP tool surface
   * defaults it to true: `rules action:"list"` is the session-start call, and a
   * project with no rules of its own must not answer "0 rules" while global
   * rules exist. Each rule's `owner` (tenant_id or "global") tells them apart.
   */
  includeGlobal?: boolean;
}

export interface RuleResponse {
  success: boolean;
  action: RuleAction;
  label?: string;
  rules?: Rule[];
  /** Number of rules in `rules` (post-budget). */
  count?: number;
  /** Total rules matching the scope (best-effort; omitted on count failure). */
  total?: number;
  /** Pagination cursor — pass back as `cursor` for the next page. */
  next_cursor?: string;
  /** Rules dropped by the response byte budget (resume via next_cursor). */
  budget_truncated?: { dropped: number };
  /** Guidance shown in summary mode. */
  hint?: string;
  similar_rules?: Array<Rule & { similarity: number }>;
  message?: string;
  fallback_mode?: 'unified_queue';
  queue_id?: string;
  /** Read-side project echo on a project-scoped list (parity with the other
   *  tenant-addressed reads): the tenant answered from and how it was resolved. */
  project_id?: string;
  project_path?: string;
  project_source?: ProjectSource;
}

export interface RuleToolConfig {
  qdrantUrl: string;
  qdrantApiKey?: string;
  qdrantTimeout?: number;
  duplicationThreshold?: number;
}
