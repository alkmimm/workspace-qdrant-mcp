# Quick Start

Get workspace-qdrant-mcp running in about 5 minutes. This fork is **container-first**: the daemon
(Rust + static ONNX), the CLI, and the MCP server (TypeScript) all build and run inside Docker — no
local Rust/cargo, ONNX Runtime, or host `npm` build required.

## 1. Clone and configure

```bash
git clone https://github.com/alkmimm/workspace-qdrant-mcp.git
cd workspace-qdrant-mcp
cp docker/.env.example docker/.env
```

Edit `docker/.env`:

- `WQM_DEV_ROOT` — the folder of repos the daemon should watch/index (WSL: a native ext4 path;
  Windows: a host path)
- `MCP_HTTP_TOKEN` — generate one with `openssl rand -hex 32`

## 2. Build and start the stack

```bash
make first-time            # Linux / WSL
# or
make -f Makefile.win first-time   # Windows / PowerShell
```

This creates the SQLite volume, builds the `mcp` and `memexd` images, starts the stack, and installs
Git hooks.

## 3. Verify

```bash
make stack-status                        # compose ps + ping admin/qdrant/daemon
docker exec wqm-memexd wqm status health   # should show "healthy"
```

Open `http://localhost:6335/admin/` for the admin UI.

## 4. Register a project

```bash
docker exec wqm-memexd wqm project register /path/to/your/project
```

The daemon automatically watches the directory, detects files, and indexes them. Check progress:

```bash
docker exec wqm-memexd wqm queue stats       # Watch queue drain to 0
docker exec wqm-memexd wqm project list      # See registered projects
```

## 5. Search

```bash
docker exec wqm-memexd wqm search project "authentication middleware"        # Search the current project
docker exec wqm-memexd wqm search project "handleRequest" --file-type code   # Filter by file type
docker exec wqm-memexd wqm search project "error handling" --include-libs    # Include library content
```

## 6. Connect to Claude

The MCP server is served over streamable HTTP at `http://localhost:6335/mcp`, authenticated with the
`MCP_HTTP_TOKEN` bearer token.

**Claude Code** (native HTTP transport):

```bash
claude mcp add --transport http workspace-qdrant http://localhost:6335/mcp \
  --header "Authorization: Bearer <your-MCP_HTTP_TOKEN>"
```

**Claude Desktop / Codex** (HTTP-only clients need a local stdio↔HTTP proxy):

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

See `templates/fork-kit/` for full config examples (Claude Desktop, Codex).

## Next steps

- [User Manual](user-manual.md) — detailed usage guide
- [CLI Reference](reference/cli.md) — all `wqm` commands
- [MCP Tools Reference](reference/mcp-tools.md) — tool parameters and examples
