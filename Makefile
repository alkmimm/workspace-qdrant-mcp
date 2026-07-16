# Makefile - Linux/WSL operations for the workspace-qdrant-mcp fork.
#
# Container-first workflow: every build happens INSIDE Docker. You do NOT need
# a local Rust/cargo toolchain, a local ONNX Runtime, or a host npm build —
# `docker/Dockerfile.memexd` builds the daemon (Rust + static ONNX) and the
# root `Dockerfile` compiles the TypeScript MCP server and the Rust node addon.
#
# Run this from inside the WSL distro (native ext4), e.g.:
#   wsl -d ubuntu-24.04
#   cd /home/alkmimm/respositorios/workspace-qdrant-mcp
#   make help
#
# The Windows/PowerShell flow lives in `Makefile.win` (use `make -f Makefile.win`).
# This file is the Linux/WSL counterpart and intentionally has NO cargo/npm host
# targets — wiring the Windows side to build via the container is a future step.
#
# Useful variables (override on the command line, e.g. `make redeploy LOG_TAIL=100`):
#   REPO              repo root (default: this Makefile's directory)
#   COMPOSE_ENV_FILE  compose env file (default: docker/.env)
#   COMPOSE_FILE      compose file    (default: docker-compose.yml)
#   MCP_HTTP_PORT     host port for the MCP HTTP/admin endpoint (default: 6335)
#   QDRANT_HTTP_PORT  host port for Qdrant REST (default: 6333)
#   LOG_TAIL          lines for stack-logs (default: 50)
#   MARKER            string for `verify-deploy` to grep in the deployed memexd
#                     binary (default: empty = skip the binary check)

SHELL := /usr/bin/env bash
.SHELLFLAGS := -eu -o pipefail -c

REPO ?= $(patsubst %/,%,$(dir $(abspath $(lastword $(MAKEFILE_LIST)))))
COMPOSE_FILE ?= $(REPO)/docker-compose.yml
COMPOSE_ENV_FILE ?= $(REPO)/docker/.env
MCP_HTTP_PORT ?= 6335
QDRANT_HTTP_PORT ?= 6333
MEMEXD_GRPC_PORT ?= 50051
CODEX_HOME ?= $(HOME)/.codex
CODEX_BIN ?= codex
CODEX_MCP_NAME ?= workspace-qdrant
CODEX_MCP_URL ?= http://localhost:$(MCP_HTTP_PORT)/mcp
CODEX_BEARER_TOKEN_ENV_VAR ?= MCP_HTTP_TOKEN
LOG_TAIL ?= 50
MARKER ?=

# Single source of truth for every compose invocation.
COMPOSE := docker compose --env-file "$(COMPOSE_ENV_FILE)" -f "$(COMPOSE_FILE)"
COMPOSE_BUILDER ?= default

# Build provenance for the mcp image build-info (the Docker context omits .git,
# so `git` cannot run inside the image). Exported so every `$(COMPOSE) build`
# recipe passes it through docker-compose.yml's `build.args`. Best-effort SHA.
# Only the SHA is exported (stable per commit → cache-friendly); the build
# timestamp is stamped inside the image by generate-build-info.ts, so it never
# busts the Docker build cache (see Dockerfile).
WQM_BUILD_SHA ?= $(shell git -C "$(REPO)" rev-parse --short HEAD 2>/dev/null || echo unknown)
export WQM_BUILD_SHA

MCP_HEALTH_URL ?= http://localhost:$(MCP_HTTP_PORT)/admin/api/health
MCP_INIT_URL ?= http://localhost:$(MCP_HTTP_PORT)/admin/init
MEMEXD_DB_VOLUME ?= workspace-qdrant-mcp_memexd_db
MEMEXD_IMAGE ?= workspace-qdrant-mcp-memexd:local
PROMETHEUS_PORT ?= 9090
DB_BACKUP_DIR ?= $(REPO)/state/backups
# Snapshots are heavy (~7.4 GB live: search.db 4.3G + graph.db 1.8G +
# memexd.db 1.3G), so the default keeps only the last two.
#
# Before raising this, CHECK FREE SPACE — `DB_BACKUP_KEEP=5` (~35 GB) once
# filled the host disk mid-rollout, killed the copy with a misleading
# "unexpected EOF" (ENOSPC in disguise), and left a partial snapshot that
# looked valid (issue #276). backup-db now rotates BEFORE copying and refuses
# to start when the volume cannot hold the snapshot, so a raise that does not
# fit fails loudly instead of wedging the host. One extra snapshot ahead of a
# risky migration is what `redeploy` already takes for you.
DB_BACKUP_KEEP ?= 2

