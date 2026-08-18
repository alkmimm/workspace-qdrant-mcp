/**
 * "You got a slice, not the whole symbol" hint, shared by every read surface
 * that can hand back one fragment of an oversized symbol.
 *
 * A symbol too large for a single chunk is split into `chunk_total_fragments`
 * fragments that all carry the same `chunk_symbol_name`. For a container type —
 * a large enum, a class of constants — fragment 0 holds the declarations, which
 * is almost always what the caller was after, while the later fragments hold
 * trailing utility methods. Retrieval scores fragments independently, so
 * fragment 11 of 12 can win on its own merits and be the only thing returned.
 *
 * Field feedback (DOC-V2, 2026-08-13): `Capability.java` — 326 lines, ONE enum,
 * 12 fragments — under a single-file `pathGlob` returned exactly one hit,
 * fragment 11, the utility tail. With no sibling result to compensate, the call
 * was wasted. The payload already carried `chunk_fragment_index` and
 * `chunk_total_fragments`: the caller HAD the facts and still lost the call,
 * because raw fields do not say what to do next.
 *
 * Lives here, not in the search shaping path, because `retrieve` can hand back
 * exactly the same slice with exactly the same silence (CLAUDE.md
 * shared-behaviour rule).
 */

function asString(metadata: Record<string, unknown>, key: string): string | undefined {
  const v = metadata[key];
  return typeof v === 'string' && v.trim() !== '' ? v : undefined;
}

function asNumber(metadata: Record<string, unknown>, key: string): number | undefined {
  const v = metadata[key];
  if (typeof v === 'number' && Number.isFinite(v)) return v;
  if (typeof v === 'string' && v.trim() !== '') {
    const parsed = Number(v);
    if (Number.isFinite(parsed)) return parsed;
  }
  return undefined;
}

/**
 * Hint for a payload that is a NON-ZERO fragment of a multi-fragment symbol,
 * or `undefined` when the payload is a whole symbol (or fragment 0, which
 * already carries the declarations).
 */
export function fragmentSliceHint(metadata: Record<string, unknown>): string | undefined {
  if (asString(metadata, 'chunk_is_fragment') !== 'true') return undefined;
  const idx = asNumber(metadata, 'chunk_fragment_index');
  const total = asNumber(metadata, 'chunk_total_fragments');
  if (idx === undefined || total === undefined || total <= 1 || idx === 0) return undefined;

  const symbol = asString(metadata, 'chunk_symbol_name') ?? 'the symbol';
  const path = asString(metadata, 'relative_path') ?? asString(metadata, 'file_path');
  const where = path === undefined ? '' : ` in ${path}`;
  return (
    `Note: this is fragment ${idx} of ${total} of \`${symbol}\`${where} — one symbol too large ` +
    `for a single chunk, so this is a slice, not the whole thing. For a container type (enum, ` +
    `class of constants) fragment 0 carries the declarations and is usually the one you want: ` +
    `re-run with a query naming a declaration, or grep the symbol name for the exact lines.`
  );
}

/** First applicable {@link fragmentSliceHint} across a result page. */
export function firstFragmentSliceHint(
  results: readonly { metadata: Record<string, unknown> }[]
): string | undefined {
  for (const r of results) {
    const hint = fragmentSliceHint(r.metadata);
    if (hint !== undefined) return hint;
  }
  return undefined;
}
