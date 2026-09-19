#!/usr/bin/env bash
# Host-memory preflight for a WSL2 deployment — refuse to start (or grow) the
# stack when the Windows host is already short of memory.
#
# Why this exists (2026-09-16): the WSL2 VM sat at its 96 GB ceiling for hours
# with 50–61 GiB of PAGE CACHE (processes never exceeded 17 GiB). The host was
# left with ~33 GB for Windows + editors + the GPU's shared memory, failed to
# page its own files (InPageError), froze the VM and finally rebooted — taking
# every other workload in the VM down with it. Nothing in the stack was "using"
# that memory, which is why no per-container limit would have caught it: the
# host is charged for the VM's working set, cache included, until the guest
# goes idle long enough for autoMemoryReclaim to hand it back.
#
# What it checks (probes in scripts/wsl-memory-lib.sh):
#   1. Windows free physical memory            ≥ HOST_MIN_FREE_GB
#   2. what the host is charged for the VM     ≤ VM_MAX_FOOTPRINT_GB
#      (vmmemWSL working set; falls back to the guest's MemTotal − MemFree
#      when the host cannot be asked — then the guest page cache is also
#      capped at VM_MAX_CACHE_GB, because the guest cannot tell how much of
#      it the host has already reclaimed)
#   3. memory pressure inside the VM (PSI full avg60) ≤ VM_MAX_PSI_FULL_PCT
#      Swap-in-use is reported but never fails the check: autoMemoryReclaim
#      swaps idle anon pages out on purpose (8.8 GB after an idle night with
#      PSI at 0.06 %), so "swap used" is not evidence of pressure here.
#
# Exit 0 = safe to start; 1 = refuse (prints what to do); 2 = could not measure
# the host (treated as refuse unless PREFLIGHT_SOFT=1). PREFLIGHT_SOFT=1 turns
# EVERY refusal into a warning — the operator can see that a breach is build
# residue (a `make validate` leaves ~50 GB of reclaimable cargo cache in the
# guest) when the script cannot; the stack-guard watchdog stays the backstop.
# Every threshold is an env var; the defaults fit a 127 GB host with a 96 GB
# WSL cap.
#
# Usage:  scripts/host-memory-preflight.sh            # check and exit
#         PREFLIGHT_SOFT=1 scripts/host-memory-preflight.sh   # warn only
set -uo pipefail
# shellcheck source=scripts/wsl-memory-lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/wsl-memory-lib.sh"

HOST_MIN_FREE_GB="${HOST_MIN_FREE_GB:-24}"
VM_MAX_FOOTPRINT_GB="${VM_MAX_FOOTPRINT_GB:-60}"
VM_MAX_CACHE_GB="${VM_MAX_CACHE_GB:-40}"
VM_MAX_PSI_FULL_PCT="${VM_MAX_PSI_FULL_PCT:-10}"
PREFLIGHT_SOFT="${PREFLIGHT_SOFT:-0}"

say() { printf '  %-8s %s\n' "$1" "$2"; }

fail=0
echo "=== host-memory preflight ==="

# ── 1. Windows host free memory (interop; absent outside WSL) ──────────────
host_free_kb=$(wsl_host_free_kb); host_total_kb=$(wsl_host_total_kb)
if [[ -n "$host_free_kb" ]]; then
  if (( host_free_kb < HOST_MIN_FREE_GB * 1048576 )); then
    say "[FAIL]" "Windows host has $(wsl_gb "$host_free_kb") GB free of $(wsl_gb "${host_total_kb:-0}") — below HOST_MIN_FREE_GB=$HOST_MIN_FREE_GB. Starting the stack here is what took the host down on 2026-09-16."
    fail=1
  else
    say "[OK]" "Windows host free memory: $(wsl_gb "$host_free_kb") GB of $(wsl_gb "${host_total_kb:-0}") (min $HOST_MIN_FREE_GB)"
  fi
else
  say "[?]" "could not read Windows host memory (not WSL, interop disabled, or implausible reading) — guest-side checks only"
  fail=2
fi