.PHONY: help check-env first-time redeploy \
	stack-up stack-down stack-restart stack-status stack-logs verify-deploy \
	build-images mcp-rebuild memexd-recreate \
	backup-db rehearse-migrations \
	codex-register \
	health-quick scan register-all watch reindex reindex-status hooks-install clean \
	mem-watch-start mem-watch mem-watch-stop

help:
	@echo "============================================================"
	@echo "workspace-qdrant-mcp — Linux/WSL (container-first) targets"
	@echo "============================================================"
	@echo "Stack lifecycle (day-to-day):"
	@echo "  first-time       SETUP FROM SCRATCH: create db volume + build + up + hooks + status"
	@echo "  redeploy         AFTER CODE CHANGES / git pull: rebuild mcp+memexd images + recreate + status"
	@echo "  stack-up         start the docker stack (no rebuild)"
	@echo "  stack-down       stop the docker stack"
	@echo "  stack-restart    down + up"
	@echo "  stack-status     compose ps + ping admin/qdrant/daemon"
	@echo "  stack-logs       tail mcp + memexd logs (LOG_TAIL=$(LOG_TAIL))"
	@echo "  verify-deploy    confirm running stack == latest build + knobs wired + health"
	@echo "  backup-db        WAL-safe snapshot of the live SQLite DBs into state/backups/"
	@echo "  rehearse-migrations  run the new binary's schema migrations against a COPY of the live DBs"
	@echo "                   (MARKER='<str>' also greps the deployed memexd binary)"
	@echo "------------------------------------------------------------"
	@echo "Build / recreate (all builds run INSIDE Docker — no local cargo/npm):"
	@echo "  build-images     docker compose build mcp memexd"
	@echo "  mcp-rebuild      rebuild + recreate ONLY the mcp container"
	@echo "  memexd-recreate  recreate memexd (picks up env changes from docker/.env)"
	@echo "------------------------------------------------------------"
	@echo "Codex integration:"
	@echo "  codex-register   create/update the workspace-qdrant HTTP MCP in Codex"
	@echo "                   (config: $(CODEX_HOME)/config.toml)"
	@echo "------------------------------------------------------------"
	@echo "Observability / indexing:"
	@echo "  health-quick     curl the MCP /admin/api/health endpoint"
	@echo "  scan             list git repos discovered under WQM_DEV_ROOT (no register)"
	@echo "  register-all     register every discovered repo with the daemon (starts indexing)"
	@echo "  watch            poll indexing progress until all projects drain (or timeout)"
	@echo "  reindex          trigger a full reindex of the watched projects (admin API)"
	@echo "  reindex-status   per-project indexing progress (one-shot)"
	@echo "  mem-watch-start  start the background memory-growth sampler (detached)"
	@echo "  mem-watch        analyze memory samples: per-container trend + leak projection"
	@echo "  mem-watch-stop   stop the background memory sampler"
	@echo "  hooks-install    install POSIX git hooks into .wqm-fork/git-hooks"
	@echo "  clean            remove the MCP dist build artifacts"
	@echo "------------------------------------------------------------"
	@echo "Watch root (daemon-observed projects) is set via WQM_DEV_ROOT in"
	@echo "$(COMPOSE_ENV_FILE). For WSL use a native ext4 path, e.g."
	@echo "  WQM_DEV_ROOT=/home/<user>/respositorios"
	@echo "============================================================"

check-env:
	@if [[ ! -f "$(COMPOSE_ENV_FILE)" ]]; then \
		echo "ERROR: $(COMPOSE_ENV_FILE) not found. Copy docker/.env.example to docker/.env first." >&2; \
		exit 1; \
	fi

# ── Stack lifecycle ──────────────────────────────────────────────────────────

