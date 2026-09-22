#!/usr/bin/env bash
# Self-test for the WSL2 memory preflight + watchdog, hermetic (no Windows
# host, no docker): every probe in scripts/wsl-memory-lib.sh is replaced
# through its WSL_MEMORY_PROBES_OVERRIDE seam, and the guard's compose stop
# through GUARD_STOP_CMD. Runs in the static-checks stage — the scripts gate
# `make stack-up` / `make redeploy`, so a regression here silently removes the
# protection that keeps this stack from taking the host down (2026-09-16).
#
# Usage: bash scripts/ci/wsl-memory-guards-selftest.sh [repo-root]
set -uo pipefail
REPO="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
PREFLIGHT="$REPO/scripts/host-memory-preflight.sh"
GUARD="$REPO/scripts/wsl-memory-guard.sh"
LIB="$REPO/scripts/wsl-memory-lib.sh"
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
fails=0; ran=0

for f in "$LIB" "$PREFLIGHT" "$GUARD"; do
  bash -n "$f" || { echo "FAIL: syntax error in $f"; exit 1; }
done

# Probe stubs. Scalars come from FAKE_* env vars ("" = unmeasurable, exactly
# what the real probes return when they cannot measure). The VM charge can be
# a space-separated SEQUENCE consumed one value per call, so a guard run can
# be fed "80 80" (trip) or "80 20 20" (recover) across its polls.
cat > "$TMP/probes.sh" <<'STUB'
wsl_host_free_kb()        { echo "${FAKE_HOST_FREE_KB-}"; }
wsl_host_total_kb()       { echo "${FAKE_HOST_TOTAL_KB-133000000}"; }
wsl_guest_footprint_kb()  { echo "${FAKE_GUEST_KB-20971520}"; }
wsl_guest_cached_kb()     { echo "${FAKE_CACHED_KB-5242880}"; }
wsl_guest_swap_used_kb()  { echo "${FAKE_SWAP_KB-0}"; }
wsl_psi_full_avg60_pct()  { echo "${FAKE_PSI-0}"; }
wsl_vm_charge_kb() {
  local seq=( ${FAKE_CHARGE_SEQ-} ) n
  [[ ${#seq[@]} -gt 0 ]] || { echo ""; return; }
  n=$(cat "$FAKE_COUNTER" 2>/dev/null || echo 0); echo $(( n + 1 )) > "$FAKE_COUNTER"
  (( n >= ${#seq[@]} )) && n=$(( ${#seq[@]} - 1 ))
  echo "${seq[$n]}"
}
STUB
export WSL_MEMORY_PROBES_OVERRIDE="$TMP/probes.sh"
gb() { echo $(( $1 * 1048576 )); }

# expect_preflight <name> <expected-exit> [VAR=value ...]
expect_preflight() {
  local name="$1" want="$2"; shift 2
  local out got
  out=$(env "$@" FAKE_COUNTER="$TMP/c.$RANDOM" bash "$PREFLIGHT" 2>&1); got=$?
  ran=$(( ran + 1 ))
  if [[ "$got" != "$want" ]]; then
    echo "FAIL: preflight/$name: exit $got, wanted $want"; echo "$out" | sed 's/^/    /'; fails=$(( fails + 1 ))
  else
    echo "ok: preflight/$name (exit $got)"
  fi
}

# ── preflight ────────────────────────────────────────────────────────────────
expect_preflight healthy                    0 FAKE_HOST_FREE_KB=$(gb 100) FAKE_CHARGE_SEQ=$(gb 20)
expect_preflight host-short                 1 FAKE_HOST_FREE_KB=$(gb 10)  FAKE_CHARGE_SEQ=$(gb 20)
expect_preflight charge-high                1 FAKE_HOST_FREE_KB=$(gb 100) FAKE_CHARGE_SEQ=$(gb 65)
expect_preflight charge-high-soft           0 FAKE_HOST_FREE_KB=$(gb 100) FAKE_CHARGE_SEQ=$(gb 65) PREFLIGHT_SOFT=1
# Cache is only capped when the host cannot say what it is charged: 50 GB of
# guest cache with a 30 GB host charge is a reclaimed cache, not a risk.
expect_preflight big-cache-charge-fine      0 FAKE_HOST_FREE_KB=$(gb 100) FAKE_CHARGE_SEQ=$(gb 30) FAKE_CACHED_KB=$(gb 50) FAKE_GUEST_KB=$(gb 66)
# Swap in use is informational (autoMemoryReclaim swaps idle pages on purpose).
expect_preflight swap-only-is-fine          0 FAKE_HOST_FREE_KB=$(gb 100) FAKE_CHARGE_SEQ=$(gb 20) FAKE_SWAP_KB=$(gb 20)
expect_preflight psi-pressure               1 FAKE_HOST_FREE_KB=$(gb 100) FAKE_CHARGE_SEQ=$(gb 20) FAKE_PSI=15
expect_preflight psi-unmeasurable           0 FAKE_HOST_FREE_KB=$(gb 100) FAKE_CHARGE_SEQ=$(gb 20) FAKE_PSI=
# No host view: exit 2 (unmeasurable), and the guest-side caps take over.
expect_preflight host-unmeasurable          2 FAKE_HOST_FREE_KB= FAKE_CHARGE_SEQ=
expect_preflight host-unmeasurable-soft     0 FAKE_HOST_FREE_KB= FAKE_CHARGE_SEQ= PREFLIGHT_SOFT=1
expect_preflight no-host-guest-cache-high   1 FAKE_HOST_FREE_KB= FAKE_CHARGE_SEQ= FAKE_CACHED_KB=$(gb 50) FAKE_GUEST_KB=$(gb 55)
expect_preflight no-host-guest-footprint    1 FAKE_HOST_FREE_KB= FAKE_CHARGE_SEQ= FAKE_GUEST_KB=$(gb 66)
# Host measurable but no vmmem process (older WSL naming): guest caps apply.
expect_preflight host-ok-no-vmmem-guest-ok  0 FAKE_HOST_FREE_KB=$(gb 100) FAKE_CHARGE_SEQ=
expect_preflight host-ok-no-vmmem-guest-big 1 FAKE_HOST_FREE_KB=$(gb 100) FAKE_CHARGE_SEQ= FAKE_GUEST_KB=$(gb 66)

# ── guard ────────────────────────────────────────────────────────────────────
# run_guard <name> <expected-exit> <expect-stop:yes|no> [VAR=value ...]
# The cache drop is a no-op here unless a case sets GUARD_DROP_CMD itself;
# the default cache reading (5 GB) is under GUARD_DROP_MIN_CACHE_GB anyway.
# A case may pass a 4th token as LOG=<needle>: the guard log must contain it.
run_guard() {
  local name="$1" want="$2" stop="$3"; shift 3
  local needle=""; [[ "${1:-}" == LOG=* ]] && { needle="${1#LOG=}"; shift; }
  local marker="$TMP/stopped.$RANDOM" log="$TMP/guard.$RANDOM.log" got
  # GUARD_LOCK= disables the single-instance lock: these cases run guards in
  # parallel against the real repo path on purpose, and the live guard on a
  # developer's machine already holds that lock. Cases that TEST the lock pass
  # their own GUARD_LOCK after "$@", which wins.
  env GUARD_DROP_CMD=true GUARD_DROP_SETTLE_SECS=0 GUARD_LOCK= "$@" FAKE_COUNTER="$TMP/g.$RANDOM" GUARD_LOG="$log" GUARD_STOP_CMD="touch '$marker'" INTERVAL_SECS=0 \
    timeout 2 bash "$GUARD" >/dev/null 2>&1; got=$?
  ran=$(( ran + 1 ))
  local stopped=no; [[ -e "$marker" ]] && stopped=yes
  if [[ "$got" != "$want" || "$stopped" != "$stop" ]] || { [[ -n "$needle" ]] && ! grep -q -- "$needle" "$log"; }; then
    echo "FAIL: guard/$name: exit $got (wanted $want), stopped=$stopped (wanted $stop)${needle:+, log must contain '$needle'}"; sed 's/^/    /' "$log"; fails=$(( fails + 1 ))
  else
    echo "ok: guard/$name (exit $got, stopped=$stopped)"
  fi
}
# exit 3 = tripped and stopped; exit 124 = timeout ran out with no trip.
run_guard trips-after-two-breaches   3   yes FAKE_HOST_FREE_KB=$(gb 100) FAKE_CHARGE_SEQ="$(gb 80) $(gb 80)"
run_guard one-breach-then-recovery   124 no  FAKE_HOST_FREE_KB=$(gb 100) FAKE_CHARGE_SEQ="$(gb 80) $(gb 20)"
run_guard host-short-trips           3   yes FAKE_HOST_FREE_KB=$(gb 5)   FAKE_CHARGE_SEQ=$(gb 20)
run_guard healthy-never-trips        124 no  FAKE_HOST_FREE_KB=$(gb 100) FAKE_CHARGE_SEQ=$(gb 20)
# No host view: the guest footprint stands in for the charge.
run_guard no-host-guest-footprint    3   yes FAKE_HOST_FREE_KB= FAKE_CHARGE_SEQ= FAKE_GUEST_KB=$(gb 75)
run_guard no-host-guest-fine         124 no  FAKE_HOST_FREE_KB= FAKE_CHARGE_SEQ= FAKE_GUEST_KB=$(gb 20)
# Page cache is the charge (the 2026-09-19 redeploy): the drop runs, the
# re-measure (second value in the sequence) is back under the line, and the
# breach is averted — no stop, ever. The 3rd reading onward stays at 20.
run_guard cache-drop-averts-breach   124 no  LOG=averted FAKE_HOST_FREE_KB=$(gb 100) FAKE_CACHED_KB=$(gb 50) \
  GUARD_DROP_CMD="touch '$TMP/dropped'" FAKE_CHARGE_SEQ="$(gb 80) $(gb 40) $(gb 20)"
[[ -e "$TMP/dropped" ]] || { echo "FAIL: guard/cache-drop-averts-breach: the drop command never ran"; fails=$(( fails + 1 )); }
# Real pressure under a big cache: the drop runs, the re-measure is still
# over the line, the breach counts, and two of them trip as before.
run_guard cache-drop-not-enough-trips 3  yes LOG="did not clear" FAKE_HOST_FREE_KB=$(gb 100) FAKE_CACHED_KB=$(gb 50) \
  GUARD_DROP_CMD=true FAKE_CHARGE_SEQ="$(gb 80) $(gb 78) $(gb 80) $(gb 78)"
# No way to drop (sudo and docker both refused): the breach counts as is.
run_guard cache-drop-unavailable-trips 3 yes LOG="drop unavailable" FAKE_HOST_FREE_KB=$(gb 100) FAKE_CACHED_KB=$(gb 50) \
  GUARD_DROP_CMD=false FAKE_CHARGE_SEQ="$(gb 80) $(gb 80)"
# Single-instance lock: the boot unit and `make stack-guard` both start the
# script, and two guards would each answer the same breach with its own
# `compose stop`. With a free lock the guard runs normally; with the lock held
# by another process it exits 0 (NOT a failure — systemd's Restart=on-failure
# must not respawn it into a contention loop) and stops nothing, even while a
# breach is on the table.
run_guard lock-free-guard-runs          124 no  GUARD_LOCK="$TMP/free.lock" FAKE_HOST_FREE_KB=$(gb 100) FAKE_CHARGE_SEQ=$(gb 20)
exec 8>"$TMP/held.lock"; flock -n 8 || { echo "FAIL: harness could not take the test lock"; fails=$(( fails + 1 )); }
run_guard lock-held-second-exits-zero     0 no  LOG="another guard already holds" GUARD_LOCK="$TMP/held.lock" \
  FAKE_HOST_FREE_KB=$(gb 5) FAKE_CHARGE_SEQ="$(gb 80) $(gb 80)"
exec 8>&-

echo "wsl-memory-guards selftest: $ran cases, $fails failed"
(( ran == 25 )) || { echo "GATE: expected exactly 25 cases to run (got $ran) — a deleted case is a silently un-gated behaviour; adjust the count when you add one."; exit 1; }
exit $(( fails > 0 ))
