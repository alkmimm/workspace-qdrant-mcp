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

/**
 * Page cap used for `countOnly` when the caller names none.
 *
 * The small agent-sized default above exists to bound the RESPONSE, but a
 * count-only response carries no match bodies — so the only thing the cap
 * governs is whether the reported total is exact (the deduped set) or a
 * truncation upper bound. Counting is the entire point of the mode, so it gets
 * a ceiling high enough that a real surface is counted exactly; beyond it the
 * response still reports `truncated: true` rather than lying.
 */
const COUNT_ONLY_MAX_RESULTS = 10000;

// Single source of truth for the options shape: the tool's own interface.
// A structural clone here was a fourth sync point for every new knob — a
// forgotten copy passes JSON-schema validation and is silently dropped
// before reaching the tool.
import type { GrepOptions } from '../tools/grep.js';

export type { GrepOptions };

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

  const countOnly = args?.['countOnly'] as boolean | undefined;
  if (countOnly === true) options.countOnly = true;

  const maxResults = args?.['maxResults'] as number | undefined;
  options.maxResults =
    maxResults ?? (countOnly === true ? COUNT_ONLY_MAX_RESULTS : DEFAULT_GREP_MAX_RESULTS);

  const offset = args?.['offset'] as number | undefined;
  if (offset !== undefined) options.offset = offset;

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
