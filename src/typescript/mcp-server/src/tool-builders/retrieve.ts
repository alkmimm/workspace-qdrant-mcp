/**
 * Retrieve tool argument builder — parse raw MCP tool arguments into RetrieveOptions.
 *
 * The result type is the canonical {@link RetrieveOptions} from the retrieve tool
 * itself — NOT a local copy. A local subset used to drift: `filePath`/`lineNumber`
 * were advertised in the tool schema and implemented in the handler
 * (`retrieveByLocation`) but silently dropped here, so the documented
 * exact-search-locator form degraded into a broad tenant scroll. Reusing the
 * canonical type means the extractors below must cover every arg-derived field,
 * and adding a field there surfaces here as the single source of truth.
 */

import { RETRIEVE_ARG_KEYS } from '../tool-definitions/retrieve.js';
import type { RetrieveOptions } from '../tools/retrieve-types.js';

export type { RetrieveOptions };

const KNOWN_ARG_KEYS: ReadonlySet<string> = new Set(RETRIEVE_ARG_KEYS);

/** Build retrieve options from raw tool arguments */
export function buildRetrieveOptions(args: Record<string, unknown> | undefined): RetrieveOptions {
  const options: RetrieveOptions = {};

  const documentId = args?.['documentId'] as string | undefined;
  if (documentId) options.documentId = documentId;

  const filePath = args?.['filePath'] as string | undefined;
  if (filePath) options.filePath = filePath;

  const lineNumber = args?.['lineNumber'] as number | undefined;
  if (lineNumber !== undefined) options.lineNumber = lineNumber;

  const collection = args?.['collection'] as string | undefined;
  if (
    collection === 'projects' ||
    collection === 'libraries' ||
    collection === 'rules' ||
    collection === 'scratchpad'
  ) {
    options.collection = collection;
  }

  const filter = args?.['filter'] as Record<string, string> | undefined;
  if (filter) options.filter = filter;

  const limit = args?.['limit'] as number | undefined;
  if (limit !== undefined) options.limit = limit;

  const offset = args?.['offset'] as number | undefined;
  if (offset !== undefined) options.offset = offset;

  const projectId = args?.['projectId'] as string | undefined;
  if (projectId) options.projectId = projectId;

  const libraryName = args?.['libraryName'] as string | undefined;
  if (libraryName) options.libraryName = libraryName;

  const unknownArgs = args ? Object.keys(args).filter((key) => !KNOWN_ARG_KEYS.has(key)) : [];
  if (unknownArgs.length > 0) options.unknownArgs = unknownArgs;

  return options;
}
