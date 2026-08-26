# MCP Tools Reference

The workspace-qdrant MCP server exposes twelve tools to AI assistants. All tools except the static `help` communicate with the `memexd` daemon over gRPC.

## Tool Index

| Tool | Purpose |
|------|---------|
| [`search`](#search) | Hybrid semantic + keyword search across indexed content |
| [`retrieve`](#retrieve) | Direct document lookup by ID or metadata filter |
| [`rules`](#rules) | Manage persistent behavioral rules |
| [`store`](#store) | Store content, register projects, save notes |
| [`scratchpad`](#scratchpad) | List, update, or delete scratchpad notes |
| [`grep`](#grep) | Exact substring or regex search using FTS5 |
| [`list`](#list) | List project files and folder structure |
| [`embedding`](#embedding) | Generate vector embeddings for text |
| [`graph`](#graph) | Navigate the code-relationship graph (callers, impact, hotspots) |
| [`workspace_index`](#workspace_index) | Manage indexed-project registry + branch sync (host hooks) |
| [`search_eval`](#search_eval) | Benchmark search quality (hit@k, recall, MRR) against a case set |
| [`help`](#help) | On-demand topical usage manual (static, progressive disclosure) |

---

## search

Search for documents using hybrid semantic and keyword search. Use this tool first when answering questions about the user's codebase, project architecture, or stored knowledge. Results come from the user's actual indexed content, which is more accurate than training data.

### Parameters

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `query` | string | Yes | — | The search query text |
| `collection` | string | No | — | Specific collection to search: `projects`, `libraries`, `rules`, `scratchpad` |
| `mode` | string | No | `hybrid` | Search mode: `hybrid`, `semantic`, or `keyword` |
| `scope` | string | No | `project` | Search scope: `project` (current project only), `global` (all projects), or `all` |
| `limit` | number | No | `10` | Maximum number of results to return |
| `projectId` | string | No | — | Specific project ID to search |
| `libraryName` | string | No | — | Library name when searching the `libraries` collection |
| `branch` | string | No | — | Filter results by branch name |
| `fileType` | string | No | — | Filter by file type |
| `includeLibraries` | boolean | No | `false` | Include library content in project search results |
| `tag` | string | No | — | Filter results by concept tag (exact match) |
| `tags` | string[] | No | — | Filter results by multiple concept tags (OR logic) |
| `pathGlob` | string | No | — | File path glob filter, e.g. `"**/*.rs"` or `"src/**/*.ts"` |
| `pathExclude` | string | No | — | File path glob to EXCLUDE (hard filter, opposite of `pathGlob`). Floats: `"old_project/**"` drops that dir at the repo root and any nested depth. Use it to silence a legacy/vendored tree. See also `WQM_SEARCH_DERANK` below to demote paths globally without passing it each call. |
| `component` | string | No | — | Filter by project component, e.g. `"daemon"` or `"daemon.core"`. Supports prefix matching. |
| `exact` | boolean | No | `false` | Use exact substring search instead of semantic search |
| `contextLines` | number | No | `0` | Lines of context to include before/after matches when `exact` is `true` |
| `includeGraphContext` | boolean | No | `false` | Include code relationship graph context (callers/callees) for matched symbols |

### Collections

| Value | Contents |
|-------|----------|
| `projects` | Indexed source files from all registered projects |
| `libraries` | Reference documentation, PDFs, and ingested library content |
| `rules` | Behavioral rules |
| `scratchpad` | Temporary notes and scratch content |

### Examples

Search the current project for authentication-related code:

```json
{
  "query": "JWT token validation",
  "scope": "project",
  "limit": 10
}
```

Search only Rust files using a path glob:

```json
{
  "query": "error handling retry",
  "scope": "project",
  "pathGlob": "**/*.rs",
  "limit": 15
}
```

Silence a legacy tree that pollutes the ranking (hard exclude):

```json
{
  "query": "invoice total calculation",
  "scope": "project",
  "pathExclude": "old_project/**"
}
```

> **De-rank vs exclude.** `pathExclude` removes matching hits outright, per call.
> To *demote* a path everywhere WITHOUT passing it each call, the deployment sets
> `WQM_SEARCH_DERANK` (comma-separated path substrings). (For the graph, the
> analogous knob is `WQM_GRAPH_EXCLUDE`, which HARD-excludes legacy/generated trees
> from centrality AND cycles — see the env table.) A matched hit's ranking
> score is multiplied by `WQM_SEARCH_DERANK_PENALTY` (0–1, default `0.2`) so it sinks
> below live code but stays findable. Semantic/hybrid `search` only — `grep` and
> exact search are literal, not ranked. Read per-call; change ⇒ recreate the mcp
> container (`docker compose up -d --force-recreate mcp`), no reembed.

Search library documentation for a specific concept:

```json
{
  "query": "connection pooling configuration",
  "collection": "libraries",
  "libraryName": "tokio-docs",
  "limit": 5
}
```

Search across all projects in semantic mode:

```json
{
  "query": "database migration strategy",
  "scope": "global",
  "mode": "semantic",
  "limit": 20
}
```

Search with graph context to understand callers:

```json
{
  "query": "process_queue_item",
  "scope": "project",
  "exact": true,
  "includeGraphContext": true
}
```

### Response Format

Returns an array of result objects. Each object includes:

- `id` — document or chunk identifier
- `score` — relevance score (0.0–1.0, higher is better)
- `content` — matched text content
- `metadata` — document metadata including file path, language, branch, component, and concept tags
- `graphContext` — (when `includeGraphContext: true`) callers and callees of matched symbols

---

## retrieve

Retrieve documents by point ID, exact-search file locator, or metadata filter. Use `retrieve` when you already know the point ID. Use `search` for discovery.

### Parameters

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `documentId` | string | No | — | Document ID to retrieve |
| `filePath` | string | No | — | Exact-search file locator |
| `lineNumber` | number | No | — | 1-based line number for an exact-search hit |
| `collection` | string | No | `projects` | Collection to retrieve from: `projects`, `libraries`, `rules`, `scratchpad` |
| `filter` | object | No | — | Metadata filter as key-value pairs. Values must be strings. |
| `limit` | number | No | `10` | Maximum number of results |
| `offset` | number | No | `0` | Pagination offset |
| `projectId` | string | No | — | Project ID for the `projects` collection |
| `libraryName` | string | No | — | Library name for the `libraries` collection |

At least one of `documentId`, `filePath`, or `filter` should be provided.

If you are retrieving something returned by `search` or `list`, pass the
result `id` field to `documentId`. If the hit came from exact search, pass
`filePath` + `lineNumber` from the result metadata instead. The metadata field
`document_id` is not a Qdrant point id; use `filter: { "document_id": "..." }`
for that case. The tool will also try that metadata filter automatically if the
direct point id lookup misses.

Failures may include a short `hint` with the recommended recovery step.

### Examples

Retrieve a document by its known ID:

```json
{
  "documentId": "abc123def456",
  "collection": "projects"
}
```

Retrieve a chunk by exact-search locator:

```json
{
  "filePath": "src/auth/validator.rs",
  "lineNumber": 42,
  "collection": "projects"
}
```

Retrieve all documents from a specific file:

```json
{
  "collection": "projects",
  "filter": {
    "file_path": "src/auth/validator.rs"
  }
}
```

Paginate through library content:

```json
{
  "collection": "libraries",
  "libraryName": "rust-book",
  "limit": 20,
  "offset": 40
}
```

### Response Format

Returns an array of document objects. Each object includes:

- `id` — document identifier
- `content` — document text content
- `metadata` — associated metadata fields

---

## rules

Manage persistent behavioral rules. Rules guide how the AI assistant behaves across sessions. They are loaded at the start of each session and persist in the `rules` collection.

### Parameters

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `action` | string | Yes | — | Action to perform: `add`, `update`, `remove`, or `list` |
| `label` | string | Conditional | — | Rule label (max 15 chars, format: `word-word-word`, e.g. `prefer-uv`). Required for `add`, `update`, `remove`. |
| `content` | string | Conditional | — | Rule content. Required for `add` and `update`. |
| `scope` | string | No | `global` | Rule scope: `global` or `project` |
| `projectId` | string | No | — | Project ID for project-scoped rules |
| `title` | string | No | — | Rule title (max 50 chars) |
| `tags` | string[] | No | — | Categorization tags (max 5 tags, max 20 chars each) |
| `priority` | number | No | — | Rule priority (higher number = more important) |
| `limit` | number | No | `50` | Maximum rules to return for `list` action |

### Actions

| Action | Required Parameters | Description |
|--------|--------------------|-|
| `add` | `label`, `content` | Create a new rule |
| `update` | `label`, `content` | Update an existing rule |
| `remove` | `label` | Delete a rule |
| `list` | — | List rules, optionally filtered by scope |

### Label Format

Labels must be lowercase, hyphen-separated words, maximum 15 characters total. Examples: `prefer-uv`, `use-pytest`, `no-commits`.

### Examples

List all global rules at session start:

```json
{
  "action": "list",
  "scope": "global"
}
```

Add a global preference rule:

```json
{
  "action": "add",
  "label": "prefer-types",
  "content": "Always use explicit type annotations in TypeScript. Avoid `any`.",
  "scope": "global",
  "priority": 8
}
```

Add a project-scoped rule:

```json
{
  "action": "add",
  "label": "no-direct-db",
  "content": "Never write directly to the database. All writes must go through the queue.",
  "scope": "project",
  "projectId": "abc123def456"
}
```

Remove a rule:

```json
{
  "action": "remove",
  "label": "prefer-types",
  "scope": "global"
}
```

### Response Format

- `add` / `update` / `remove`: returns a confirmation message and the affected rule label.
- `list`: returns an array of rule objects, each with `label`, `content`, `scope`, `priority`, `title`, and `tags`.

---

## store

Store content or register a project. Use `type` to select the storage destination.

### Parameters

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `type` | string | No | `library` | Storage type: `library`, `url`, `scratchpad`, or `project` |
| `content` | string | Conditional | — | Content to store. Required when `type` is `library`. |
| `libraryName` | string | Conditional | — | Library name. Required when `type` is `library` unless `forProject` is `true`. |
| `forProject` | boolean | No | `false` | When `true`, stores to the libraries collection scoped to the current project. `libraryName` defaults to `"project-refs"`. |
| `path` | string | Conditional | — | Project directory path. Required when `type` is `project`. |
| `name` | string | No | — | Project display name when `type` is `project`. Defaults to the directory name. |
| `title` | string | No | — | Content title for `library` type |
| `url` | string | No | — | Source URL for web content |
| `filePath` | string | No | — | Source file path |
| `tags` | string[] | No | — | Tags for `scratchpad` entries |
| `sourceType` | string | No | `user_input` | Source type: `user_input`, `web`, `file`, `scratchbook`, or `note` |
| `metadata` | object | No | — | Additional metadata as string key-value pairs |

### Storage Types

| Type | Destination | Use Case |
|------|-------------|----------|
| `library` | `libraries` collection | Store reference documentation, notes, code snippets |
| `url` | `libraries` or `projects` | Fetch and ingest a web page |
| `scratchpad` | `scratchpad` collection | Save temporary working notes |
| `project` | Daemon registration | Register a project directory for file watching and indexing |

### Examples

Store reference documentation in a library:

```json
{
  "type": "library",
  "libraryName": "project-notes",
  "title": "Architecture Decision: Queue Design",
  "content": "The unified queue uses SQLite with WAL mode for crash resistance...",
  "sourceType": "note"
}
```

Store a note scoped to the current project:

```json
{
  "type": "library",
  "forProject": true,
  "title": "API contract notes",
  "content": "The gRPC service exposes RegisterProject which enqueues to the unified queue..."
}
```

Fetch and ingest a web page:

```json
{
  "type": "url",
  "url": "https://docs.rs/tokio/latest/tokio/",
  "libraryName": "tokio-docs",
  "title": "Tokio API Reference"
}
```

Save a scratchpad note:

```json
{
  "type": "scratchpad",
  "content": "Investigating slow queue processing: suspect IDF penalty too aggressive",
  "tags": ["investigation", "queue", "performance"]
}
```

Register a project directory:

```json
{
  "type": "project",
  "path": "/Users/chris/dev/projects/my-service",
  "name": "My Service"
}
```

### Response Format

Returns a confirmation message with the stored document ID or registration status.

---

## scratchpad

Manage existing scratchpad notes: list, update, or delete. To *create* a note, use `store` with
`type: "scratchpad"` — this tool never creates. Notes are project-scoped: pass `projectId` (the
`tenant_id` seen in a `search`/`list` result) or `cwd` to auto-detect the project.

`update`/`delete` identify a note by its **current** content (content-addressed) — it must match
verbatim, taken from a `scratchpad` `list` call (full content), not a `search` hit (which may be
truncated). If nothing matches exactly, the call fails with a clear error instead of silently no-oping.

### Parameters

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `action` | string | Yes | — | `list`, `update`, or `delete` |
| `content` | string | Conditional | — | Current, verbatim text of the note to target. Required for `update`/`delete`. |
| `newContent` | string | Conditional | — | Replacement text. Required for `update`. |
| `title` | string | No | — | New title (for `update`) |
| `tags` | string[] | No | — | New tags (for `update`) |
| `projectId` | string | No | — | Tenant the note belongs to (takes precedence over `cwd`); use `"global"` for project-less notes |
| `cwd` | string | No | — | Working directory used to auto-detect the project when `projectId` is omitted |
| `limit` | number | No | `50` | Maximum entries to return (for `list`) |

### Examples

```json
{ "action": "list" }
```

```json
{ "action": "delete", "content": "<verbatim note text>" }
```

```json
{ "action": "update", "content": "<old text>", "newContent": "<new text>" }
```

---

## grep

Search code with exact substring or regex pattern matching. Uses an FTS5 trigram index for fast line-level search across all indexed files. Unlike `search`, `grep` does not use embeddings and always returns exact matches.

### Parameters

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `pattern` | string | Yes | — | Search pattern (exact substring or regex) |
| `regex` | boolean | No | `false` | Treat `pattern` as a regular expression |
| `caseSensitive` | boolean | No | `true` | Case-sensitive matching |
| `pathGlob` | string | No | — | File path glob filter, e.g. `"**/*.rs"` or `"src/**/*.ts"` |
| `pathExclude` | string | No | — | File path glob to EXCLUDE from matches (hard filter, opposite of `pathGlob`). Floats: `"old_project/**"` drops that dir at the repo root and any nested depth. |
| `scope` | string | No | `project` | Search scope: `project` (current project) or `all` (all projects) |
| `contextLines` | number | No | `0` | Lines of context to include before and after each match |
| `maxResults` | number | No | `1000` | Maximum number of results to return |
| `branch` | string | No | — | Filter by branch name |
| `projectId` | string | No | — | Specific project ID to search |

### When to Use grep vs search

| Situation | Tool |
|-----------|------|
| Looking for an exact function name, string literal, or identifier | `grep` |
| Looking for code that does a particular thing, conceptually | `search` |
| Verifying a specific string exists in the codebase | `grep` |
| Finding related code by meaning or similarity | `search` |
| Tracking all uses of an API call | `grep` |

### Examples

Find all occurrences of a function call:

```json
{
  "pattern": "process_queue_item",
  "pathGlob": "**/*.rs"
}
```

Case-insensitive regex search in TypeScript files:

```json
{
  "pattern": "use(Effect|Callback|Memo)",
  "regex": true,
  "caseSensitive": false,
  "pathGlob": "**/*.tsx",
  "contextLines": 2
}
```

Find all `TODO` comments across the entire project:

```json
{
  "pattern": "TODO",
  "scope": "project",
  "maxResults": 200
}
```

### Response Format

Returns an array of match objects. Each object includes:

- `filePath` — relative path to the matched file
- `lineNumber` — line number of the match (1-indexed)
- `lineContent` — the matched line text
- `contextBefore` — lines before the match (when `contextLines` > 0)
- `contextAfter` — lines after the match (when `contextLines` > 0)

---

## list

List project files and folder structure. Only shows indexed files; gitignored paths, `node_modules`, and build artifacts are excluded automatically.

### Parameters

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `path` | string | No | project root | Subfolder relative to the project root |
| `depth` | number | No | `3` | Maximum directory depth (maximum accepted value: `10`) |
| `format` | string | No | `tree` | Output format: `tree`, `summary`, or `flat` |
| `fileType` | string | No | — | Filter by file category: `code`, `text`, `data`, `config`, `build`, or `web` |
| `language` | string | No | — | Filter by programming language, e.g. `"rust"` or `"typescript"` |
| `extension` | string | No | — | Filter by file extension, e.g. `"rs"` or `"ts"` |
| `pattern` | string | No | — | Glob pattern applied to relative paths, e.g. `"**/*.test.ts"` |
| `pathExclude` | string | No | — | Glob to EXCLUDE from the listing (hard filter, opposite of `pattern`). Floats: `"old_project/**"` hides that dir at the repo root and any nested depth. |
| `includeTests` | boolean | No | `true` | Include test files in results |
| `limit` | number | No | `200` | Maximum number of entries returned (maximum: `500`) |
| `projectId` | string | No | current project | Specific project ID |
| `component` | string | No | — | Filter by component using dot-separated ID prefix, e.g. `"daemon"` or `"daemon.core"`. Auto-detected from `Cargo.toml`/`package.json` workspaces. |

### Output Formats

| Format | Description |
|--------|-------------|
| `tree` | Hierarchical directory tree with file names |
| `summary` | High-level overview showing directory counts and top-level structure |
| `flat` | Flat list of relative file paths |

### Examples

Get a high-level project overview:

```json
{
  "format": "summary"
}
```

Browse a specific subdirectory:

```json
{
  "path": "src/rust/daemon/core/src",
  "depth": 2,
  "format": "tree"
}
```

List all Rust source files in a component:

```json
{
  "component": "daemon.core",
  "language": "rust",
  "format": "flat",
  "includeTests": false
}
```

Find all test files:

```json
{
  "pattern": "**/*.test.ts",
  "format": "flat",
  "limit": 500
}
```

List only configuration files:

```json
{
  "fileType": "config",
  "format": "flat"
}
```

### Response Format

- `tree` format: returns a formatted directory tree string.
- `summary` format: returns counts of files per directory and language breakdown.
- `flat` format: returns an array of relative file path strings.

---

## embedding

Report the active embedding provider used by the daemon: provider id, model, configured output
dimensionality, base URL (for remote providers), and the live probe status. Useful for `/health`-style
introspection from the MCP client, and for verifying that a provider migration (`wqm admin reembed`)
succeeded.

### Parameters

None — the tool takes no arguments.

### Examples

```json
{}
```

### Response Format

Returns an object describing the active provider (id, model, output dimensionality, base URL if
remote) and its current probe status.

---

## graph

Navigate the **code-relationship graph** the daemon builds from symbol relations (calls, contains, uses-type, imports, inheritance) extracted during indexing. Use it for structural questions that `search`/`grep` can only guess at — "what calls this?", "what breaks if I change X?", "what are the most central functions?" — especially **before** refactoring or renaming a widely-used symbol.

### Parameters

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `action` | string | Yes | `stats` | One of `stats`, `relations`, `impact`, `usages`, `hotspots`, `bridges`, `modules` (see below) |
| `symbol` | string | For `impact`/`relations` | — | Symbol name |
| `filePath` | string | For `relations` | — | Relative path of the symbol's definition (optional narrowing for `impact`) |
| `symbolType` | string | No | `function` | Symbol kind for `relations` node lookup (`function`, `async_function`, `method`, `struct`, `class`, `enum`, `interface`, `trait`, `type_alias`, `constant`, `module`, `macro`, `impl`). Falls back to name resolution if it doesn't match. |
| `maxHops` | number | No | `1` | Traversal depth for `relations` (1–5) |
| `topK` | number | No | `20` / `50` | Max results (top symbols for hotspots/bridges/modules = 20; max nodes for impact/usages/relations = 50; `0` = all, true total still reported) |
| `minConfidence` | number | No | — | For `relations`/`impact`/`usages`: drop nodes whose best-path `confidence` is below this (0–1). Use ~`0.5` to suppress ambiguous same-name (homonym) fan-out. |
| `minSize` | number | No | `2` | Minimum community size for `modules` |
| `memberLimit` | number | No | `10` | Members listed per community for `modules` (`0` = all; each community reports its true `member_count`) |
| `maxSamples` | number | No | — | For `bridges`: sample N source nodes for betweenness on large graphs (`0`/omit = exact) |
| `edgeTypes` | string[] | No | — | Filter by edge type, e.g. `["CALLS","IMPORTS","CONTAINS","USES_TYPE","EXTENDS","IMPLEMENTS"]` |
| `projectId` / `cwd` | string | No | — | Project scoping — same semantics as `search`/`grep`/`list` |

### Actions

| Action | Returns |
|--------|---------|
| `stats` | Node/edge counts (project-wide) |
| `relations` | A symbol's dependencies (calls/uses-type/imports/inheritance). Excludes `CONTAINS` membership by default — pass `edgeTypes:["CONTAINS"]` to list a class's members instead of its dependencies |
| `impact` | Transitive change blast-radius: direct + indirect dependents |
| `usages` | DIRECT references only (1-hop "find references") |
| `hotspots` | Most central symbols (PageRank) |
| `bridges` | Bottleneck symbols on many shortest paths (betweenness) |
| `modules` | Code clusters (community detection) |

> **Confidence.** Each `relations`/`impact`/`usages` node carries a best-path `confidence`: ~`1.0` precise, `0.7` a tenant-unique name, ~`1/N` (e.g. `0.17`) an ambiguous same-name fan-out. Pass `minConfidence` to filter homonym noise at the daemon (before `topK` and the reported total).

### Examples

Change blast-radius before editing a function:

```json
{ "action": "impact", "symbol": "process_queue_item" }
```

A symbol's direct dependencies (precise view, homonyms suppressed):

```json
{ "action": "relations", "symbol": "SearchService", "filePath": "src/typescript/mcp-server/src/tools/search.ts", "symbolType": "class", "minConfidence": 0.5 }
```

Project-wide importance ranking:

```json
{ "action": "hotspots", "topK": 20 }
```

> **`graph` vs `search(includeGraphContext:true)`.** The `graph` tool is the dedicated, richer entry point (impact/hotspots/bridges/modules). `includeGraphContext` on `search` is a lightweight 1-hop callers/callees annotation on semantic hits — handy inline, but for blast-radius or centrality use `graph`.

---

## workspace_index

Observe and manage the local registry of indexed projects and the
agent-created branches that live under them. Read-only by default;
mutating actions require explicit double opt-in (env
`WQM_INDEX_MANAGER_ALLOW_MUTATION=1` + `allowMutation: true` argument).

Most actions delegate to a PowerShell registry helper (host-only). The
`sync_current_branch` action is implemented natively in TypeScript so
it works inside a containerized MCP server without `git` or PowerShell
on the host where the MCP is running.

### Actions

**Read-only** (no special opt-in):

- `list_projects`, `project_status`, `status_all`
- `list_branches`, `agent_branch_status`
- `observe_project`, `observe_all`
- `incremental_check`, `incremental_check_all`

**Mutating** (require both env + argument opt-in):

- `init`, `add_project`
- `start_agent_branch`, `finish_agent_branch`, `abandon_agent_branch`
- `register_wqm`, `register_all_wqm`
- `cleanup_orphans`
- `sync_current_branch` — *TypeScript-native, no PowerShell*

### `sync_current_branch`

Forward a `RegisterProject` gRPC call to the daemon with
`register_if_new=true`. Intended for git hooks: the host script
detects the current branch/commit/worktree state and POSTs it; the
MCP server delivers it to the daemon and the daemon decides whether
to create a new watch folder, auto-register a worktree under the main
repo's tenant_id, or reactivate an existing entry.

| Argument | Type | Required | Description |
|---|---|---|---|
| `action` | string | yes | `"sync_current_branch"` |
| `repoDir` | string | yes | Absolute path to the target git repo |
| `currentBranch` | string | no | Output of `git rev-parse --abbrev-ref HEAD` |
| `commitHash` | string | no | Output of `git rev-parse HEAD` |
| `worktreePath` | string | no | Path of the linked worktree (when `isWorktree=true`) |
| `isWorktree` | boolean | no | `true` when `.git` in `repoDir` is a file |
| `gitRemote` | string | no | `remote.origin.url` |
| `projectName` | string | no | Display name; defaults to `basename(repoDir)` |
| `hookName` | string | no | Label recorded for observability (e.g. `post-checkout`) |

The handler is best-effort: any missing field is filled in by calling
`getGitState(repoDir)` when the MCP server can see the path on its
own filesystem. Hook values always win when both are present.

### Example

```json
{
  "tool": "workspace_index",
  "action": "sync_current_branch",
  "repoDir": "/Users/me/dev/my-project",
  "currentBranch": "feature/auth",
  "commitHash": "3abd11df3abc",
  "isWorktree": false,
  "gitRemote": "https://github.com/me/my-project.git",
  "hookName": "post-checkout"
}
```

Response (truncated):

```json
{
  "success": true,
  "action": "sync_current_branch",
  "project_id": "367157a01d98",
  "newly_registered": true,
  "is_active": true,
  "is_worktree": false,
  "watch_path": "/Users/me/dev/my-project"
}
```

For the host-side hook script that drives this action, see
[scripts/git-hooks/README.md](../../scripts/git-hooks/README.md). For
the browser dashboard that exercises every other `workspace_index`
action interactively, see [Admin UI](../ADMIN_UI.md).

---

## search_eval

Benchmark semantic-search quality. Runs known-item queries (each with the file(s) that *should* rank)
through the live search pipeline and returns hit@1/3/10, recall@10, MRR, and duplicate-rate per mode
(`semantic`/`hybrid`/`exact`), plus a quality verdict. Use it for the measure → edit → measure loop when
tuning a ranking change. Runs in-process against the real index — no extra setup.

Omitting `cases` falls back to the bundled dataset, which only describes this server's own repo —
evaluating any *other* project without `cases` is refused, since the bundled gold files wouldn't exist
in that project (which would falsely report 0%).

### Parameters

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `cases` | array | No | bundled dataset | Ad-hoc eval set: each case is `{ id?, query, expectedFiles }`, where `expectedFiles` are repo-relative paths expected to rank. Required when evaluating a project other than this repo. |
| `limit` | number | No | `10` | Results fetched per query |
| `topK` | number | No | `10` | Evaluation cutoff K for hit@k / recall |
| `projectId` | string | No | auto-detect from `cwd` | Tenant to evaluate against |
| `cwd` | string | No | — | Working directory for project auto-detection; ignored if `projectId` is set |
| `scope` | string | No | `project` | `project`, `global`, or `all` |
| `includeTopPaths` | boolean | No | `false` | Include returned file paths per query (semantic mode), for debugging misses |
| `rerank` | boolean | No | deployment default (`WQM_SEARCH_RERANK`) | Force the cross-encoder rerank on/off for every query, for A/B sweeps without redeploying |
| `rerankWeight` | number | No | `WQM_SEARCH_RERANK_WEIGHT` (0.10 measured default) | Blend weight 0–1: final order is `(1-w)·norm(rrf_boosted) + w·norm(rerank)` |
| `summary` | boolean | No | `false` | `true` = metadata-only hits (no chunk bodies); ranking metrics are unchanged, only response size differs |

### Examples

Evaluate this repo with the bundled dataset:

```json
{}
```

Evaluate another project with an ad-hoc case set:

```json
{
  "cases": [
    { "query": "hybrid search fusion", "expectedFiles": ["src/search/fusion.ts"] }
  ],
  "cwd": "/home/me/projects/other-repo"
}
```

### Response Format

Returns per-mode metrics (hit@1/3/10, recall@10, MRR, duplicate-rate) and an overall quality verdict.

---

## help

On-demand topical usage manual. The server's always-on `instructions` carry only a short behavioral kernel; the detailed chapters live behind this tool and cost tokens only when fetched. Static — answered from in-process constants, no daemon call, no project detection.

### Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `topic` | string (enum) | No | Topic id. Omit to get the `{topic, summary}` index. |

The topic catalog is defined in `src/typescript/mcp-server/src/tools/help-topics.ts` and the input enum is derived from it — call `help` with no argument for the live list. Unknown or missing topics return the index plus a corrective hint, and error hints elsewhere may point at a chapter as `help("<topic>")`.

### Example

```json
{ "topic": "branches" }
```

---

## Common Patterns

### Session initialization

At the start of each session, load behavioral rules before doing any work:

```json
{
  "tool": "rules",
  "action": "list",
  "scope": "global"
}
```

Then check for project-specific rules:

```json
{
  "tool": "rules",
  "action": "list",
  "scope": "project"
}
```

### Codebase exploration

Start with a summary to understand the project layout:

```json
{
  "tool": "list",
  "format": "summary"
}
```

Then drill into a specific area with semantic search:

```json
{
  "tool": "search",
  "query": "queue processing pipeline",
  "scope": "project",
  "limit": 10
}
```

Confirm a specific implementation detail with grep:

```json
{
  "tool": "grep",
  "pattern": "UnifiedQueueClient::connect",
  "pathGlob": "**/*.rs"
}
```

### Library lookup

Search reference documentation:

```json
{
  "tool": "search",
  "query": "async trait object safety",
  "collection": "libraries",
  "libraryName": "rust-reference",
  "limit": 5
}
```
