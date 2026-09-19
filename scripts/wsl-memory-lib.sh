#!/usr/bin/env bash
# Shared memory probes for the WSL2 preflight and watchdog. Sourced, not run.
#
# Three numbers matter on a WSL2 host, and they are NOT the same thing:
#
#   host_free_kb    Windows free physical memory. The one that decides whether
#                   the host survives; below ~16 GB Windows starts failing its
#                   own page-ins (2026-09-16 host reboot).
#   vm_charge_kb    What the host is charged for the VM RIGHT NOW: the working
#                   set of the vmmem/vmmemWSL process. Guest page cache counts
#                   until autoMemoryReclaim hands it back — which only happens
#                   when the guest CPU is idle, so a busy indexer (or a cargo
#                   build) pins the cache and the charge grows to the cap.
#   guest_footprint_kb
#                   MemTotal − MemFree as the guest sees it. A proxy for the
#                   charge when the host cannot be asked (interop off, not WSL):
#                   measured 2026-09-19 at 14.3 GB guest vs 16.8 GB vmmemWSL,
#                   i.e. charge ≈ footprint + 3–7 GB of hypervisor overhead
#                   (samples 14.3→16.8, 17.3→23.9, 16.8→20.4).
#
# Swap-in-use is deliberately NOT a pressure signal here: autoMemoryReclaim
# swaps idle anon pages out on purpose (8.8 GB after an idle night, with PSI
# at 0.06 %). Pressure is read from the kernel's own PSI counters instead.
# Every reader returns "" when the number cannot be measured; callers treat
# "" as unknown, never as zero.

WSL_PS=/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe

# A host reading below this is not a measurement, it is a broken interop call:
# FreePhysicalMemory cannot be ~0 on a host that is still answering CIM. On
# 2026-09-18 a detached guard read "0" once — while the host had 65 GB free —
# and stopped a healthy stack.
WSL_HOST_MIN_PLAUSIBLE_KB=$(( 1 * 1048576 ))

wsl_ps() { [[ -x "$WSL_PS" ]] || return 1; "$WSL_PS" -NoProfile -NonInteractive -Command "$1" 2>/dev/null | tr -d '\r[:space:]'; }
wsl_gb() { awk -v kb="$1" 'BEGIN { printf "%.1f", kb / 1048576 }'; }
wsl_meminfo() { awk -v k="$1" '$1 == k":" { print $2 }' /proc/meminfo; }

# Windows free physical memory in KB, or "" when unmeasurable/implausible.
wsl_host_free_kb() {
  local raw; raw=$(wsl_ps '(Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory') || { echo ""; return; }
  if [[ "$raw" =~ ^[0-9]+$ ]] && (( raw >= WSL_HOST_MIN_PLAUSIBLE_KB )); then echo "$raw"; else echo ""; fi
}
wsl_host_total_kb() {
  local raw; raw=$(wsl_ps '(Get-CimInstance Win32_OperatingSystem).TotalVisibleMemorySize') || { echo ""; return; }
  [[ "$raw" =~ ^[0-9]+$ ]] && echo "$raw" || echo ""
}

# Host-side charge for the VM in KB (vmmem + vmmemWSL working sets), or "".
wsl_vm_charge_kb() {
  local raw; raw=$(wsl_ps '[int64]((Get-Process vmmem,vmmemWSL -ErrorAction SilentlyContinue | Measure-Object WorkingSet64 -Sum).Sum / 1024)') || { echo ""; return; }
  if [[ "$raw" =~ ^[0-9]+$ ]] && (( raw > 0 )); then echo "$raw"; else echo ""; fi
}

wsl_guest_footprint_kb() { echo $(( $(wsl_meminfo MemTotal) - $(wsl_meminfo MemFree) )); }
wsl_guest_cached_kb()    { wsl_meminfo Cached; }
wsl_guest_swap_used_kb() { echo $(( $(wsl_meminfo SwapTotal) - $(wsl_meminfo SwapFree) )); }

# PSI memory pressure, "full" share of the last 60 s as an integer percent
# (rounded down), or "" when the kernel has no PSI. "full" = every runnable
# task stalled on memory at once; a healthy VM sits at 0, a thrashing one at
# tens of percent.
wsl_psi_full_avg60_pct() {
  [[ -r /proc/pressure/memory ]] || { echo ""; return; }
  awk '$1 == "full" { for (i = 2; i <= NF; i++) if ($i ~ /^avg60=/) { sub(/^avg60=/, "", $i); printf "%d", $i; exit } }' /proc/pressure/memory
}

# Test seam: a file that redefines the probes above with fixed values, so the
# preflight and the guard can be exercised without a Windows host
# (scripts/ci/wsl-memory-guards-selftest.sh, run by the static-checks stage).
if [[ -n "${WSL_MEMORY_PROBES_OVERRIDE:-}" ]]; then
  # shellcheck disable=SC1090
  source "$WSL_MEMORY_PROBES_OVERRIDE"
fi
