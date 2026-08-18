/**
 * Payload fields stripped from every SERVED read-surface response.
 *
 * The daemon writes a wide Qdrant payload per chunk: retrieval-scoring aids,
 * ingest bookkeeping, and several fields that are byte-identical duplicates of
 * a neighbour. A reading agent consumes none of it, yet it dominates the
 * response — a 9-line code hit measured 28 metadata fields whose serialization
 * was LARGER than the code it described. On a `limit:10` search that is
 * thousands of tokens of plumbing; across a 15-20 query audit it is context the
 * caller wanted to spend on code.
 *
 * Three categories, kept here as the single source of truth so the `search`
 * ({@link ../tools/search-shaping.ts}) and `retrieve`
 * ({@link ../tools/retrieve-types.ts}) paths cannot drift:
 *
 *  1. {@link RANKING_AID_KEYS} — scoring signal written by
 *     `inject_extraction_results` in
 *     `src/rust/daemon/core/src/strategies/processing/file/keyword_extract.rs`
 *     (~1.5-2k tokens/hit).
 *  2. {@link INTERNAL_PLUMBING_KEYS} — ingest/dedup/BM25 bookkeeping that
 *     addresses nothing an agent can call back into.
 *  3. {@link REDUNDANT_METADATA_PAIRS} — dropped ONLY when byte-identical to
 *     the field that supersedes them, so a payload where they ever diverge
 *     keeps both rather than hiding a difference.
 */

/**
 * Indexing / ranking-aid payload fields the daemon injects on every code chunk.
 *
 * They are pure *indexing* signal: a reading agent never consumes them, yet
 * they add ~1.5-2k tokens per hit — ~15-20k on a 10-hit response. Must mirror
 * the keys inserted in `keyword_extract.rs`.
 */
export const RANKING_AID_KEYS: readonly string[] = [
  'keywords',
  'keyword_baskets',
  'concept_tags',
  'structural_tags',
];

/**
 * Ingest bookkeeping with no agent-facing meaning. None of these addresses
 * anything a caller can pass back into a tool: point ids come from `id`,
 * follow-up reads take `file_path`/`document_id`, and project scoping is by
 * `cwd`/`projectId` — never by the payload's `tenant_id`.
 *
 * - `file_hash`     64-char content digest (change detection)
 * - `base_point`    32-char multi-clone dedup anchor
 * - `idf_epoch`     BM25 sparse-vector generation counter
 * - `tenant_id`     internal project key
 * - `item_type`     ingest discriminator ("file"/"content")
 * - `chunk_index`   chunk ordinal within the document
 * - `chunk_encoding`      always "utf-8" for indexed text
 * - `chunk_collection`    duplicates the hit's top-level `collection`
 * - `chunk_line_count`    the FILE's line count, not the chunk's
 * - `chunk_source_format` chunker-internal ("code"/"text")
 */
export const INTERNAL_PLUMBING_KEYS: readonly string[] = [
  'file_hash',
  'base_point',
  'idf_epoch',
  'tenant_id',
  'item_type',
  'chunk_index',
  'chunk_encoding',
  'chunk_collection',
  'chunk_line_count',
  'chunk_source_format',
];

/**
 * `[drop, keep]` pairs where `drop` is removed ONLY when its value is
 * byte-identical to `keep` in the same payload.
 *
 * `absolute_path` is an identity with `file_path` by construction — the
 * daemon's own chunk-payload test asserts it
 * (`src/rust/daemon/core/src/strategies/processing/file/chunk_embed/tests.rs`:
 * `assert_eq!(payload["file_path"], payload["absolute_path"])`) — so a hit
 * carried the same absolute path twice, plus `relative_path`, plus the
 * derived top-level `location`. The rest are the chunk-scoped restatements of
 * a document-scoped field.
 *
 * The equality guard is what makes this safe to apply blind across
 * projects/libraries/rules/scratchpad: if a collection ever populates them
 * differently, both survive.
 */
export const REDUNDANT_METADATA_PAIRS: readonly (readonly [string, string])[] = [
  ['absolute_path', 'file_path'],
  ['chunk_language', 'language'],
  ['document_type', 'file_type'],
  ['chunk_symbol_kind', 'chunk_chunk_type'],
];

/** Payload keys carrying chunk TEXT. The daemon duplicates the body into the
 *  payload as well as the hit's `content`, so serving both ships it twice. */
export const TEXT_BODY_KEYS: readonly string[] = [
  'content',
  'text',
  'chunk_text',
  'unit_text',
  'snippet',
  'body',
];

/** Raw vector fields — never served, unbounded. */
export const VECTOR_KEYS: readonly string[] = ['dense_vector', 'sparse_vector'];

const UNCONDITIONAL_DROP: ReadonlySet<string> = new Set<string>([
  ...RANKING_AID_KEYS,
  ...INTERNAL_PLUMBING_KEYS,
]);

/** True when `file_extension` merely restates the suffix already visible in a
 *  path field present on the same hit. Guarded (not unconditional) so an
 *  extensionless or mismatched entry keeps the explicit value. */
function extensionIsDerivable(metadata: Record<string, unknown>): boolean {
  const ext = metadata['file_extension'];
  if (typeof ext !== 'string' || ext === '') return false;
  const suffix = `.${ext}`;
  for (const key of ['relative_path', 'file_path']) {
    const p = metadata[key];
    if (typeof p === 'string' && p.endsWith(suffix)) return true;
  }
  return false;
}

/**
 * Drop ranking aids, ingest plumbing, and provably-redundant duplicates from a
 * payload about to be served to an agent.
 *
 * Denylist + equality-guarded pairs rather than an allowlist: the four
 * collections (projects / libraries / rules / scratchpad) carry different
 * payload shapes, and an allowlist tuned on code chunks would silently swallow
 * scratchpad provenance or library fields. Anything unrecognized survives.
 *
 * `extraKeys` lets a caller add surface-specific drops (e.g. `retrieve`'s
 * vector fields) without a second copy of this logic.
 */
export function stripServedNoise(
  metadata: Record<string, unknown>,
  extraKeys: readonly string[] = []
): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  const extra = extraKeys.length > 0 ? new Set(extraKeys) : undefined;
  for (const [key, value] of Object.entries(metadata)) {
    if (UNCONDITIONAL_DROP.has(key)) continue;
    if (extra?.has(key) === true) continue;
    out[key] = value;
  }
  for (const [drop, keep] of REDUNDANT_METADATA_PAIRS) {
    if (drop in out && keep in out && out[drop] === out[keep]) delete out[drop];
  }
  if ('file_extension' in out && extensionIsDerivable(out)) delete out['file_extension'];
  return out;
}
