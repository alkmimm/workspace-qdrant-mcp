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
# Before a breach COUNTS, the cheap remedy is tried once: when the guest's
# page cache is at least GUARD_DROP_MIN_CACHE_GB (default 8), drop it
# (`echo 3 > /proc/sys/vm/drop_caches` — clean pages only, nothing is lost)
# and re-measure; a reading back under the line is logged as averted, not
# counted. 2026-09-19: a `make redeploy` (image build + 11 GB backup copy)
# pushed the charge to 70.8 GB with 50.9 GB of it cache, the guard stopped
# the stack mid-deploy, and the next `compose up` brought back only the two
# services it was asked for — embeddings and the collector stayed down.
# Dropping the cache took the charge from 63.5 to 40.7 GB; nothing needed
# stopping. The drop needs root: passwordless sudo when available, else a
# privileged helper container on the same kernel (GUARD_DROP_CMD overrides).
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
GUARD_DROP_MIN_CACHE_GB="${GUARD_DROP_MIN_CACHE_GB:-8}"
GUARD_DROP_SETTLE_SECS="${GUARD_DROP_SETTLE_SECS:-3}"
LOG="${GUARD_LOG:-$REPO/.wqm-fork/logs/memory-guard.log}"
COMPOSE=(docker compose --env-file "$REPO/docker/.env" -f "$REPO/docker-compose.yml")
# GUARD_STOP_CMD replaces the compose stop (self-test only; see wsl-memory-lib.sh).
stop_stack() { if [[ -n "${GUARD_STOP_CMD:-}" ]]; then bash -c "$GUARD_STOP_CMD"; else "${COMPOSE[@]}" stop; fi; }
# GUARD_DROP_CMD replaces the cache drop (self-test, or a host with its own way in).
drop_caches() {
  if [[ -n "${GUARD_DROP_CMD:-}" ]]; then bash -c "$GUARD_DROP_CMD"; return; fi
  sync
  sudo -n sh -c 'echo 3 > /proc/sys/vm/drop_caches' 2>/dev/null && return 0
  docker run --rm --privileged alpine sh -c 'echo 3 > /proc/sys/vm/drop_caches' >/dev/null 2>&1
}

mkdir -p "$(dirname "$LOG")"
log() { printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" | tee -a "$LOG"; }

log "guard start: stop if host charge for the VM > ${VM_GUARD_STOP_GB} GB or host free < ${HOST_GUARD_MIN_FREE_GB} GB for ${CONSECUTIVE_BREACHES} consecutive polls (every ${INTERVAL_SECS}s); page cache ≥ ${GUARD_DROP_MIN_CACHE_GB} GB is dropped and re-measured before a breach counts"
breaches=0
host_unknown_logged=0
# One reading of every probe; sets guest_kb cached_kb charge_kb hf measured how reason.
measure() {
  guest_kb=$(wsl_guest_footprint_kb); cached_kb=$(wsl_guest_cached_kb)
  charge_kb=$(wsl_vm_charge_kb); hf=$(wsl_host_free_kb)
  # What the host is charged: the vmmem working set when we can see it, the
  # guest footprint (charge minus 3–7 GB of hypervisor overhead) otherwise.
  if [[ -n "$charge_kb" ]]; then measured="$charge_kb"; how="vmmemWSL"; else measured="$guest_kb"; how="guest MemTotal−MemFree"; fi
  reason=""
  if (( measured > VM_GUARD_STOP_GB * 1048576 )); then
    reason="host charge for the VM $(wsl_gb "$measured") GB (${how}; guest used $(wsl_gb "$guest_kb") GB, cache $(wsl_gb "$cached_kb") GB) > ${VM_GUARD_STOP_GB} GB"
  elif [[ -n "$hf" ]] && (( hf < HOST_GUARD_MIN_FREE_GB * 1048576 )); then
    reason="host free $(wsl_gb "$hf") GB < ${HOST_GUARD_MIN_FREE_GB} GB (VM charge $(wsl_gb "$measured") GB)"
  fi
}
while :; do
  measure
  if [[ -n "$hf" ]]; then
    host_unknown_logged=0
  elif (( host_unknown_logged == 0 )); then
    log "note: host memory reading unusable — host check off until it recovers; guest-side footprint check still active"
    host_unknown_logged=1
  fi
  # The cheap remedy first: page cache is reclaimable and the host is charged
  # for it all the same. Drop it, let the reading settle, measure again — and
  # only count what remains.
  if [[ -n "$reason" ]] && (( cached_kb >= GUARD_DROP_MIN_CACHE_GB * 1048576 )); then
    before_cache_kb="$cached_kb"; before_measured="$measured"; before_reason="$reason"
    if drop_caches; then
      sleep "$GUARD_DROP_SETTLE_SECS"
      measure
      if [[ -z "$reason" ]]; then
        log "averted: $before_reason — dropped $(wsl_gb "$before_cache_kb") GB of page cache, charge $(wsl_gb "$before_measured") → $(wsl_gb "$measured") GB"
      else
        log "cache drop did not clear it ($(wsl_gb "$before_cache_kb") → $(wsl_gb "$cached_kb") GB cached, charge $(wsl_gb "$before_measured") → $(wsl_gb "$measured") GB)"
      fi
    else
      log "cache drop unavailable (no passwordless sudo, no docker privileged helper) — counting the breach as is"
    fi
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