# ── 2. What the host is charged for the VM ─────────────────────────────────
guest_footprint_kb=$(wsl_guest_footprint_kb); cached_kb=$(wsl_guest_cached_kb)
charge_kb=$(wsl_vm_charge_kb)
if [[ -n "$charge_kb" ]]; then
  if (( charge_kb > VM_MAX_FOOTPRINT_GB * 1048576 )); then
    say "[FAIL]" "host is charged $(wsl_gb "$charge_kb") GB for the VM (vmmemWSL working set; guest sees $(wsl_gb "$guest_footprint_kb") GB used, $(wsl_gb "$cached_kb") GB of it page cache) — above VM_MAX_FOOTPRINT_GB=$VM_MAX_FOOTPRINT_GB. Cache only returns when the guest CPU idles; stop the churn or drop it (root: echo 3 > /proc/sys/vm/drop_caches)."
    fail=1
  else
    say "[OK]" "host charge for the VM: $(wsl_gb "$charge_kb") GB (guest: $(wsl_gb "$guest_footprint_kb") GB used, $(wsl_gb "$cached_kb") GB page cache; max $VM_MAX_FOOTPRINT_GB)"
  fi
else
  # No host view: the guest numbers are all there is, and cache must be capped
  # separately because the guest cannot see what the host already reclaimed.
  if (( guest_footprint_kb > VM_MAX_FOOTPRINT_GB * 1048576 )); then
    say "[FAIL]" "VM footprint (MemTotal−MemFree) is $(wsl_gb "$guest_footprint_kb") GB — above VM_MAX_FOOTPRINT_GB=$VM_MAX_FOOTPRINT_GB. This is roughly what the host is charged (+3–7 GB)."
    fail=1
  else
    say "[OK]" "VM footprint: $(wsl_gb "$guest_footprint_kb") GB of $(wsl_gb "$(wsl_meminfo MemTotal)") (max $VM_MAX_FOOTPRINT_GB)"
  fi
  if (( cached_kb > VM_MAX_CACHE_GB * 1048576 )); then
    say "[FAIL]" "VM page cache is $(wsl_gb "$cached_kb") GB — above VM_MAX_CACHE_GB=$VM_MAX_CACHE_GB and the host cannot tell us whether it was reclaimed. Drop it (root: echo 3 > /proc/sys/vm/drop_caches) or wait for reclaim before starting."
    fail=1
  else
    say "[OK]" "VM page cache: $(wsl_gb "$cached_kb") GB (max $VM_MAX_CACHE_GB)"
  fi
fi

# ── 3. Pressure inside the VM ──────────────────────────────────────────────
psi=$(wsl_psi_full_avg60_pct); swap_used_kb=$(wsl_guest_swap_used_kb)
if [[ -n "$psi" ]]; then
  if (( psi > VM_MAX_PSI_FULL_PCT )); then
    say "[FAIL]" "VM memory pressure: PSI full avg60 = ${psi}% — above VM_MAX_PSI_FULL_PCT=$VM_MAX_PSI_FULL_PCT; every task is already stalling on memory (swap in use: $(wsl_gb "$swap_used_kb") GB)."
    fail=1
  else
    say "[OK]" "VM memory pressure: PSI full avg60 = ${psi}% (max $VM_MAX_PSI_FULL_PCT); swap in use $(wsl_gb "$swap_used_kb") GB (informational — autoMemoryReclaim swaps idle pages on purpose)"
  fi
else
  say "[?]" "no PSI in this kernel — pressure not measured; swap in use $(wsl_gb "$swap_used_kb") GB"
fi

echo ""
case "$fail" in
  0) echo "preflight: safe to start." ;;
  2) if [[ "$PREFLIGHT_SOFT" == "1" ]]; then echo "preflight: host unmeasurable, continuing (PREFLIGHT_SOFT=1)."; fail=0
     else echo "preflight: could not measure the host — refusing (set PREFLIGHT_SOFT=1 to override)."; fi ;;
  *) if [[ "$PREFLIGHT_SOFT" == "1" ]]; then
       echo "preflight: thresholds exceeded, continuing anyway (PREFLIGHT_SOFT=1) — the stack-guard watchdog is the remaining protection; run 'make stack-guard' if it is not up."
       fail=0
     else
       echo "preflight: UNSAFE — not starting. Free host memory first (close what you can, or lower memory= in ~/.wslconfig and wsl --shutdown when the other workloads allow it). PREFLIGHT_SOFT=1 overrides."
     fi ;;
esac
exit "$fail"
