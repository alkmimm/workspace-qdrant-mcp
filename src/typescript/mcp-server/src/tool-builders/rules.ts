/**
 * Rules tool argument builder — parse raw MCP tool arguments into RuleOptions
 */

import { DEFAULT_MAX_RESPONSE_BYTES } from '../tools/search-types.js';

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
    const maxResponseBytes = args?.['maxResponseBytes'];
    options.maxResponseBytes =
      typeof maxResponseBytes === 'number' ? maxResponseBytes : DEFAULT_MAX_RESPONSE_BYTES;
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
