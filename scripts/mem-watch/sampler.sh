#!/usr/bin/env bash
#
# sampler.sh — long-running memory sampler for the workspace-qdrant-mcp stack.
#
# Why this exists: memexd's resident set has been observed to grow slowly over
# days under sustained indexing load (tens of GiB before a restart recovers it).
# memexd does NOT export process RSS to Prometheus (only app-level `memexd_*`
# metrics), so there is no time series to consult after the fact. This sampler
# is the missing soak signal — see docs/deployment/reliability.md.
#
# What it records (cgroup v2 `memory.current` + the `anon` line of
# `memory.stat`, the same accounting `docker stats` reads):
#   * <name>_cur_gib   total charged memory (INCLUDES reclaimable page cache)
#   * <name>_anon_gib  anonymous memory — the non-reclaimable part that a real
#                      leak shows up in. Judge the trend on ANON, not cur.
# plus host MemAvailable. One CSV row per INTERVAL seconds.
#
# Ground truth is the host cgroup, resolved from each container's init PID so it
# works under both the systemd and cgroupfs docker drivers. If the cgroup files
# are unreachable (e.g. Docker Desktop's VM), it falls back to `docker stats`
# for the working set and leaves anon blank.
#
# Config (env overrides):
#   WQM_MEM_WATCH_DIR         output dir          (default: $HOME/.wqm-mem-watch)
#   WQM_MEM_WATCH_INTERVAL    seconds between rows (default: 300)
#   WQM_MEM_WATCH_CONTAINERS  space-separated list (default: the wqm services)
#
# Run detached so it survives the launching shell:
#   setsid nohup bash scripts/mem-watch/sampler.sh >/dev/null 2>&1 &
# or `make mem-watch-start`. Analyze with scripts/mem-watch/analyze.py.
set -u

OUT_DIR="${WQM_MEM_WATCH_DIR:-$HOME/.wqm-mem-watch}"
INTERVAL="${WQM_MEM_WATCH_INTERVAL:-300}"
read -r -a CONTAINERS <<< "${WQM_MEM_WATCH_CONTAINERS:-wqm-memexd wqm-qdrant wqm-embeddings wqm-embeddings-gpu}"

CSV="$OUT_DIR/samples.csv"
LOCK="$OUT_DIR/sampler.pid"
mkdir -p "$OUT_DIR"

# Single-instance guard: bail if a live sampler already holds the lock.
if [ -f "$LOCK" ] && kill -0 "$(cat "$LOCK" 2>/dev/null)" 2>/dev/null; then
  echo "sampler already running (pid $(cat "$LOCK"))" >&2
  exit 0
fi
echo $$ > "$LOCK"
trap 'rm -f "$LOCK"' EXIT

# Header only on a fresh file, so restarts append to the same series.
if [ ! -s "$CSV" ]; then
  hdr="epoch,iso,host_avail_gib"
  for c in "${CONTAINERS[@]}"; do hdr+=",${c#wqm-}_cur_gib,${c#wqm-}_anon_gib"; done
  echo "$hdr" > "$CSV"
fi

bytes_to_gib() { awk "BEGIN{printf \"%.3f\", ${1:-0}/1073741824}"; }

# Parse a `docker stats` MemUsage token ("4.448GiB", "85.08MiB", ...) to GiB.
docker_stats_gib() {
  docker stats --no-stream --format '{{.MemUsage}}' "$1" 2>/dev/null \
    | awk '{v=$1; n=v+0; u=v; gsub(/[0-9.]/,"",u);
            if(u=="GiB")print n; else if(u=="MiB")print n/1024;
            else if(u=="KiB")print n/1048576; else if(u=="B")print n/1073741824;
            else print 0}'
}

# Echo "cur_gib anon_gib" for one container; blanks if it cannot be read.
sample_container() {
  local c="$1" pid rel base cur anon
  pid=$(docker inspect -f '{{.State.Pid}}' "$c" 2>/dev/null)
  if [ -n "${pid:-}" ] && [ "$pid" != "0" ] && [ -r "/proc/$pid/cgroup" ]; then
    rel=$(awk -F: '$1=="0"{print $3}' "/proc/$pid/cgroup" 2>/dev/null)
    base="/sys/fs/cgroup${rel}"
    cur=$(cat "$base/memory.current" 2>/dev/null)
    anon=$(awk '/^anon /{print $2}' "$base/memory.stat" 2>/dev/null)
  fi
  if [ -z "${cur:-}" ]; then            # cgroup unreachable -> docker stats fallback
    local g; g=$(docker_stats_gib "$c")
    echo "${g:-} "                       # anon unknown in this mode
    return
  fi
  echo "$(bytes_to_gib "$cur") $(bytes_to_gib "${anon:-0}")"
}

while true; do
  epoch=$(date +%s)
  iso=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  avail_kb=$(awk '/MemAvailable/{print $2}' /proc/meminfo)
  row="$epoch,$iso,$(awk "BEGIN{printf \"%.3f\", ${avail_kb:-0}/1048576}")"
  for c in "${CONTAINERS[@]}"; do
    read -r cur anon <<< "$(sample_container "$c")"
    row+=",${cur:-},${anon:-}"
  done
  echo "$row" >> "$CSV"
  sleep "$INTERVAL"
done
