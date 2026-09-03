/**
 * Store tool argument builder — parse raw MCP tool arguments into StoreOptions
 */

export type StoreOptions = {
  content: string;
  libraryName?: string;
  forProject?: boolean;
  projectId?: string;
  title?: string;
  url?: string;
  filePath?: string;
  sourceType?: 'user_input' | 'web' | 'file' | 'scratchbook' | 'note';
  metadata?: Record<string, string>;
};

// ── Validation ────────────────────────────────────────────────────────────

function validateStoreArgs(args: Record<string, unknown> | undefined): string {
  const content = args?.['content'] as string;
  if (!content) {
    throw new Error('Content is required for store operation');
  }

  const forProject = args?.['forProject'] as boolean | undefined;
  const libraryName = args?.['libraryName'] as string | undefined;

  if (!forProject && !libraryName) {
    throw new Error(
      'libraryName is required for type "library" (the default). ' +
      "Use forProject: true to store to the current project's library, " +
      'or pass type: "scratchpad" to save an ad-hoc/persistent note instead.'
    );
  }

  return content;
}

// ── Option extractors ─────────────────────────────────────────────────────

function extractTargetOptions(
  args: Record<string, unknown> | undefined,
  options: StoreOptions,
): void {
  const libraryName = args?.['libraryName'] as string | undefined;
  if (libraryName) options.libraryName = libraryName;

  const forProject = args?.['forProject'] as boolean | undefined;
  if (forProject) options.forProject = true;
  // `projectId` is deliberately NOT filled here. A forProject entry is
  // project-scoped, so its tenant is resolved by the shared write resolver in
  // the dispatcher (explicit projectId > effective cwd > session project) —
  // reading `sessionState.projectId` here would reinstate the session-first
  // precedence that misrouted writes away from the caller's cwd.
}

function extractMetadataOptions(
  args: Record<string, unknown> | undefined,
  options: StoreOptions,
): void {
  const title = args?.['title'] as string | undefined;
  if (title) options.title = title;

  const url = args?.['url'] as string | undefined;
  if (url) options.url = url;

  const filePath = args?.['filePath'] as string | undefined;
  if (filePath) options.filePath = filePath;

  const sourceType = args?.['sourceType'] as string | undefined;
  if (
    sourceType === 'user_input' ||
    sourceType === 'web' ||
    sourceType === 'file' ||
    sourceType === 'scratchbook' ||
    sourceType === 'note'
  ) {
    options.sourceType = sourceType;
  }

  const metadata = args?.['metadata'] as Record<string, string> | undefined;
  if (metadata) options.metadata = metadata;
}

/**
 * Build store options from raw tool arguments.
 * Store tool is for libraries collection ONLY per spec.
 */
export function buildStoreOptions(args: Record<string, unknown> | undefined): StoreOptions {
  const content = validateStoreArgs(args);
  const options: StoreOptions = { content };

  extractTargetOptions(args, options);
  extractMetadataOptions(args, options);

  return options;
}
