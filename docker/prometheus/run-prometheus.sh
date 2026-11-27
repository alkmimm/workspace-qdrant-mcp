#!/bin/sh
set -eu

TOKEN_FILE=/tmp/mcp_metrics.token
QDRANT_TOKEN_FILE=/tmp/qdrant_metrics.token

if [ -z "${MCP_METRICS_TOKEN:-}" ]; then
  echo "MCP_METRICS_TOKEN is required to scrape the MCP metrics endpoint" >&2
  exit 1
fi

umask 077
printf '%s' "$MCP_METRICS_TOKEN" > "$TOKEN_FILE"
printf '%s' "${QDRANT_API_KEY:-}" > "$QDRANT_TOKEN_FILE"

exec /bin/prometheus "$@"
