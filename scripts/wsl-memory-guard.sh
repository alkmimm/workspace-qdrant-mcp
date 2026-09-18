#!/usr/bin/env bash
# Memory watchdog for a WSL2 deployment — stop THIS compose project (and only
# it) before the VM's footprint can take the Windows host down.
#
# Companion to host-memory-preflight.sh: the preflight refuses to START on a
# short host; this guards the hours AFTER a start, when indexing I/O grows the
# page cache and `autoMemoryReclaim` may not hand it back (2026-09-16: the VM
# reached its 96 GB cap with 61 GiB of cache, the host rebooted, and every
# other workload in the VM went down with ours). Stopping our stack is cheap
# and reversible (`make stack-up`); a host reboot is neither.
#
# Trip conditions (any one), polled every INTERVAL_SECS:
#   VM footprint (MemTotal − MemFree) > VM_GUARD_STOP_GB        (default 70)
#   Windows host free memory        < HOST_GUARD_MIN_FREE_GB   (default 16)
# On trip: `docker compose stop` for this project (manual stop wins over
# `restart: always`, so nothing comes back on its own), one line in the log,
# then exit 3. Runs in the foreground; `make stack-guard` backgrounds it.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INTERVAL_SECS="${INTERVAL_SECS:-20}"
VM_GUARD_STOP_GB="${VM_GUARD_STOP_GB:-70}"
HOST_GUARD_MIN_FREE_GB="${HOST_GUARD_MIN_FREE_GB:-16}"
LOG="${GUARD_LOG:-$REPO/.wqm-fork/logs/memory-guard.log}"
COMPOSE=(docker compose --env-file "$REPO/docker/.env" -f "$REPO/docker-compose.yml")
PS=/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe

mkdir -p "$(dirname "$LOG")"
log() { printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" | tee -a "$LOG"; }
gb()  { awk -v kb="$1" 'BEGIN { printf "%.1f", kb / 1048576 }'; }
read_meminfo() { awk -v k="$1" '$1 == k":" { print $2 }' /proc/meminfo; }
host_free_kb() {
  [[ -x "$PS" ]] || { echo ""; return; }
  "$PS" -NoProfile -NonInteractive -Command '(Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory' 2>/dev/null | tr -d '\r[:space:]'
}

# A host reading below this is not a measurement, it is a broken interop call:
# FreePhysicalMemory cannot be ~0 on a host that is still answering CIM. On
# 2026-09-18 a detached guard read "0" once — while the host had 65 GB free —
# and stopped a healthy stack. Implausible readings count as UNKNOWN (the VM
# checks still apply), and every trip needs CONSECUTIVE_BREACHES polls in a row.
HOST_MIN_PLAUSIBLE_KB=$(( 1 * 1048576 ))
CONSECUTIVE_BREACHES="${CONSECUTIVE_BREACHES:-2}"

log "guard start: stop if VM footprint > ${VM_GUARD_STOP_GB} GB or host free < ${HOST_GUARD_MIN_FREE_GB} GB for ${CONSECUTIVE_BREACHES} consecutive polls (every ${INTERVAL_SECS}s)"
breaches=0
host_unknown_logged=0
while :; do
  total_kb=$(read_meminfo MemTotal); free_kb=$(read_meminfo MemFree)
  footprint_kb=$(( total_kb - free_kb ))
  hf_raw=$(host_free_kb)
  hf=""
  if [[ "$hf_raw" =~ ^[0-9]+$ ]] && (( hf_raw >= HOST_MIN_PLAUSIBLE_KB )); then
    hf="$hf_raw"
  elif (( host_unknown_logged == 0 )); then
    log "note: host memory reading unusable (raw=[${hf_raw:0:40}]) — host check off until it recovers; VM checks still active"
    host_unknown_logged=1
  fi
  reason=""
  if (( footprint_kb > VM_GUARD_STOP_GB * 1048576 )); then
    reason="VM footprint $(gb "$footprint_kb") GB > ${VM_GUARD_STOP_GB} GB"
  elif [[ -n "$hf" ]] && (( hf < HOST_GUARD_MIN_FREE_GB * 1048576 )); then
    reason="host free $(gb "$hf") GB < ${HOST_GUARD_MIN_FREE_GB} GB (raw=${hf_raw})"
  fi
  if [[ -n "$reason" ]]; then
    breaches=$(( breaches + 1 ))
    log "breach ${breaches}/${CONSECUTIVE_BREACHES}: $reason"
    if (( breaches >= CONSECUTIVE_BREACHES )); then
      log "TRIP: $reason — stopping the workspace-qdrant stack (other compose projects untouched)"
      "${COMPOSE[@]}" stop >>"$LOG" 2>&1
      log "stopped. Restart with: make stack-up (after fixing memory — see docs/runbooks/experiment-freeze.md §7b)"
      exit 3
    fi
  else
    breaches=0
  fi
  sleep "$INTERVAL_SECS"
done
