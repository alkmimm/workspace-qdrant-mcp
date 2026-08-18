# Dockerfile for workspace-qdrant-mcp MCP server (TypeScript)
#
# This build is intended for the fork's local compose stack:
# it compiles the TypeScript MCP server and the native Rust addon from the
# repository source so local changes are included before `docker compose up`.
#
# Usage:
#   docker build -t workspace-qdrant-mcp:local .
#   docker run -i -e MCP_SERVER_MODE=http workspace-qdrant-mcp:local

ARG NODE_VERSION=20
ARG RUST_VERSION=1

# ---- Build the native addon from source -----------------------------------
FROM rust:${RUST_VERSION}-bookworm AS addon-builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl-dev \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build/src/rust

# Copy the full Rust workspace so the workspace-level Cargo.toml patch layout
# is preserved while building the common-node addon.
COPY src/rust/ ./
COPY assets/ /build/assets/

RUN cargo build -p wqm-common-node --release \
    && mkdir -p /out/src/rust/common-node \
    && cp target/release/libwqm_common_node.so /out/src/rust/common-node/wqm-common-node.linux-x64-gnu.node

# ---- Build the TypeScript MCP server --------------------------------------
FROM node:${NODE_VERSION}-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    curl \
    g++ \
    git \
    make \
    python3 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Assets needed by generate-config-defaults.ts
COPY assets/ assets/

# The gRPC contract. The MCP server does NOT keep its own copy — `copy:proto`
# reads this canonical daemon proto straight into dist/, so the TypeScript
# client and the Rust daemon cannot decode with different schemas. (They did:
# a hand-maintained fork under src/typescript/mcp-server/src/proto/ silently
# dropped every field added on the Rust side — proto3 ignores unknown fields,
# so there was no error, only a missing value.) Copied before the TS sources so
# it keeps its own cache layer: the proto changes far less often than the app.
COPY src/rust/daemon/proto/ src/rust/daemon/proto/

# Native addon loader plus the Linux x64 glibc addon compiled in the previous stage.
COPY src/rust/common-node/index.js src/rust/common-node/index.js
COPY src/rust/common-node/index.d.ts src/rust/common-node/index.d.ts
COPY --from=addon-builder /out/src/rust/common-node/wqm-common-node.linux-x64-gnu.node src/rust/common-node/wqm-common-node.linux-x64-gnu.node

# Install dependencies with a stable cache layer.
COPY src/typescript/mcp-server/package*.json src/typescript/mcp-server/
RUN cd src/typescript/mcp-server && npm ci

# Build provenance passed from the host (Makefile / compose). The build context
# omits .git, so generate-build-info.ts cannot call `git` here — without this the
# compiled build-info is a meaningless constant. Falls back to git/unknown when
# unset (e.g. a bare `docker build`). See scripts/generate-build-info.ts.
#
# ONLY the commit SHA is injected as a build-arg — it is STABLE per commit, so
# this ENV layer (and the source COPY + `npm run build` below it) stays cached
# across successive builds of the same commit. The build TIMESTAMP is deliberately
# NOT a build-arg: it changes every second and would bust this layer on every
# build, forcing a full TypeScript recompile. generate-build-info.ts instead
# stamps `new Date()` when it runs, so BUILD_TIME reflects the last real build.
ARG WQM_BUILD_SHA=unknown
ENV WQM_BUILD_SHA=$WQM_BUILD_SHA

# Copy source and build TypeScript.
COPY src/typescript/mcp-server/ src/typescript/mcp-server/

# gRPC call-surface gate. The TS client invokes RPCs by hardcoded camelCase
# STRING, which no compiler checks: a renamed or typo'd method still builds and
# fails only at runtime with "Unknown method". Sharing one proto (above) fixes
# schema drift but cannot fix a wrong string, so the check still earns its keep.
#
# It runs HERE, in the image build, because this fork's GitHub Actions are
# disabled — the container build IS the CI. The check previously lived only in
# .github/workflows/ci.yml and therefore never ran at all.
COPY scripts/check-proto-consistency.sh scripts/check-proto-consistency.sh
RUN bash scripts/check-proto-consistency.sh

RUN cd src/typescript/mcp-server && npm run build

# Keep only production dependencies in the runtime image.
RUN cd src/typescript/mcp-server && npm prune --omit=dev

# ---- Runtime ---------------------------------------------------------------
FROM node:${NODE_VERSION}-slim

# `git` is required at runtime so the MCP server can detect the current
# branch / worktree state of mounted project roots. `getGitState` shells
# out to `git` (see src/typescript/mcp-server/src/utils/git-utils.ts).
RUN apt-get update && apt-get install -y --no-install-recommends \
    curl \
    git \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /build/src/typescript/mcp-server/dist src/typescript/mcp-server/dist/
COPY --from=builder /build/src/typescript/mcp-server/node_modules src/typescript/mcp-server/node_modules/
COPY --from=builder /build/src/typescript/mcp-server/package.json src/typescript/mcp-server/package.json
COPY --from=builder /build/src/rust/common-node src/rust/common-node

# Run as non-root using the pre-created `node` user from the base image.
USER node

ENV NODE_ENV=production
ENV WQM_LOG_LEVEL=info

LABEL org.opencontainers.image.title="workspace-qdrant-mcp"
LABEL org.opencontainers.image.description="Project-scoped semantic workspace memory MCP server with Qdrant hybrid search"
LABEL org.opencontainers.image.source="https://github.com/ChrisGVE/workspace-qdrant-mcp"
LABEL org.opencontainers.image.licenses="MIT"

WORKDIR /app/src/typescript/mcp-server
ENTRYPOINT ["node", "dist/index.js"]
