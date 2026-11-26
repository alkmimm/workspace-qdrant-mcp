export const readLogToolDefinition = {
  name: 'read_log',
  annotations: {
    title: 'Read a server log by line range',
    readOnlyHint: true,
    openWorldHint: false,
  },
  description:
    'Read a UTF-8 file in the server log directory (WQM_LOG_DIR). Compact mode groups only consecutive identical lines, ignoring a leading RFC 3339 timestamp. Use raw mode on the reported line range to inspect the original text.',
  inputSchema: {
    type: 'object' as const,
    properties: {
      file: {
        type: 'string',
        description: 'Log basename (.log or .jsonl, optional numeric rotation).',
      },
      mode: { type: 'string', enum: ['compact', 'raw'], description: 'Default: compact.' },
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
