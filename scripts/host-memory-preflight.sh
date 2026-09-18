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
# VM's footprint as the host sees it is MemTotal − MemFree, cache included.
#
# What it checks, in order of what actually protects the host:
#   1. Windows free physical memory (via powershell.exe interop) ≥ HOST_MIN_FREE_GB
#   2. VM footprint (MemTotal − MemFree)                        ≤ VM_MAX_FOOTPRINT_GB
#   3. Page cache inside the VM                                 ≤ VM_MAX_CACHE_GB
#   4. Swap in use inside the VM                                ≤ VM_MAX_SWAP_USED_GB
#
# Exit 0 = safe to start; 1 = refuse (prints what to do); 2 = could not measure
# (treated as refuse unless PREFLIGHT_SOFT=1). Every threshold is an env var so
# a bigger/smaller machine can tune it; the defaults fit a 127 GB host with a
# 96 GB WSL cap.
#
# Usage:  scripts/host-memory-preflight.sh            # check and exit
#         PREFLIGHT_SOFT=1 scripts/host-memory-preflight.sh   # warn only
set -uo pipefail

HOST_MIN_FREE_GB="${HOST_MIN_FREE_GB:-24}"
VM_MAX_FOOTPRINT_GB="${VM_MAX_FOOTPRINT_GB:-60}"
VM_MAX_CACHE_GB="${VM_MAX_CACHE_GB:-40}"
VM_MAX_SWAP_USED_GB="${VM_MAX_SWAP_USED_GB:-8}"
PREFLIGHT_SOFT="${PREFLIGHT_SOFT:-0}"

say() { printf '  %-8s %s\n' "$1" "$2"; }
gb()  { awk -v kb="$1" 'BEGIN { printf "%.1f", kb / 1048576 }'; }

fail=0
echo "=== host-memory preflight ==="

# ── 1. Windows host free memory (interop; absent outside WSL) ──────────────
PS=/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe
host_free_kb=""
if [[ -x "$PS" ]]; then
  host_free_kb=$("$PS" -NoProfile -NonInteractive -Command \
    '(Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory' 2>/dev/null | tr -d '\r[:space:]')
  host_total_kb=$("$PS" -NoProfile -NonInteractive -Command \
    '(Get-CimInstance Win32_OperatingSystem).TotalVisibleMemorySize' 2>/dev/null | tr -d '\r[:space:]')
fi
if [[ "$host_free_kb" =~ ^[0-9]+$ ]]; then
  if (( host_free_kb < HOST_MIN_FREE_GB * 1048576 )); then
    say "[FAIL]" "Windows host has $(gb "$host_free_kb") GB free of $(gb "${host_total_kb:-0}") — below HOST_MIN_FREE_GB=$HOST_MIN_FREE_GB. Starting the stack here is what took the host down on 2026-09-16."
    fail=1
  else
    say "[OK]" "Windows host free memory: $(gb "$host_free_kb") GB of $(gb "${host_total_kb:-0}") (min $HOST_MIN_FREE_GB)"
  fi
else
  say "[?]" "could not read Windows host memory (not WSL, or interop disabled) — VM checks only"
  [[ "$PREFLIGHT_SOFT" == "1" ]] || fail=2
fi

# ── 2–4. Inside the VM ──────────────────────────────────────────────────────
read_meminfo() { awk -v k="$1" '$1 == k":" { print $2 }' /proc/meminfo; }
total_kb=$(read_meminfo MemTotal); free_kb=$(read_meminfo MemFree)
cached_kb=$(read_meminfo Cached); swap_total_kb=$(read_meminfo SwapTotal); swap_free_kb=$(read_meminfo SwapFree)
footprint_kb=$(( total_kb - free_kb ))
swap_used_kb=$(( swap_total_kb - swap_free_kb ))

if (( footprint_kb > VM_MAX_FOOTPRINT_GB * 1048576 )); then
  say "[FAIL]" "VM footprint (MemTotal−MemFree) is $(gb "$footprint_kb") GB — above VM_MAX_FOOTPRINT_GB=$VM_MAX_FOOTPRINT_GB. This is what the host is charged for."
  fail=1
else
  say "[OK]" "VM footprint: $(gb "$footprint_kb") GB of $(gb "$total_kb") (max $VM_MAX_FOOTPRINT_GB)"
fi
if (( cached_kb > VM_MAX_CACHE_GB * 1048576 )); then
  say "[FAIL]" "VM page cache is $(gb "$cached_kb") GB — above VM_MAX_CACHE_GB=$VM_MAX_CACHE_GB; autoMemoryReclaim is not returning it. Drop it (root: echo 3 > /proc/sys/vm/drop_caches) or wait for reclaim before starting."
  fail=1
else
  say "[OK]" "VM page cache: $(gb "$cached_kb") GB (max $VM_MAX_CACHE_GB)"
fi
if (( swap_used_kb > VM_MAX_SWAP_USED_GB * 1048576 )); then
  say "[FAIL]" "VM swap in use: $(gb "$swap_used_kb") GB — above VM_MAX_SWAP_USED_GB=$VM_MAX_SWAP_USED_GB; the VM is already under pressure."
  fail=1
else
  say "[OK]" "VM swap in use: $(gb "$swap_used_kb") GB (max $VM_MAX_SWAP_USED_GB)"
fi

echo ""
case "$fail" in
  0) echo "preflight: safe to start." ;;
  2) if [[ "$PREFLIGHT_SOFT" == "1" ]]; then echo "preflight: host unmeasurable, continuing (PREFLIGHT_SOFT=1)."; fail=0
     else echo "preflight: could not measure the host — refusing (set PREFLIGHT_SOFT=1 to override)."; fi ;;
  *) if [[ "$PREFLIGHT_SOFT" == "1" ]]; then
       # Downgrade EVERY threshold breach, not just the unmeasurable host: the
       # VM-side numbers can be build residue (a `make validate` leaves ~50 GB
       # of reclaimable cargo cache in the guest) while the host is fine, and
       # the operator is the one who can tell. The watchdog stays the backstop.
       echo "preflight: thresholds exceeded, continuing anyway (PREFLIGHT_SOFT=1) — the stack-guard watchdog is the remaining protection; run 'make stack-guard' if it is not up."
       fail=0
     else
       echo "preflight: UNSAFE — not starting. Free host memory first (close what you can, or lower memory= in ~/.wslconfig and wsl --shutdown when the other workloads allow it). PREFLIGHT_SOFT=1 overrides."
     fi ;;
esac
exit "$fail"
