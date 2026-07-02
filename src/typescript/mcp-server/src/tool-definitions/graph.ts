/**
 * MCP tool schema definition for the 'graph' tool — code-relationship
 * graph navigation backed by the daemon's GraphService.
 */

export const graphToolDefinition = {
  name: 'graph',
  annotations: {
    title: 'Navigate code-relationship graph',
    readOnlyHint: true,
    openWorldHint: false,
  },
  description:
    'Navigate the code-relationship graph: callers/callees, change-impact, importance ranking, and module clusters. ' +
    'Built from symbol relations (calls, contains, uses-type, imports) extracted during indexing. ' +
    'Use this to understand how code connects before editing — e.g. "what calls this function?", "what breaks if I change X?", "what are the most central functions?". ' +
    'Required args per action: relations → symbol + filePath; impact/usages → symbol; stats/hotspots/bridges/modules → none (project-wide). ' +
    'Each relations/impact/usages node carries a `confidence` (best-path certainty): ~1.0 precise, 0.7 tenant-unique name, ~1/N (e.g. 0.17) an ambiguous same-name fan-out; pass `minConfidence` (e.g. 0.5) to suppress the low-confidence homonym noise.',
  inputSchema: {
    type: 'object' as const,
    properties: {
      action: {
        type: 'string',
        enum: ['stats', 'relations', 'impact', 'usages', 'hotspots', 'bridges', 'modules'],
        description:
          "stats: node/edge counts. relations: a symbol's dependencies (calls/uses-type/imports/inheritance) — excludes CONTAINS membership by default, so a large class returns its dependencies, not its member list (pass edgeTypes:[\"CONTAINS\"] to list members). impact: transitive change blast-radius (direct + indirect dependents). usages: DIRECT references only (1-hop \"find references\"). hotspots: most central symbols (PageRank). bridges: bottleneck symbols on many shortest paths (betweenness). modules: code clusters. Default: 'stats'.",
      },
      symbol: {
        type: 'string',
        description: "Symbol name. Required for 'impact' and 'relations'.",
      },
      filePath: {
        type: 'string',
        description:
          "Relative file path of the symbol's definition. Required for 'relations'; optional narrowing for 'impact'.",
      },
      symbolType: {
        type: 'string',
        default: 'function',
        description:
          "Symbol kind for 'relations' node lookup. Valid: function, async_function, method, struct, class, enum, interface, trait, type_alias, constant, module, macro, impl. Default: 'function'. If it doesn't match what the indexer stored (e.g. an async fn is 'async_function', not 'function'), relations now falls back to resolving the node by NAME — so a wrong symbolType no longer silently returns 0.",
      },
      maxHops: {
        type: 'number',
        default: 1,
        description: "Traversal depth for 'relations' (1-5, default 1).",
      },
      topK: {
        type: 'number',
        default: 20,
        description:
          "Max results: top symbols for 'hotspots'/'bridges', top-K largest clusters for 'modules' (default 20), and max nodes returned for 'impact'/'usages'/'relations' (nearest-first, default 50; 0 = all — the true total is still reported; when minConfidence is set, totals count the filtered set).",
      },
      maxSamples: {
        type: 'number',
        description:
          "For 'bridges': sample N source nodes for betweenness on large graphs (0/omit = exact).",
      },
      minConfidence: {
        type: 'number',
        description:
          "For 'relations'/'impact'/'usages': drop nodes whose best-path `confidence` is below this (0-1), applied at the daemon BEFORE topK and the reported total (so topK fills with passing nodes). Each node's confidence is the best-path edge-weight product: ~1.0 precise, 0.7 tenant-unique name, ~1/N (e.g. 0.17) an ambiguous same-name fan-out. Use ~0.5 for a precise view that suppresses homonym noise. Omitted/0 = no filter; values outside [0,1] are rejected (confidence is a product, not a percentage).",
      },
      minSize: {
        type: 'number',
        default: 2,
        description: "Minimum community size for 'modules' (default 2).",
      },
      memberLimit: {
        type: 'number',
        default: 10,
        description:
          "For 'modules': members listed per community (default 10). Each community also reports its true `member_count`; the largest clusters hold thousands of members, so this keeps the response agent-sized. Use 0 for all members.",
      },
      edgeTypes: {
        type: 'array',
        items: { type: 'string' },
        description:
          'Filter by edge type (e.g. ["CALLS","IMPORTS","CONTAINS","USES_TYPE","EXTENDS","IMPLEMENTS"]). Omitted = all types for hotspots/bridges/modules, but dependency edges only (CONTAINS excluded) for relations.',
      },
      projectId: {
        type: 'string',
        description:
          'Project tenant_id. Takes precedence over cwd. If both are omitted the project is auto-detected from cwd; graph errors rather than guessing.',
      },
      cwd: {
        type: 'string',
        description:
          'Absolute path of your current working directory. Pass this so the server can auto-detect the project over HTTP (same as search/grep/list). Ignored when projectId is provided.',
      },
    },
    required: ['action'],
  },
};
