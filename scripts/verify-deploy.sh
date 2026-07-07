#!/usr/bin/env bash
# Post-redeploy verification — confirm the RUNNING stack is from the latest build,
# the daemon-side knobs are wired, and (optionally) a specific code change is in
# the deployed binary.
#
# Why this exists: after `make redeploy` you want proof the new code is actually
# live. A stale MCP client and the denied `docker exec` rule out the obvious
# checks, so this leans on `docker cp` + `docker inspect` (both allowed) instead.
#
# Usage:
#   scripts/verify-deploy.sh                  # freshness + env wiring + health
#   scripts/verify-deploy.sh "branch bulk"    # ALSO assert the deployed memexd
#                                             # binary contains that string — the
#                                             # robust way to confirm a specific
#                                             # Rust fix landed (log line, env-var
#                                             # name, or embedded SQL fragment).
#
# Exit code is non-zero if any check fails or warns, so it is CI/script-friendly.
set -uo pipefail

MARKER="${1:-}"
MCP_HTTP_PORT="${MCP_HTTP_PORT:-6335}"
QDRANT_HTTP_PORT="${QDRANT_HTTP_PORT:-6333}"
MEMEXD_GRPC_PORT="${MEMEXD_GRPC_PORT:-50051}"

warn=0
say() { printf '  %-8s %s\n' "$1" "$2"; }

# memexd -> workspace-qdrant-mcp-memexd:local ; mcp -> workspace-qdrant-mcp:local
declare -A IMG=(
  [wqm-memexd]="workspace-qdrant-mcp-memexd:local"
  [wqm-mcp]="workspace-qdrant-mcp:local"
)

echo "=== 1. image freshness (container recreated after the image was built?) ==="
for c in wqm-memexd wqm-mcp; do
  c_created=$(docker inspect -f '{{.Created}}' "$c" 2>/dev/null)
  i_created=$(docker image inspect -f '{{.Created}}' "${IMG[$c]}" 2>/dev/null)
  if [[ -z "$c_created" ]]; then
    say "[FAIL]" "$c is not running"; warn=$((warn + 1))
  elif [[ -z "$i_created" ]]; then
    say "[?]" "$c running, but local image ${IMG[$c]} not found to compare"
  else
    # NEVER compare the .Created strings lexically: docker emits the CONTAINER's
    # in UTC ('...Z') but the IMAGE's in local time ('...-03:00'), so a string
    # compare is a timezone-format bug (false 'fresh' when the image is newer in
    # absolute time). Normalize both to epoch seconds with `date -d`.
    c_epoch=$(date -d "$c_created" +%s 2>/dev/null || echo 0)
    i_epoch=$(date -d "$i_created" +%s 2>/dev/null || echo 0)
    if [[ "$c_epoch" -eq 0 || "$i_epoch" -eq 0 ]]; then
      say "[?]" "$c: could not parse .Created timestamps to compare"
    elif [[ "$c_epoch" -ge "$i_epoch" ]]; then
      say "[OK]" "$c recreated after its image build (fresh)"
    else
      say "[STALE]" "$c predates the latest ${IMG[$c]} build — run 'make redeploy'"; warn=$((warn + 1))
    fi
  fi
done

echo ""
echo "=== 2. graph-centrality knobs reach the daemon container ==="
env_dump=$(docker inspect wqm-memexd --format '{{range .Config.Env}}{{println .}}{{end}}' 2>/dev/null)
for v in WQM_GRAPH_CENTRALITY_EXCLUDE WQM_GRAPH_CENTRALITY_GENERIC_THRESHOLD WQM_GRAPH_CENTRALITY_SKIP_SYMBOLS WQM_GRAPH_CENTRALITY_USAGE_THRESHOLD; do
  line=$(grep "^$v=" <<<"$env_dump" || true)
  if [[ -n "$line" ]]; then say "[OK]" "$line"; else say "[MISS]" "$v NOT wired into the memexd container"; warn=$((warn + 1)); fi
done

if [[ -n "$MARKER" ]]; then
  echo ""
  echo "=== 3. deployed artifacts contain: '$MARKER' (memexd binary + mcp bundle) ==="
  # A fix can land in EITHER product: a Rust fix in the memexd binary, or a
  # TypeScript fix in the mcp `dist` bundle. Check both, pass if the marker is in
  # either, and report where — so `MARKER=` verifies a TS fix (e.g. a new message
  # string / function name) just as robustly as a Rust one. Use `docker cp` (not
  # `docker exec`, which a deny rule may block) into a fixed pre-removed /tmp path.
  found_in=""

  # ── memexd binary ──
  bin="/tmp/wqm-memexd-verify.bin"
  rm -f "$bin"
  if docker cp wqm-memexd:/usr/local/bin/memexd "$bin" 2>/dev/null; then
    # Count, do NOT `grep -q`: under `set -o pipefail`, grep -q exits at the first
    # match and SIGPIPEs `strings` (exit 141), which pipefail then reports as a
    # pipeline failure — a false [MISS] even when the marker is present.
    hits=$(strings -n 6 "$bin" | grep -cF "$MARKER" || true)
    [[ "${hits:-0}" -gt 0 ]] && found_in="memexd binary"
  else
    say "[?]" "docker cp of memexd failed — cannot inspect the binary"
  fi
  rm -f "$bin"

  # ── mcp dist bundle (compiled TypeScript) ──
  dist="/tmp/wqm-mcp-verify-dist"
  rm -rf "$dist"
  if docker cp wqm-mcp:/app/src/typescript/mcp-server/dist "$dist" 2>/dev/null; then
    # -l stops at the first matching FILE (not line), so no SIGPIPE-into-strings
    # concern here; restrict to .js so source maps / .d.ts don't give false hits.
    if grep -rlF --include='*.js' "$MARKER" "$dist" >/dev/null 2>&1; then
      found_in="${found_in:+$found_in + }mcp bundle"
    fi
  else
    say "[?]" "docker cp of mcp dist failed — cannot inspect the bundle"
  fi
  rm -rf "$dist"

  if [[ -n "$found_in" ]]; then
    say "[OK]" "marker present in: $found_in"
  else
    say "[MISS]" "marker ABSENT from both memexd binary and mcp bundle — the running artifacts predate this change"; warn=$((warn + 1))
  fi
fi

echo ""
echo "=== 4. endpoint health ==="
if curl -fsS -o /dev/null -m 3 "http://localhost:$MCP_HTTP_PORT/admin/init"; then say "[OK]" "MCP /admin/init"; else say "[FAIL]" "MCP /admin/init"; warn=$((warn + 1)); fi
if curl -fsS -o /dev/null -m 3 "http://localhost:$QDRANT_HTTP_PORT/collections"; then say "[OK]" "qdrant /collections"; else say "[FAIL]" "qdrant /collections"; warn=$((warn + 1)); fi
if (exec 3<>/dev/tcp/localhost/"$MEMEXD_GRPC_PORT") 2>/dev/null; then say "[OK]" "memexd gRPC :$MEMEXD_GRPC_PORT"; else say "[FAIL]" "memexd gRPC :$MEMEXD_GRPC_PORT"; warn=$((warn + 1)); fi

echo ""
if [[ $warn -eq 0 ]]; then
  echo "verify-deploy: all checks passed."
else
  echo "verify-deploy: $warn warning(s)/failure(s) — see [STALE]/[MISS]/[FAIL] above."
fi
exit $((warn > 0 ? 1 : 0))
