/**
 * Argument readers shared by the `graph` tool's actions.
 */

export type JsonObject = Record<string, unknown>;

export function str(args: JsonObject, key: string): string | undefined {
  const v = args[key];
  return typeof v === 'string' && v.trim().length > 0 ? v : undefined;
}

export function num(args: JsonObject, key: string): number | undefined {
  const v = args[key];
  return typeof v === 'number' && Number.isFinite(v) ? v : undefined;
}

/**
 * Extract and validate `minConfidence` (shared by relations/impact/usages).
 * Confidence is a best-path edge-weight product in [0,1] — NOT a percentage; a
 * threshold above 1.0 would silently filter out every node (all confidences are
 * <= 1.0), indistinguishable from "no relations exist", so out-of-range values
 * are rejected loudly here before any daemon call.
 */
export function minConfidenceArg(args: JsonObject): number | undefined {
  const v = num(args, 'minConfidence');
  if (v !== undefined && (v < 0 || v > 1)) {
    throw new Error(
      `\`minConfidence\` must be within [0, 1], got ${v} — confidence is a best-path ` +
        'edge-weight product (e.g. 0.5), not a percentage.'
    );
  }
  return v;
}

/**
 * Extract `maxHops` for `impact`. A depth is a whole number of hops from 1 up;
 * anything else is rejected rather than silently rounded. Above the daemon's
 * ceiling (5) it is passed through: the daemon caps it and echoes the depth it
 * walked, so the cap is visible in the response.
 */
export function maxHopsArg(args: JsonObject): number | undefined {
  const v = num(args, 'maxHops');
  if (v !== undefined && (!Number.isInteger(v) || v < 1)) {
    throw new Error(`\`maxHops\` must be a whole number from 1 to 5, got ${v}.`);
  }
  return v;
}

export function strArray(args: JsonObject, key: string): string[] | undefined {
  const v = args[key];
  if (Array.isArray(v)) {
    const out = v.filter((x): x is string => typeof x === 'string');
    return out.length > 0 ? out : undefined;
  }
  return undefined;
}
