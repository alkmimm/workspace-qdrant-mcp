export const READ_LOG_MODES = ['compact', 'template', 'interleaved', 'auto', 'raw'] as const;
export type ReadLogMode = (typeof READ_LOG_MODES)[number];

export const readLogToolDefinition = {
  name: 'read_log',
  annotations: {
    title: 'Read a server log by line range',
    readOnlyHint: true,
    openWorldHint: false,
  },
  description:
    'Read a UTF-8 file from an allowlisted server log source. source="mcp" reads WQM_LOG_DIR; operators can configure additional read-only aliases with WQM_READ_LOG_ROOTS. Compact mode groups only consecutive identical lines, ignoring a leading RFC 3339 timestamp. Template mode also groups consecutive memexd queue events with varying item IDs and shows min–max duration for successful items. Interleaved mode groups matching events even with intervening messages. Use raw mode on any listed line or inclusive range to inspect the original text.',
  inputSchema: {
    type: 'object' as const,
    properties: {
      file: {
        type: 'string',
        description: 'Log basename (.log or .jsonl, optional numeric rotation).',
      },
      source: {
        type: 'string',
        pattern: '^[a-z][a-z0-9_-]{0,31}$',
        description: 'Configured log source alias. Default: mcp.',
      },
      mode: {
        type: 'string',
        enum: [...READ_LOG_MODES],
        description:
          'Default: compact. auto returns the smallest UTF-8 text among compact, template, interleaved/block and interleaved/window. Earlier formats win ties.',
      },
      groupBy: {
        type: 'string',
        enum: ['block', 'window'],
        description:
          'Interleaved mode only. block (default) groups within each 100 lines starting at startLine. window groups across the entire requested range.',
      },
      startLine: { type: 'integer', minimum: 1, description: 'Inclusive. Default: 1.' },
      endLine: {
        type: 'integer',
        minimum: 1,
        description:
          'Inclusive. Default: 200 lines from startLine. Maximum range: 2000 lines and 1 MiB.',
      },
    },
    required: ['file'],
  },
};
