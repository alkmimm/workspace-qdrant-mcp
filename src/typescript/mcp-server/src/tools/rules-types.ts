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
}

export interface RuleToolConfig {
  qdrantUrl: string;
  qdrantApiKey?: string;
  qdrantTimeout?: number;
  duplicationThreshold?: number;
}
