/**
 * Rules tool argument builder — parse raw MCP tool arguments into RuleOptions
 */

import { DEFAULT_RULES_MAX_RESPONSE_BYTES } from '../tools/search-types.js';
import { RULES_SESSION_FETCH_LIMIT } from '../tools/rules-list.js';

export type RuleOptions = {
  action: 'add' | 'update' | 'remove' | 'list';
  content?: string;
  label?: string;
  scope?: 'global' | 'project';
  projectId?: string;
  title?: string;
  tags?: string[];
  priority?: number;
  limit?: number;
  summary?: boolean;
  maxResponseBytes?: number;
  cursor?: string;
  includeGlobal?: boolean;
};

/** Build rule options from raw tool arguments */
export function buildRuleOptions(args: Record<string, unknown> | undefined): RuleOptions {
  const action = args?.['action'] as string;
  if (action !== 'add' && action !== 'update' && action !== 'remove' && action !== 'list') {
    throw new Error(`Invalid rules action: ${action}`);
  }

  const options: RuleOptions = { action };

  const content = args?.['content'] as string | undefined;
  if (content) options.content = content;

  const label = args?.['label'] as string | undefined;
  if (label) options.label = label;

  const scope = args?.['scope'] as string | undefined;
  if (scope === 'global' || scope === 'project') options.scope = scope;

  const projectId = args?.['projectId'] as string | undefined;
  if (projectId) options.projectId = projectId;

  const title = args?.['title'] as string | undefined;
  if (title) options.title = title;

  const tags = args?.['tags'] as string[] | undefined;
  if (tags) options.tags = tags;

  const priority = args?.['priority'] as number | undefined;
  if (priority !== undefined) options.priority = priority;

  const limit = args?.['limit'] as number | undefined;
  if (limit !== undefined) options.limit = limit;

  // List response shaping (parity with scratchpad/list): the MCP surface
  // defaults to summary + the shared byte budget so a large rule set stays
  // cheap to load at session start. Internal callers (agent-rules, seeder)
  // bypass this builder and keep full, unbudgeted content.
  if (action === 'list') {
    const summary = args?.['summary'];
    options.summary = typeof summary === 'boolean' ? summary : true;
    // Fetch cap above the byte budget — the surface default lives HERE with the
    // other two (summary, budget), not in core listRules, so internal callers
    // (admin REST, seeder) keep their behaviour. See RULES_SESSION_FETCH_LIMIT.
    if (options.limit === undefined) options.limit = RULES_SESSION_FETCH_LIMIT;
    // Rules get a HIGHER default budget than the other read surfaces. Everything
    // else is a query whose next page is one call away; this is the call an
    // agent makes once, at session start, to learn how it must work. Truncating
    // it does not cost a round-trip — it silently drops conventions the agent
    // then violates, and nothing signals the omission at the point of use.
    // With the leaner summary shape, 61 rules land near 20k; 40k leaves room
    // for the set to grow before anyone has to page.
    const maxResponseBytes = args?.['maxResponseBytes'];
    options.maxResponseBytes =
      typeof maxResponseBytes === 'number' ? maxResponseBytes : DEFAULT_RULES_MAX_RESPONSE_BYTES;
    const cursor = args?.['cursor'];
    if (typeof cursor === 'string' && cursor) options.cursor = cursor;
    // A project-scoped listing also carries the global rules: they apply to
    // every project, and this is the session-start "load the rules" call — a
    // project with no rules of its own must not answer "0 rules". Internal
    // callers bypass this builder and list each scope separately.
    const includeGlobal = args?.['includeGlobal'];
    options.includeGlobal = typeof includeGlobal === 'boolean' ? includeGlobal : true;
  }

  return options;
}