first-time: check-env
	@echo "=== First-time setup (container-first) ==="
	@echo "Step 1/4: ensure the external SQLite volume exists"
	@docker volume create "$(MEMEXD_DB_VOLUME)" >/dev/null
	@echo "Step 2/4: build + start the whole stack"
	@cd "$(REPO)" && extra=(); if $(COMPOSE) build --help 2>/dev/null | grep -q -- '--builder'; then extra=(--builder "$(COMPOSE_BUILDER)"); fi; $(COMPOSE) build "$${extra[@]}" mcp memexd
	@cd "$(REPO)" && $(COMPOSE) up -d mcp memexd
	@echo "Step 3/4: install POSIX git hooks"
	@$(MAKE) -f "$(lastword $(MAKEFILE_LIST))" hooks-install
	@echo "Step 4/4: status"
	@sleep 8
	@$(MAKE) -f "$(lastword $(MAKEFILE_LIST))" stack-status
	@echo ""
	@echo "=== Done. Open http://localhost:$(MCP_HTTP_PORT)/admin/ ==="

redeploy: check-env
	@echo "=== Redeploy after code changes (build runs inside Docker) ==="
	@echo "Step 1/6: rebuild mcp + memexd images"
	@docker volume create "$(MEMEXD_DB_VOLUME)" >/dev/null
	@cd "$(REPO)" && extra=(); if $(COMPOSE) build --help 2>/dev/null | grep -q -- '--builder'; then extra=(--builder "$(COMPOSE_BUILDER)"); fi; $(COMPOSE) build "$${extra[@]}" mcp memexd
	@echo "Step 2/6: snapshot live databases (rollback insurance)"
	@$(MAKE) -f "$(lastword $(MAKEFILE_LIST))" backup-db
	@echo "Step 3/6: rehearse pending schema migrations against a copy of the live DBs"
	@$(MAKE) -f "$(lastword $(MAKEFILE_LIST))" rehearse-migrations
	@echo "Step 4/6: recreate mcp + memexd (env may have changed)"
	@cd "$(REPO)" && $(COMPOSE) up -d --force-recreate mcp memexd
	@echo "Step 5/6: reinstall git hooks (idempotent — lives in the repo, not the image)"
	@$(MAKE) -f "$(lastword $(MAKEFILE_LIST))" hooks-install
	@echo "Step 6/6: status"
	@sleep 6
	@$(MAKE) -f "$(lastword $(MAKEFILE_LIST))" stack-status
	@echo ""
	@echo "=== Redeploy complete ==="

# ── Migration safety (born from the 2026-07-15 v48 incident) ────────────────
# A schema migration that passed clippy + unit gates crash-looped production.
# Two mechanized guards now run inside `redeploy`, between build and up:
#   backup-db            WAL-safe snapshot of memexd.db/search.db/graph.db
#                        into state/backups/ (rotated, keep DB_BACKUP_KEEP).
#   rehearse-migrations  run the NEWLY BUILT memexd with --migrate-only against
#                        a copy of the live DBs in a throwaway container; any
#                        failure aborts the redeploy while the old (working)
#                        containers keep running.

backup-db: check-env
	@mkdir -p "$(DB_BACKUP_DIR)"
	@ts=$$(date -u +%Y%m%dT%H%M%SZ); \
	docker run --rm --user "$$(id -u):$$(id -g)" \
	  -v "$(MEMEXD_DB_VOLUME)":/live:ro \
	  -v "$(DB_BACKUP_DIR)":/backup \
	  -v "$(REPO)/scripts/migration-rehearsal-copy.py":/copy.py:ro \
	  --entrypoint python3 "$(MEMEXD_IMAGE)" /copy.py /live "/backup/pre-deploy-$$ts" \
	  --keep "$(DB_BACKUP_KEEP)" --rotate-dir /backup \
	&& echo "backup: state/backups/pre-deploy-$$ts"

rehearse-migrations: check-env
	@docker run --rm \
	  -v "$(MEMEXD_DB_VOLUME)":/live:ro \
	  -v "$(REPO)/scripts/migration-rehearsal-copy.py":/copy.py:ro \
	  --entrypoint sh "$(MEMEXD_IMAGE)" -c \
	  'set -e; out=$$(python3 /copy.py /live /tmp/rehearsal); echo "$$out"; \
	   if echo "$$out" | grep -q "^SKIP:"; then exit 0; fi; \
	   WQM_DATABASE_PATH=/tmp/rehearsal/memexd.db /usr/local/bin/memexd \
	     --foreground --migrate-only 2>&1 | tee /tmp/rehearsal.out || true; \
	   grep -q MIGRATIONS_OK /tmp/rehearsal.out'
	@echo "rehearse-migrations: OK (new binary migrated a copy of the live DBs)"

