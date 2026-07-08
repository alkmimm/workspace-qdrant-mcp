/**
 * Grep tool argument builder — parse raw MCP tool arguments into GrepOptions
 */

/**
 * Default cap on grep matches for the MCP tool. Absent an explicit `maxResults`
 * the daemon applies its own 1000-row default, which for a common identifier
 * floods the MCP response — a single unscoped grep for a hot symbol can exceed
 * 60k chars and blow the caller's context budget. This agent-sized default keeps
 * responses small; the daemon still reports `truncated` + `total_matches`, so the
 * caller sees when to narrow with pathGlob or raise `maxResults` for a full sweep.
 * The Rust CLI/daemon keep their own larger default (this is MCP-only).
 */
const DEFAULT_GREP_MAX_RESULTS = 100;

export type GrepOptions = {
  pattern: string;
  regex?: boolean;
  caseSensitive?: boolean;
  pathGlob?: string;
  pathExclude?: string;
  scope?: 'project' | 'all';
  contextLines?: number;
  maxResults?: number;
  branch?: string;
  projectId?: string;
  maxBytesPerLine?: number;
  maxResponseBytes?: number;
};

/** Build grep options from raw tool arguments. */
export function buildGrepOptions(args: Record<string, unknown> | undefined): GrepOptions {
  const pattern = args?.['pattern'] as string;
  if (!pattern) {
    throw new Error('Pattern is required for grep operation');
  }

  const options: GrepOptions = { pattern };

  const regex = args?.['regex'] as boolean | undefined;
  if (regex !== undefined) options.regex = regex;

  const caseSensitive = args?.['caseSensitive'] as boolean | undefined;
  if (caseSensitive !== undefined) options.caseSensitive = caseSensitive;

  const pathGlob = args?.['pathGlob'] as string | undefined;
  if (pathGlob) options.pathGlob = pathGlob;

  const pathExclude = args?.['pathExclude'] as string | undefined;
  if (pathExclude) options.pathExclude = pathExclude;

  const scope = args?.['scope'] as string | undefined;
  if (scope === 'project' || scope === 'all') options.scope = scope;

  const contextLines = args?.['contextLines'] as number | undefined;
  if (contextLines !== undefined) options.contextLines = contextLines;

  const maxResults = args?.['maxResults'] as number | undefined;
  options.maxResults = maxResults ?? DEFAULT_GREP_MAX_RESULTS;

  const branch = args?.['branch'] as string | undefined;
  if (branch) options.branch = branch;

  const projectId = args?.['projectId'] as string | undefined;
  if (projectId) options.projectId = projectId;

  const maxBytesPerLine = args?.['maxBytesPerLine'] as number | undefined;
  if (maxBytesPerLine !== undefined) options.maxBytesPerLine = maxBytesPerLine;

  const maxResponseBytes = args?.['maxResponseBytes'] as number | undefined;
  if (maxResponseBytes !== undefined) options.maxResponseBytes = maxResponseBytes;

  return options;
}
