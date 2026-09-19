#!/usr/bin/env bash
# Memory watchdog for a WSL2 deployment — stop THIS compose project (and only
# it) before the VM's footprint can take the Windows host down.
#
# Companion to host-memory-preflight.sh: the preflight refuses to START on a
# short host; this guards the hours AFTER a start, when indexing I/O grows the
# page cache and `autoMemoryReclaim` cannot hand it back (it only runs when
# the guest CPU is idle, which a busy indexer never is). 2026-09-16: the VM
# reached its 96 GB cap with 61 GiB of cache, the host rebooted, and every
# other workload in the VM went down with ours. Stopping our stack is cheap
# and reversible (`make stack-up`); a host reboot is neither.
#
# Trip conditions (any one), polled every INTERVAL_SECS, and only after
# CONSECUTIVE_BREACHES polls in a row (one bogus interop reading must not
# stop a healthy stack — it did, on 2026-09-18):
#   host charge for the VM  > VM_GUARD_STOP_GB        (default 70)
#       vmmemWSL working set when the host can be asked, else the guest's
#       MemTotal − MemFree (≈ charge − 3–7 GB) — see scripts/wsl-memory-lib.sh
#   Windows host free memory < HOST_GUARD_MIN_FREE_GB   (default 16)
# On trip: `docker compose stop` for this project (manual stop wins over
# `restart: always`, so nothing comes back on its own), one line in the log,
# then exit 3. Runs in the foreground; `make stack-guard` backgrounds it.
set -uo pipefail
# shellcheck source=scripts/wsl-memory-lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/wsl-memory-lib.sh"

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INTERVAL_SECS="${INTERVAL_SECS:-20}"
VM_GUARD_STOP_GB="${VM_GUARD_STOP_GB:-70}"
HOST_GUARD_MIN_FREE_GB="${HOST_GUARD_MIN_FREE_GB:-16}"
CONSECUTIVE_BREACHES="${CONSECUTIVE_BREACHES:-2}"
LOG="${GUARD_LOG:-$REPO/.wqm-fork/logs/memory-guard.log}"
COMPOSE=(docker compose --env-file "$REPO/docker/.env" -f "$REPO/docker-compose.yml")
# GUARD_STOP_CMD replaces the compose stop (self-test only; see wsl-memory-lib.sh).
stop_stack() { if [[ -n "${GUARD_STOP_CMD:-}" ]]; then bash -c "$GUARD_STOP_CMD"; else "${COMPOSE[@]}" stop; fi; }

mkdir -p "$(dirname "$LOG")"
log() { printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" | tee -a "$LOG"; }

log "guard start: stop if host charge for the VM > ${VM_GUARD_STOP_GB} GB or host free < ${HOST_GUARD_MIN_FREE_GB} GB for ${CONSECUTIVE_BREACHES} consecutive polls (every ${INTERVAL_SECS}s)"
breaches=0
host_unknown_logged=0
while :; do
  guest_kb=$(wsl_guest_footprint_kb); cached_kb=$(wsl_guest_cached_kb)
  charge_kb=$(wsl_vm_charge_kb); hf=$(wsl_host_free_kb)
  if [[ -n "$hf" ]]; then
    host_unknown_logged=0
  elif (( host_unknown_logged == 0 )); then
    log "note: host memory reading unusable — host check off until it recovers; guest-side footprint check still active"
    host_unknown_logged=1
  fi
  # What the host is charged: the vmmem working set when we can see it, the
  # guest footprint (charge minus 3–7 GB of hypervisor overhead) otherwise.
  if [[ -n "$charge_kb" ]]; then measured="$charge_kb"; how="vmmemWSL"; else measured="$guest_kb"; how="guest MemTotal−MemFree"; fi
  reason=""
  if (( measured > VM_GUARD_STOP_GB * 1048576 )); then
    reason="host charge for the VM $(wsl_gb "$measured") GB (${how}; guest used $(wsl_gb "$guest_kb") GB, cache $(wsl_gb "$cached_kb") GB) > ${VM_GUARD_STOP_GB} GB"
  elif [[ -n "$hf" ]] && (( hf < HOST_GUARD_MIN_FREE_GB * 1048576 )); then
    reason="host free $(wsl_gb "$hf") GB < ${HOST_GUARD_MIN_FREE_GB} GB (VM charge $(wsl_gb "$measured") GB)"
  fi
  if [[ -n "$reason" ]]; then
    breaches=$(( breaches + 1 ))
    log "breach ${breaches}/${CONSECUTIVE_BREACHES}: $reason"
    top=$(wsl_host_top_processes); [[ -n "$top" ]] && log "  host top working sets (GB): $top"
    if (( breaches >= CONSECUTIVE_BREACHES )); then
      log "TRIP: $reason — stopping the workspace-qdrant stack (other compose projects untouched)"
      stop_stack >>"$LOG" 2>&1
      log "stopped. Restart with: make stack-up (after fixing memory — see docs/runbooks/experiment-freeze.md §7b)"
      exit 3
    fi
  else
    breaches=0
  fi
  sleep "$INTERVAL_SECS"
done