stack-up: check-env
	@cd "$(REPO)" && $(COMPOSE) up -d
	@echo "Stack started. Run 'make stack-status' to verify."

stack-down: check-env
	@cd "$(REPO)" && $(COMPOSE) down

stack-restart: stack-down stack-up

stack-status: check-env
	@echo "=== docker compose ps ==="
	@cd "$(REPO)" && $(COMPOSE) ps
	@echo ""
	@echo "=== endpoints ==="
	@if curl -fsS -o /dev/null -m 3 "$(MCP_INIT_URL)"; then echo "/admin/init     [ok]"; else echo "/admin/init     [fail]"; fi
	@if curl -fsS -o /dev/null -m 3 "http://localhost:$(QDRANT_HTTP_PORT)/collections"; then echo "qdrant          [ok]"; else echo "qdrant          [fail]"; fi
	@if (exec 3<>/dev/tcp/localhost/$(MEMEXD_GRPC_PORT)) 2>/dev/null; then echo "memexd gRPC     [ok] localhost:$(MEMEXD_GRPC_PORT)"; else echo "memexd gRPC     [fail]"; fi
	@echo ""
	@echo "=== prometheus alerts (firing) ==="
	@python3 "$(REPO)/scripts/firing-alerts.py" "http://localhost:$(PROMETHEUS_PORT)" || true

stack-logs: check-env
	@cd "$(REPO)" && $(COMPOSE) logs --tail $(LOG_TAIL) mcp memexd

# Post-redeploy sanity check: are the running containers actually from the latest
# build, are the daemon-side graph-centrality knobs wired, and is the stack
# healthy? `MARKER='<string>'` additionally asserts the deployed memexd binary
# contains that literal (a log line, env-var name, or embedded SQL fragment) —
# the robust, MCP-free way to confirm a specific Rust fix shipped. Uses docker
# cp/inspect (the `docker exec` rule does not apply).
verify-deploy: check-env
	@MCP_HTTP_PORT="$(MCP_HTTP_PORT)" QDRANT_HTTP_PORT="$(QDRANT_HTTP_PORT)" \
		MEMEXD_GRPC_PORT="$(MEMEXD_GRPC_PORT)" \
		bash "$(REPO)/scripts/verify-deploy.sh" "$(MARKER)"

# ── Build / recreate (everything builds inside Docker) ───────────────────────

build-images: check-env
	@cd "$(REPO)" && extra=(); if $(COMPOSE) build --help 2>/dev/null | grep -q -- '--builder'; then extra=(--builder "$(COMPOSE_BUILDER)"); fi; $(COMPOSE) build "$${extra[@]}" mcp memexd

mcp-rebuild: check-env
	@echo "Rebuilding MCP image (TypeScript compiled inside the container)..."
	@cd "$(REPO)" && extra=(); if $(COMPOSE) build --help 2>/dev/null | grep -q -- '--builder'; then extra=(--builder "$(COMPOSE_BUILDER)"); fi; $(COMPOSE) build "$${extra[@]}" mcp
	@cd "$(REPO)" && $(COMPOSE) up -d mcp

memexd-recreate: check-env
	@echo "Recreating memexd container (picks up env changes from docker/.env)..."
	@cd "$(REPO)" && $(COMPOSE) up -d --force-recreate memexd

# Register the containerized Streamable HTTP server in the Linux/WSL Codex
# config. The helper delegates creation/update to the installed Codex CLI, then
# restores this fork's canonical timeouts and enabled-tools allowlist (the CLI's
# `mcp add` command does not expose those fields).
codex-register:
	@python3 "$(REPO)/scripts/linux/register-codex-mcp.py" \
		--repo "$(REPO)" \
		--codex-home "$(CODEX_HOME)" \
		--codex-bin "$(CODEX_BIN)" \
		--server-name "$(CODEX_MCP_NAME)" \
		--url "$(CODEX_MCP_URL)" \
		--bearer-token-env-var "$(CODEX_BEARER_TOKEN_ENV_VAR)"

# ── Observability / indexing ─────────────────────────────────────────────────
#
# Host-side admin calls rely on MCP_HTTP_TRUST_LOCALHOST=1 (the compose default),
# which skips the Bearer check for loopback peers — so no token is needed here.
# JSON is pretty-printed with python3 (jq is not assumed to be installed).

PP := python3 -m json.tool

