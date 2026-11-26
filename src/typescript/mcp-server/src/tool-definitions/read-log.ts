export const readLogToolDefinition = {
  name: 'read_log',
  annotations: {
    title: 'Read a server log by line range',
    readOnlyHint: true,
    openWorldHint: false,
  },
  description:
    'Read a UTF-8 file in the server log directory (WQM_LOG_DIR). Compact mode groups only consecutive identical lines, ignoring a leading RFC 3339 timestamp. Template mode also groups consecutive memexd queue events with varying item IDs and shows min–max duration for successful items. Interleaved mode groups matching events even with intervening messages. Use raw mode on any listed line or inclusive range to inspect the original text.',
  inputSchema: {
    type: 'object' as const,
    properties: {
      file: {
        type: 'string',
        description: 'Log basename (.log or .jsonl, optional numeric rotation).',
      },
      mode: {
        type: 'string',
        enum: ['compact', 'template', 'interleaved', 'raw'],
        description: 'Default: compact.',
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
