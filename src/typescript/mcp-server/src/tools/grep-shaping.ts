/**
 * Response shaping for the `grep` tool (spec 20 §3.2).
 *
 * Extracted from grep.ts (per the repo's gradual-refactor rule for oversized
 * files) and mirroring search-shaping.ts organizationally: per-line cap on
 * match content/context plus the same global response byte budget the search
 * tool enforces (drop trailing items once the running total exceeds the
 * budget; at least one always kept).
 *
 * The cap and the budget are FUSED into one pass, deliberately not delegating
 * to `applyByteBudget`: capping every match first and budgeting after would
 * materialize capped copies of up to `maxResults` matches (~MBs on minified
 * sweeps) only for the budget to discard ~95% of them, and would count
 * budget-dropped matches into `hitsTruncated`. Fusing caps only what ships
 * and keeps `hitsTruncated` meaning "SHIPPED matches with a capped line".
 * Budget semantics stay identical to the shared helper: stop at the first
 * over-budget item, keep >= 1.
 */

import type { GrepMatch } from './grep.js';
import { DEFAULT_MAX_RESPONSE_BYTES } from './search-types.js';

/** Per-line cap for grep match content/context (chars). Grep is line-scoped,
 *  so 500 chars comfortably fits real code lines while bounding
 *  minified/generated one-liners, which otherwise ship kilobytes for a
 *  single match. */
export const DEFAULT_GREP_MAX_BYTES_PER_LINE = 500;

/** Shaping knobs threaded from GrepOptions. */
export interface GrepShapingOptions {
  maxBytesPerLine?: number | undefined;
  maxResponseBytes?: number | undefined;
  /** Return only the count — drop the match bodies entirely. The strongest
   *  shaping mode there is, so it belongs with the other response-shape knobs
   *  rather than as another positional parameter. */
  countOnly?: boolean | undefined;
}

export interface ShapedGrepMatches {
  matches: GrepMatch[];
  /** SHIPPED matches with at least one line cut by the per-line cap. */
  hitsTruncated: number;
  /** Trailing matches dropped by the response byte budget. */
  dropped: number;
  shapeMode: 'truncate' | 'none';
}

function capLine(line: string, cap: number): string {
  if (line.length <= cap) return line;
  // Compact marker — grep output is line-dense, so tell the agent how much
  // was cut without repeating a full retrieve() call per line.
  return `${line.slice(0, cap)}…[+${line.length - cap} chars]`;
}

/** Payload cost of one match: content + context lines (what bytes_out counts). */
function grepMatchBytes(m: GrepMatch): number {
  let bytes = m.content.length;
  for (const line of m.context_before) bytes += line.length;
  for (const line of m.context_after) bytes += line.length;
  return bytes;
}

function capMatch(m: GrepMatch, cap: number): { match: GrepMatch; truncated: boolean } {
  const over =
    m.content.length > cap ||
    m.context_before.some((l) => l.length > cap) ||
    m.context_after.some((l) => l.length > cap);
  if (!over) return { match: m, truncated: false };
  return {
    match: {
      ...m,
      content: capLine(m.content, cap),
      context_before: m.context_before.map((l) => capLine(l, cap)),
      context_after: m.context_after.map((l) => capLine(l, cap)),
    },
    truncated: true,
  };
}

/**
 * Shape a grep match set before it ships (spec 20 §3.2): cap lines at
 * `maxBytesPerLine` and bound the summed match bodies at `maxResponseBytes`.
 * Both knobs accept 0 to disable; with both disabled this is a pass-through
 * reported as shapeMode 'none'.
 */
export function shapeGrepMatches(
  matches: GrepMatch[],
  opts: GrepShapingOptions
): ShapedGrepMatches {
  const cap = opts.maxBytesPerLine ?? DEFAULT_GREP_MAX_BYTES_PER_LINE;
  const budget = opts.maxResponseBytes ?? DEFAULT_MAX_RESPONSE_BYTES;
  if (cap <= 0 && budget <= 0) {
    return { matches, hitsTruncated: 0, dropped: 0, shapeMode: 'none' };
  }
  const kept: GrepMatch[] = [];
  let hitsTruncated = 0;
  let used = 0;
  for (const m of matches) {
    const { match, truncated } = cap > 0 ? capMatch(m, cap) : { match: m, truncated: false };
    const bytes = grepMatchBytes(match);
    if (kept.length > 0 && budget > 0 && used + bytes > budget) break;
    kept.push(match);
    used += bytes;
    if (truncated) hitsTruncated += 1;
  }
  return {
    matches: kept,
    hitsTruncated,
    dropped: matches.length - kept.length,
    shapeMode: 'truncate',
  };
}