health-quick: check-env
	@curl -fsS -m 5 "$(MCP_HEALTH_URL)" | $(PP) 2>/dev/null || curl -fsS -m 5 "$(MCP_HEALTH_URL)"

# Discover git repos under the daemon's devRoot (WQM_DEV_ROOT) without registering.
scan: check-env
	@python3 "$(REPO)/scripts/wqm_admin.py" scan "http://localhost:$(MCP_HTTP_PORT)"

# Register EVERY git repo discovered under devRoot with the daemon (idempotent —
# already-registered projects are refreshed). Indexing starts in the background.
register-all: check-env
	@python3 "$(REPO)/scripts/wqm_admin.py" register-all "http://localhost:$(MCP_HTTP_PORT)"

# Poll indexing progress until every project drains (0 pending, 100%) or timeout.
# Override cadence/limit: make watch WATCH_INTERVAL=15 WATCH_MAX=1800
WATCH_INTERVAL ?= 10
WATCH_MAX ?= 900
watch: check-env
	@python3 "$(REPO)/scripts/wqm_admin.py" watch "http://localhost:$(MCP_HTTP_PORT)" $(WATCH_INTERVAL) $(WATCH_MAX)

# Force-rebuild the computed indexes (FTS5, tags, sparse vectors, components,
# keywords) for EVERY watched project. Enumerates tenants from the admin
# snapshot, then POSTs /admin/api/projects/reindex per tenant. Pass TENANT=<id>
# to reindex a single project instead of all.
TENANT ?=
reindex: check-env
	@base="http://localhost:$(MCP_HTTP_PORT)"; \
	if [[ -n "$(TENANT)" ]]; then tenants="$(TENANT)"; else \
		tenants=$$(python3 "$(REPO)/scripts/wqm_admin.py" tenants "$$base"); \
	fi; \
	if [[ -z "$$tenants" ]]; then echo "No watched projects found — nothing to reindex."; exit 0; fi; \
	for t in $$tenants; do \
		echo "==> reindex tenant $$t"; \
		curl -fsS -m 60 -X POST -H "Content-Type: application/json" \
			-d "{\"tenantId\":\"$$t\"}" "$$base/admin/api/projects/reindex" | $(PP) 2>/dev/null || true; \
	done; \
	echo "Reindex requested for all watched projects. Watch 'make reindex-status' / 'make stack-logs'."

# Per-project indexing progress (pending / done / total / percent) from snapshot.
reindex-status: check-env
	@python3 "$(REPO)/scripts/wqm_admin.py" status "http://localhost:$(MCP_HTTP_PORT)"

# ── Memory-growth watch (memexd soak signal) ─────────────────────────────────
#
# memexd's resident set grows slowly over days under sustained indexing load and
# exports no process RSS to Prometheus, so there's nothing to consult after the
# fact. mem-watch-start logs cgroup memory (charged + anonymous) for the wqm
# containers every WQM_MEM_WATCH_INTERVAL seconds to MEM_WATCH_DIR; mem-watch
# reports the trend. Judge a leak on ANON, not charged (cache). See
# docs/deployment/reliability.md and scripts/mem-watch/README.md.
MEM_WATCH_DIR ?= $(HOME)/.wqm-mem-watch

mem-watch-start:
	@setsid nohup bash "$(REPO)/scripts/mem-watch/sampler.sh" >/dev/null 2>&1 & true
	@sleep 1
	@echo "memory sampler launched (logs: $(MEM_WATCH_DIR)/samples.csv). Analyze: make mem-watch"

mem-watch:
	@python3 "$(REPO)/scripts/mem-watch/analyze.py" "$(MEM_WATCH_DIR)/samples.csv"

mem-watch-stop:
	@pid="$(MEM_WATCH_DIR)/sampler.pid"; \
	if [[ -f "$$pid" ]] && kill "$$(cat "$$pid")" 2>/dev/null; then \
		echo "memory sampler stopped (pid $$(cat "$$pid"))"; \
	else \
		echo "no running sampler found ($$pid)"; \
	fi

hooks-install:
	@echo "Installing POSIX git hooks (sh + curl -> MCP HTTP)..."
	@sh "$(REPO)/scripts/git-hooks/install.sh" --repo "$(REPO)" --hooks-dir "$(REPO)/.wqm-fork/git-hooks" --force

clean:
	@rm -rf "$(REPO)/src/typescript/mcp-server/dist"
	@echo "Removed src/typescript/mcp-server/dist"
