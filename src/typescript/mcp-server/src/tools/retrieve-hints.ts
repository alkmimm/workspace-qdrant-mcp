/**
 * Shared retrieve guidance strings.
 *
 * Single source for the documentId/document_id and file-locator rules, so the
 * runtime hints in retrieve.ts and the `help("exact")` chapter
 * (help-topics.ts) cannot drift apart — same pattern as the derived tool-list
 * constants in mcp-public-config.json.
 */

export const RETRIEVE_ID_FILTER_HINT =
  'If you only have `metadata.document_id`, use `filter: { document_id: "<value>" }` instead.';

export const RETRIEVE_LOCATION_HINT =
  'For exact-search hits, pass `filePath` + `lineNumber` from the result metadata.';
