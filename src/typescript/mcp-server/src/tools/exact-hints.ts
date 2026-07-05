/**
 * Shared empty-result guidance for exact (literal) code search.
 *
 * Exact substring matching is whitespace-sensitive: a multi-token literal like
 * `final currentUserProvider =` never matches when a type annotation or any
 * differing spacing sits between the tokens. When such a pattern comes back
 * empty, agents tend to retry it verbatim; steer them to a whitespace-tolerant
 * regex instead. Shared by the `grep` tool and the `search` tool's exact mode so
 * the guidance stays consistent (CLAUDE.md: no one-off fixes for shared behavior).
 */

/** True when the pattern has content-bearing internal whitespace (multi-token). */
export function isMultiTokenLiteral(pattern: string): boolean {
  return /\S\s+\S/.test(pattern);
}

/**
 * A one-sentence whitespace-sensitivity clause to append to an empty-result hint,
 * or `null` when the pattern is a single token (spacing cannot be the cause).
 *
 * `regexAvailable` — true for the `grep` tool (has a regex mode); false for the
 * `search` tool's exact mode (no regex — the remedy is to switch to `grep`).
 */
export function whitespaceSensitivityHint(pattern: string, regexAvailable: boolean): string | null {
  if (!isMultiTokenLiteral(pattern)) return null;
  const example = pattern.trim().replace(/\s+/g, '\\s+');
  const lead =
    'This is a multi-token literal and exact matching is whitespace-sensitive — a type ' +
    'annotation or any differing spacing between the tokens defeats it. ';
  return regexAvailable
    ? `${lead}Retry with regex:true and \\s+ between tokens (e.g. "${example}"), or search the longest single token.`
    : `${lead}The search tool has no regex mode — use the grep tool with regex:true and \\s+ between tokens (e.g. "${example}"), or search a single token.`;
}
