# workspace-qdrant-mcp

[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)
[![Fork of ChrisGVE/workspace-qdrant-mcp](https://img.shields.io/badge/fork%20of-ChrisGVE%2Fworkspace--qdrant--mcp-lightgrey.svg)](https://github.com/ChrisGVE/workspace-qdrant-mcp)
[![TypeScript](https://img.shields.io/badge/TypeScript-5.0%2B-blue.svg)](https://www.typescriptlang.org/)
[![Rust](https://img.shields.io/badge/Rust-1.75%2B-orange.svg)](https://www.rust-lang.org/)
[![Qdrant](https://img.shields.io/badge/Qdrant-1.7%2B-red.svg)](https://qdrant.tech)
[![Docker](https://img.shields.io/badge/Docker-container--first-2496ED.svg)](https://www.docker.com/)

**A local, project-aware memory and code-search layer for AI coding assistants.** It runs as an
[MCP](https://modelcontextprotocol.io) server that any MCP client — Claude Code, Claude Desktop, Codex, and
others — connects to, giving the assistant hybrid semantic + keyword search over your indexed repositories, a
code-relationship graph, and a knowledge base that persists across sessions. Everything runs on your machine;
with the local embedding options your code never leaves it.

### Why use it

- **Find code by meaning, not just by string.** Hybrid search (dense embeddings + keyword/BM25, fused with
  Reciprocal Rank Fusion) answers *"where is auth handled?"* as well as *"grep this identifier"* — faster and
  more accurate than an assistant reading files blindly.
- **Understand structure before editing.** The code graph answers *"what calls X?"*, *"what breaks if I change
  Y?"*, and *"what are the most central symbols?"* from real extracted relationships, not guesses.
- **Stop re-learning the codebase every session.** The assistant records findings, design rationale, and your
  standing preferences (`scratchpad` + `rules`) and retrieves them next time — cross-session memory instead of
  cold starts.
- **Project-scoped and multi-repo.** Automatic Git-project detection keeps each repository's index isolated;
  search one project or across all of them.
- **Local and private.** Container-first, runs entirely on your host — and **no GPU is required** (see
  [Running without a GPU](#running-without-a-gpu)).

This is a development fork of
[ChrisGVE/workspace-qdrant-mcp](https://github.com/ChrisGVE/workspace-qdrant-mcp), rewritten with a
TypeScript MCP server and a Rust daemon/CLI. It's a work in progress with no stability or backward-compatibility
guarantees — see [CLAUDE.md](CLAUDE.md) for the architecture this fork actually runs today.

## Features

- **Hybrid Search** - Combines semantic similarity with keyword matching using Reciprocal Rank Fusion
- **Project Detection** - Automatic Git repository awareness and project-scoped collections
- **11 MCP Tools** - search, retrieve, rules, store, scratchpad, grep, list, graph, embedding, search_eval, workspace_index
- **Code Intelligence** - Tree-sitter semantic chunking + LSP integration for active projects
- **Code Graph** - Relationship graph with algorithms (PageRank, community detection, betweenness centrality), dependency-cycle detection, and test-gap analysis (production symbols no test reaches)
- **High-Performance CLI** - Rust-based `wqm` command-line tool
- **Background Daemon** - `memexd` for continuous file monitoring and processing

## Quick Start

This fork is **container-first**: every build (Rust daemon + static ONNX, TypeScript MCP server) runs
inside Docker. No local Rust toolchain, ONNX Runtime, or host `npm` build is required for normal use.

### Prerequisites

- **Docker** and **Docker Compose**
- On WSL, run from a native ext4 path (e.g. `/home/<you>/repos/...`), not a `/mnt/c/...` or UNC path — see
  [CLAUDE.md](CLAUDE.md) for why cross-filesystem I/O is much slower there.

### Install

```bash
git clone https://github.com/alkmimm/workspace-qdrant-mcp.git
cd workspace-qdrant-mcp
cp docker/.env.example docker/.env
# Edit docker/.env: set WQM_DEV_ROOT (the folder of repos the daemon should watch)
# and MCP_HTTP_TOKEN (generate with `openssl rand -hex 32`)

make first-time            # Linux / WSL
# or
make -f Makefile.win first-time   # Windows / PowerShell
```

This builds the `mcp` and `memexd` images, starts the stack, and installs Git hooks. See
`make help` for the full target list, and [Installation Reference](docs/reference/installation.md) /
[Windows Installation Guide](docs/reference/windows-installation.md) for platform-specific notes.

### Running without a GPU

A GPU is **optional** — it only speeds up embedding generation. There are two CPU-only paths; pick one in
`docker/.env` before `make first-time` (or run `make redeploy` after changing it).

**1. Zero-setup — FastEmbed (in-process, simplest).** No embedding service and no separate model server: the
daemon embeds in-process with `all-MiniLM-L6-v2` (384-dim). Retrieval quality is lower than the
code-specialized models, but it runs anywhere with no extra moving parts.

```bash
WQM_EMBEDDING_PROVIDER=fastembed
WQM_FASTEMBED_CACHE_DIR=./.fastembed_cache
```

**2. CPU embedding backend (higher quality).** Run a real embedding model on CPU via the compose
`embeddings-cpu` profile — slower than a GPU but noticeably better retrieval than FastEmbed.

```bash
WQM_EMBEDDING_PROVIDER=openai_compatible
OPENAI_API_KEY=local-dev-no-auth   # dummy: the local backend needs a non-empty value, not real auth
COMPOSE_PROFILES=embeddings-cpu
```

**With a GPU (optional).** Add the `embeddings-gpu` profile to prefer the GPU with the CPU backend as a warm
standby: `COMPOSE_PROFILES=embeddings-cpu,embeddings-gpu`. This requires the
[NVIDIA Container Toolkit](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/) on the Docker
engine (a working `nvidia-smi` inside WSL is **not** sufficient). Switching CPU↔GPU never requires
re-indexing — vectors are model-bound, not device-bound.

See [Embeddings deployment guide](docs/deployment/embeddings.md) for the full model/dimension reference and
how to change the served model.

### Configure MCP

The MCP server is served over streamable HTTP at `http://localhost:6335/mcp`, authenticated with the
`MCP_HTTP_TOKEN` bearer token set in `docker/.env`.

**Claude Code** (native HTTP transport):

```bash
claude mcp add --transport http workspace-qdrant http://localhost:6335/mcp \
  --header "Authorization: Bearer <your-MCP_HTTP_TOKEN>"
```

**Claude Desktop / Codex** (HTTP-only clients that need a local stdio↔HTTP proxy — see `templates/fork-kit/`
for full examples):

```json
{
  "mcpServers": {
    "workspace-qdrant": {
      "command": "npx",
      "args": [
        "mcp-remote",
        "http://localhost:6335/mcp",
        "--header",
        "Authorization: Bearer <your-MCP_HTTP_TOKEN>"
      ]
    }
  }
}
```

### Verify

```bash
make stack-status                       # compose ps + ping admin/qdrant/daemon
docker exec wqm-memexd wqm status health   # wqm CLI runs inside the daemon container
```

Open `http://localhost:6335/admin/` for the admin UI.

### CLAUDE.md Integration

Add the following to your project's `CLAUDE.md` (or your global `~/.claude/CLAUDE.md`) so Claude Code uses workspace-qdrant proactively:

````markdown
## workspace-qdrant

The `workspace-qdrant` MCP server provides codebase-aware search, a library knowledge base, a scratchpad for accumulated insights, and persistent behavioral rules. The tool schemas are self-describing; these instructions cover *when* and *how* to use them.

### Primary Search and Knowledge Base

**Use `workspace-qdrant` first whenever context is uncertain** — first session on a project, returning after a significant gap, or exploring an unfamiliar subsystem. It is faster and more accurate than walking files manually, and it retrieves findings from prior sessions that would otherwise be lost.

**Three-step protocol:**
1. **Search** with `workspace-qdrant` (`search`, `grep`, `list`, or `retrieve`)
2. **Fall back** to `Grep`, `Glob`, `WebSearch` only when workspace-qdrant is insufficient or unavailable
3. **Store** any new findings, analysis, or design rationale via `store` so they are retrievable in future sessions

When a fresh handover or strong prior context already covers what you need, skip the exploratory search — but always store new findings at the end.

**Collections and their purpose:**
- `projects` — indexed codebase; use `scope="project"` (current project) or `scope="all"` (across all projects)
- `libraries` — external reference docs, API specs, third-party documentation; add via `store` with `collection="libraries"` and search with `includeLibraries=true`
- `scratchpad` — analysis, design rationale, research transcripts, architectural insights; complements session handovers by building a growing, semantically searchable knowledge layer across sessions
- `rules` — persistent behavioral rules; load at session start via `rules` → `action="list"`

**Practical notes:**
- Use `grep` for exact strings or regex; `list` with `format="summary"` to explore project structure
- Store external docs or specs into `libraries` so they are searchable alongside code
- Use the scratchpad to record *why* decisions were made, not just *what* was done — future sessions can retrieve the reasoning

### Sub-Agents

Sub-agents start with only the prompt you give them — they have no session history or handover context. They must always use `workspace-qdrant` first for any code exploration, without exception. Include this verbatim in every agent prompt:

> "You have no prior context about this codebase. Use `workspace-qdrant` as your mandatory first tool for ALL code searches — symbols, functions, architecture, patterns, prior findings. Use `search`, `grep`, `list`, or `retrieve` before touching any file with Read/Grep/Glob. Store any new findings, analysis, or design rationale via `store` (scratchpad for insights, libraries for reference docs) so they persist for future sessions."

### Project Registration

At session start, check whether the current project is registered with workspace-qdrant. If it is not, ask the user whether they want to register it (do not register silently). Once registered, the daemon handles file watching and ingestion automatically — no further action is needed.

### Behavioral Rules

The `rules` tool manages persistent rules that are injected into context across sessions. Rules are **user-initiated only** — add rules when the user explicitly instructs you to, never autonomously. Use `action="list"` at session start to load active rules.

### Issue Reporting

workspace-qdrant is under active development. If you encounter errors, unexpected behavior, or limitations with any workspace-qdrant tool, report them as GitHub issues at https://github.com/alkmimm/workspace-qdrant-mcp/issues using the `gh` CLI.
````

## MCP Tools

| Tool | Purpose |
|------|---------|
| `search` | Hybrid semantic + keyword search across indexed content |
| `retrieve` | Direct document lookup by ID or metadata filter |
| `rules` | Manage persistent behavioral rules |
| `store` | Store content, register projects, save notes |
| `scratchpad` | List, update, or delete scratchpad entries (analysis, design rationale) |
| `grep` | Exact substring or regex search using FTS5 |
| `list` | List project files and folder structure |
| `graph` | Navigate the code-relationship graph (callers, impact, centrality, cycles, test gaps) |
| `embedding` | Generate vector embeddings for text |
| `search_eval` | Evaluate search quality (hit@k, recall) against a case set |
| `workspace_index` | Manage the indexed-project registry and branch sync |

See [MCP Tools Reference](docs/reference/mcp-tools.md) for parameters and examples.

## Collections

| Collection | Purpose | Isolation |
|------------|---------|-----------|
| `projects` | Project code and documentation | Multi-tenant by `tenant_id` |
| `libraries` | Reference documentation (books, papers, docs) | Multi-tenant by `library_name` |
| `rules` | Behavioral rules and preferences | Multi-tenant by `project_id` |
| `scratchpad` | Temporary working storage | Per-session |

## CLI Reference

Run these via `docker exec wqm-memexd wqm ...` (the container owns the authoritative SQLite state), or
directly if you've built `wqm` natively — see [scripts/windows/wqm-docker.cmd](scripts/windows/wqm-docker.cmd)
for a wrapper that makes a host-installed `wqm` transparently route through the container.

```bash
# Service management
wqm service start              # Start background daemon
wqm service status             # Check daemon status
wqm status health              # System health check

# Search and content
wqm search project "query"     # Search the current project
wqm ingest file path.py        # Ingest a file
wqm rules list                 # List behavioral rules

# Project and library
wqm project list               # List registered projects
wqm project watch pause        # Pause file watchers
wqm library list               # List libraries
wqm tags list                  # List tags with counts

# Administration
wqm admin collections list     # List collections
wqm admin rebuild all          # Rebuild all indexes
wqm admin backup create        # Backup snapshots
wqm admin stats overview       # Search analytics

# Code graph
wqm graph stats --tenant <t>   # Node/edge counts
wqm graph query --node-id <id> --tenant <t> --hops 2   # Related nodes
wqm graph impact --symbol <name> --tenant <t>           # Impact analysis
wqm graph pagerank --tenant <t> --top-k 20              # PageRank centrality

# Setup
wqm init completions zsh       # Shell completions
wqm init man install           # Install man pages
wqm init hooks install         # Install Claude Code hooks (respects CLAUDE_CONFIG_DIR)

# Queue and monitoring
wqm queue stats                # Queue statistics
```

See [CLI Reference](docs/reference/cli.md) for complete documentation.

## Configuration

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `QDRANT_URL` | `http://localhost:6333` | Qdrant server URL |
| `QDRANT_API_KEY` | - | API key (required for Qdrant Cloud) |
| `WQM_EMBEDDING_PROVIDER` | `openai_compatible` | Set to `fastembed` for local runs without an API key |
| `WQM_EMBEDDING_MODEL_CACHE_DIR` | - | FastEmbed model cache directory |

For a local FastEmbed setup, point `WQM_EMBEDDING_MODEL_CACHE_DIR` and `HF_HOME` at the same writable folder and use the Windows helper target `make -f Makefile.win service-stabilize-fastembed`.

### Claude Code Integration

`wqm init hooks` reads and writes Claude Code's `settings.json`. The
location is resolved from:

| Variable | Default | Description |
|----------|---------|-------------|
| `CLAUDE_CONFIG_DIR` | `~/.claude` | Claude Code config directory used by `wqm init hooks install/uninstall/status`. Set this for Claude Code Enterprise or any non-default install. |

Example — Claude Code Enterprise:

```bash
export CLAUDE_CONFIG_DIR=~/.config/claude/claude-ent
wqm init hooks install
```

## Observability

The daemon exposes metrics and traces. Both are disabled by default.

### Prometheus (`/metrics`, pull)

Enable via config or env var, then scrape:

```yaml
# in the daemon config
observability:
  telemetry:
    prometheus:
      enabled: true
      port: 9464
      bind: 0.0.0.0
```

or:

```bash
WQM_PROMETHEUS_ENABLED=true WQM_PROMETHEUS_PORT=9464 memexd --foreground
curl http://localhost:9464/metrics | head
```

The `--metrics-port <N>` CLI flag is a shortcut that forces
`enabled=true` and overrides the port. See
`docs/observability/prometheus-scrape-example.yaml` for a
`scrape_configs` snippet and
`docs/observability/memexd-telemetry-dashboard.json` for a Grafana 10
dashboard.

### Provisioned Grafana stack (docker compose)

The compose stack ships Grafana with Prometheus, Loki, and Tempo datasources
and hot-provisions every dashboard in `docker/grafana/dashboards/` (30s file
provider — drop a JSON in, it goes live; no rebuild or restart). Notable
boards:

- **WQM — Reconciliation & Index Hygiene** (`reconcile-hygiene.json`) —
  Loki-derived loop detectors for the reconcile/prune subsystem: reconcile
  convergence per tenant, branch-tag removal rates, delete-guard activity,
  walk failures. Thresholds encode incident signatures (a steady reconcile
  rate with an identical stale count = a non-convergent loop). Born from the
  2026-07-16 #224 incident, which ran for weeks with zero dashboard
  visibility; it surfaced two more live bugs (#280, #284) in its first hour.
  First-class Prometheus counters for these signals are tracked in #283.
- **WQM — System Overview / memexd / Qdrant / Token Economy** and friends —
  queue, embedding, gRPC, graph, and token-economy panels.

### Index-coverage guardrail

`indexing_status` counts only walk-eligible files, so it can report "complete"
while git-tracked files are invisible to the index (e.g. gitignore allowlist
rot — see `docs/specs/06-file-watching.md`). Before trusting zero-hits in any
coverage-sensitive audit:

```bash
scripts/tracked_but_unindexed.sh /abs/path/to/repo [prefix/]
# exit 0 = coverage ok; exit 2 = tracked-but-unindexed files exist (listed)
```

### OTLP traces (push)

`#[tracing::instrument]` spans on the queue processor, watcher, gRPC,
embedding, and Qdrant paths are exported over OTLP/gRPC when:

```yaml
observability:
  telemetry:
    service_name: memexd
    otlp:
      enabled: true
      endpoint: http://collector.example:4317
      protocol: grpc   # http/protobuf is also recognized (logs a warning)
      sample_rate: 0.1
```

Standard OpenTelemetry env vars are honored: `OTEL_SERVICE_NAME`,
`OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_PROTOCOL`,
`OTEL_EXPORTER_OTLP_HEADERS`, `OTEL_TRACES_SAMPLER_ARG`.

OTLP metrics export is **not** currently implemented — Prometheus is
the canonical metrics surface.

## Architecture

```
                    +-----------------+
                    |  Claude/Client  |
                    +--------+--------+
                             |
                    +--------v--------+
                    |   MCP Server    |  (TypeScript)
                    +--------+--------+
                             |
              +--------------+--------------+
              |                             |
     +--------v--------+           +--------v--------+
     |   Rust Daemon   |           |     Qdrant      |
     |    (memexd)     |           | Vector Database |
     +--------+--------+           +-----------------+
              |
     +--------v--------+
     |  File Watcher   |
     |  Code Graph     |
     |  Embeddings     |
     +-----------------+
```

The Rust daemon handles file watching, embedding generation, code graph extraction, and queue processing. All writes route through the daemon for consistency.

## Documentation

**User guides:**
- [Quick Start](docs/quick-start.md) — get running in 5 minutes
- [User Manual](docs/user-manual.md) — full usage guide
- [LLM Integration](docs/reference/mcp-best-practices.md) — best practices for Claude

**Reference:**
- [Installation](docs/reference/installation.md) | [Windows](docs/reference/windows-installation.md)
- [CLI Reference](docs/reference/cli.md) — all `wqm` commands
- [MCP Tools](docs/reference/mcp-tools.md) — tool parameters and examples
- [Configuration](docs/reference/configuration.md) — all options and defaults
- [Architecture](docs/reference/architecture.md) — component overview

See the [Documentation Index](docs/INDEX.md) for specifications, ADRs, and developer resources.

## Development

**Container-first.** All builds run inside Docker — no local Rust/cargo, ONNX
Runtime, or host `npm` build required. The daemon (Rust + static ONNX) and the
TypeScript MCP server are compiled in their respective Dockerfiles.

```bash
# Linux / WSL (top-level Makefile, bash + docker compose):
make first-time     # from scratch: build images + start the stack + install hooks
make redeploy       # rebuild + recreate mcp + memexd after code changes
make stack-status   # ps + endpoint health
make help           # all targets

# Windows / PowerShell:
make -f Makefile.win redeploy

# Equivalent raw compose command:
docker compose --env-file docker/.env -f docker-compose.yml up -d --build
```

The daemon's watch root is `WQM_DEV_ROOT` in `docker/.env` (WSL: a native ext4
path; Windows: a host path) — see `docker/.env.example`.

<details>
<summary>Advanced: native (non-Docker) Rust/TS build</summary>

Only needed when iterating outside containers with a local toolchain. The Rust
build requires a static ONNX Runtime via `ORT_LIB_LOCATION` (see `CLAUDE.md` →
"ONNX Runtime Build Requirements").

```bash
# TypeScript MCP server
cd src/typescript/mcp-server && npm install && npm run build && npm test

# Rust daemon and CLI (from src/rust/) — needs ORT_LIB_LOCATION set
cargo build --release
cargo test

# Graph benchmarks
cargo bench --package workspace-qdrant-core --bench graph_bench

# Binaries output to: target/release/{wqm,memexd}
```

</details>

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup and guidelines.

## License

MIT License - see [LICENSE](LICENSE) for details.

---

*Forked from [ChrisGVE/workspace-qdrant-mcp](https://github.com/ChrisGVE/workspace-qdrant-mcp), itself inspired by [claude-qdrant-mcp](https://github.com/marlian/claude-qdrant-mcp)*
