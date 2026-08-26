/**
 * MCP tool schema definition for the 'help' tool.
 *
 * On-demand topical manual (progressive disclosure, issue #357). The topic
 * list in the description AND the input enum are DERIVED from HELP_TOPICS
 * (tools/help-topics.ts) — add a chapter there and this definition updates
 * itself; a client that validates inputSchema rejects typos before a
 * round-trip. help-topics.ts imports only retrieve-hints.ts, so this import
 * pulls no runtime dependencies into the definition module.
 */

import { HELP_TOPIC_IDS } from '../tools/help-topics.js';

export const helpToolDefinition = {
  name: 'help',
  annotations: {
    title: 'On-demand usage manual',
    readOnlyHint: true,
    openWorldHint: false,
  },
  description:
    'Detailed usage manual for this server, served on demand instead of front-loaded into the session. ' +
    `Topics: ${HELP_TOPIC_IDS.map((id) => `"${id}"`).join(', ')}. ` +
    'Call without a topic for a {topic, summary} index. Error hints may reference a chapter as help("<topic>").',
  inputSchema: {
    type: 'object' as const,
    properties: {
      topic: {
        type: 'string',
        enum: [...HELP_TOPIC_IDS],
        description: 'Topic id. Omit to list all topics with one-line summaries.',
      },
    },
  },
};
