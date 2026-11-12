#!/usr/bin/env bash
# Selftest for scripts/trunk-autopull.sh: builds throwaway origin/clone pairs
# and asserts the script fast-forwards ONLY the safe cases and leaves every
# other checkout exactly where it was — HEAD, tracked edits, untracked files,
# git's own state files. The script touches the user's working copies, so each
# rule it relies on (its own checks AND git's refusals) is pinned here. Runs in
# the static-checks stage (docker/Dockerfile.memexd) and by hand:
#   bash scripts/ci/trunk-autopull-selftest.sh [<repo root>]
set -u

ROOT="${1:-$(cd "$(dirname "$0")/../.." && pwd)}"
SCRIPT="$ROOT/scripts/trunk-autopull.sh"
EXPECTED_CASES=58
# A wrong root fails here, before anything runs, and nothing below writes to
# $SCRIPT: on 2026-11-12 a run with the wrong root reached the lock wait, whose
# `flock <path>` created an empty trunk-autopull.sh in the main checkout (the
# next fast-forward was then refused by git over that untracked file).
[[ -f "$SCRIPT" ]] || { echo "trunk-autopull selftest: no script at $SCRIPT" >&2; exit 1; }

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
# Hermetic git: no user/system config, a fixed identity for the commits.
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
# The free-space guard has its own case; elsewhere the build host's disk must
# not decide the outcome.
export WQM_AUTOPULL_MIN_FREE_GB=0
# The big-jump gate runs the real scripts/host-memory-preflight.sh with every
# probe fixed through its WSL_MEMORY_PROBES_OVERRIDE seam (as
# wsl-memory-guards-selftest.sh does), so the build host's memory never decides
# an outcome. Unless a case picks ROOMY or TIGHT the host reads as
# unmeasurable, and a jump that reaches the gate is held, never pulled.
cat > "$TMP/probes.sh" <<'STUB'
wsl_host_free_kb()       { echo "${FAKE_HOST_FREE_KB-}"; }
wsl_host_total_kb()      { echo 133000000; }
wsl_vm_charge_kb()       { echo "${FAKE_CHARGE_KB-}"; }
wsl_guest_footprint_kb() { echo 20971520; }
wsl_guest_cached_kb()    { echo 5242880; }
wsl_guest_swap_used_kb() { echo 0; }
wsl_psi_full_avg60_pct() { echo 0; }
STUB
export WSL_MEMORY_PROBES_OVERRIDE="$TMP/probes.sh"
unset FAKE_HOST_FREE_KB FAKE_CHARGE_KB PREFLIGHT_SOFT WQM_AUTOPULL_BIG_JUMP_FILES
# Host 60 GB free; the VM charged 40 GB (room) or 75 GB (over the preflight's 70).
ROOMY=(FAKE_HOST_FREE_KB=62914560 FAKE_CHARGE_KB=41943040)
TIGHT=(FAKE_HOST_FREE_KB=62914560 FAKE_CHARGE_KB=78643200)

fail=0; cases=0
pass() { cases=$((cases + 1)); echo "  ok   $1"; }
bad() { cases=$((cases + 1)); fail=1; echo "  FAIL $1"; }

g() { git -C "$1" "${@:2}" >/dev/null 2>&1; }
commit() { echo "$3" > "$1/$2"; g "$1" add "$2"; g "$1" commit -q -m "$3"; }

# A bare origin on `main` (a.txt, b.txt), a clone of it, then 3 more origin
# commits to a.txt the clone has not fetched. Echoes the clone path. The seed
# ("$TMP/<name>-seed") stays usable to move origin further.
new_pair() {
    local name="$1" seed="$TMP/$1-seed" origin="$TMP/$1.git" clone="$TMP/$1"
    git init -q -b main "$seed"
    echo b > "$seed/b.txt"; g "$seed" add b.txt
    commit "$seed" a.txt v1
    git clone -q --bare "$seed" "$origin"
    git clone -q "$origin" "$clone"
    g "$seed" remote add origin "$origin"
    for v in v2 v3 v4; do commit "$seed" a.txt "$v"; done
    g "$seed" push -q origin main
    echo "$clone"
}
seed() { echo "$TMP/$1-seed"; }
tip() { git -C "$(seed "$1")" rev-parse "${2:-main}"; }
head_of() { git -C "$1" rev-parse HEAD; }
origin_of() { git -C "$1" rev-parse "refs/remotes/origin/${2:-main}" 2>/dev/null; }
run() { bash "$SCRIPT" "$@" 2>&1; }
# run with leading VAR=value words as the script's environment.
run_with() {
    local -a e=()
    while [[ $# -gt 0 && "$1" == *=* ]]; do e+=("$1"); shift; done
    env "${e[@]}" bash "$SCRIPT" "$@" 2>&1
}
# One more origin commit adding $2 files: the jump becomes $2 + 1 files.
grow() {
    local s; s=$(seed "$1")
    for i in $(seq 1 "$2"); do echo "$i" > "$s/f$i.txt"; done
    g "$s" add .; g "$s" commit -q -m "grow $2"; g "$s" push -q origin main
}

# $1 label, $2 clone, $3 seed name, $4 expected text in the output, $5 output.
expect_ff() {
    if [[ "$(head_of "$2")" == "$(tip "$3" "${6:-main}")" && "$5" == *"$4"* ]]; then
        pass "$1"; else bad "$1: $5"; fi
}
# $1 label, $2 clone, $3 HEAD before, $4 expected text, $5 output.
expect_skip() {
    if [[ "$(head_of "$2")" == "$3" && "$5" == *"$4"* ]]; then
        pass "$1"; else bad "$1: $5"; fi
}

echo "trunk-autopull selftest ($SCRIPT)"

echo " fast-forwards"
r=$(new_pair clean); out=$(run --repo "$r")
expect_ff "clean checkout behind origin is fast-forwarded" "$r" clean "main fast-forwarded 3 commit(s)" "$out"

r=$(new_pair untracked); echo scratch > "$r/notes.txt"; out=$(run --repo "$r")
expect_ff "an untracked file does not block it" "$r" untracked "fast-forwarded" "$out"
[[ "$(cat "$r/notes.txt")" == scratch ]] && pass "...and the untracked file is kept" || bad "untracked file lost"

r=$(new_pair au-other); g "$r" update-index --assume-unchanged b.txt; echo LOCAL > "$r/b.txt"
out=$(run --repo "$r")
expect_ff "an assume-unchanged edit on a file origin did not touch does not block it" "$r" au-other "fast-forwarded" "$out"
[[ "$(cat "$r/b.txt")" == LOCAL ]] && pass "...and that edit is kept" || bad "assume-unchanged edit lost"

r=$(new_pair noff); g "$r" config merge.ff false; out=$(run --repo "$r")
expect_ff "merge.ff=false in the repo still fast-forwards" "$r" noff "fast-forwarded" "$out"
[[ $(git -C "$r" log -1 --format=%p | wc -w) -eq 1 ]] && pass "...with no merge commit" || bad "merge commit created"

r=$(new_pair hook); printf '#!/bin/sh\ntouch "%s/hook-ran"\n' "$TMP" > "$r/.git/hooks/post-merge"
chmod +x "$r/.git/hooks/post-merge"; out=$(run --repo "$r")
if [[ ! -e "$TMP/hook-ran" && "$(head_of "$r")" == "$(tip hook)" ]]; then
    pass "a .git/hooks post-merge hook does not run"; else bad "hook ran or no ff: $out"; fi

r=$(new_pair husky); mkdir -p "$r/.husky"
printf '#!/bin/sh\ntouch "%s/husky-ran"\n' "$TMP" > "$r/.husky/post-merge"; chmod +x "$r/.husky/post-merge"
g "$r" config core.hooksPath .husky; out=$(run --repo "$r")
if [[ ! -e "$TMP/husky-ran" && "$(head_of "$r")" == "$(tip husky)" ]]; then
    pass "a repo's own core.hooksPath hook does not run"; else bad "husky hook ran or no ff: $out"; fi

r=$(new_pair rp); s=$(seed rp)
g "$s" checkout -q -b release/platform; commit "$s" r.txt r1; g "$s" push -q origin release/platform
g "$r" fetch -q origin; g "$r" checkout -q -b release/platform origin/release/platform
commit "$s" r.txt r2; g "$s" push -q origin release/platform
g "$r" config wqm.trunk release/platform; out=$(run --repo "$r")
expect_ff "wqm.trunk selects a trunk with a slash (release/platform)" "$r" rp "release/platform fast-forwarded 1 commit(s)" "$out" release/platform

r=$(new_pair typo); g "$r" config wqm.trunk relese; out=$(run --repo "$r")
expect_ff "a wqm.trunk typo falls back to the remote's default, as the daemon does" "$r" typo "main fast-forwarded 3 commit(s)" "$out"

r=$(new_pair "with space"); out=$(run --repo "$r")
expect_ff "a path with a space works" "$r" "with space" "fast-forwarded" "$out"

r=$(new_pair narrow); g "$r" config remote.origin.fetch '+refs/heads/other:refs/remotes/origin/other'
out=$(run --repo "$r")
expect_ff "origin/<trunk> is fetched even when remote.origin.fetch names other branches" "$r" narrow "fast-forwarded 3 commit(s)" "$out"

r=$(new_pair current); g "$r" fetch -q origin; g "$r" merge -q --ff-only origin/main; h=$(head_of "$r")
out=$(run --repo "$r")
expect_skip "an up-to-date checkout is reported and left alone" "$r" "$h" "main up to date" "$out"

echo " skips (HEAD and files untouched)"
r=$(new_pair dirty); h=$(head_of "$r"); echo local-edit > "$r/a.txt"; out=$(run --repo "$r")
expect_skip "a modified tracked file" "$r" "$h" "tracked files modified" "$out"
[[ "$(cat "$r/a.txt")" == local-edit ]] && pass "...and the edit is kept" || bad "dirty edit lost"

r=$(new_pair staged); h=$(head_of "$r"); echo staged > "$r/a.txt"; g "$r" add a.txt; out=$(run --repo "$r")
expect_skip "a staged change" "$r" "$h" "tracked files modified or staged" "$out"

r=$(new_pair branch); g "$r" checkout -q -b feature; h=$(head_of "$r"); out=$(run --repo "$r")
expect_skip "a checkout on another branch" "$r" "$h" "checkout is on 'feature', not the trunk 'main'" "$out"

r=$(new_pair detached); g "$r" checkout -q --detach; h=$(head_of "$r"); out=$(run --repo "$r")
expect_skip "a detached HEAD" "$r" "$h" "checkout is on 'detached HEAD'" "$out"

r=$(new_pair diverged); commit "$r" c.txt local; h=$(head_of "$r"); out=$(run --repo "$r")
expect_skip "a local commit origin lacks (diverged)" "$r" "$h" "diverged from origin/main: 1 ahead, 3 behind" "$out"

r=$(new_pair rewritten); s=$(seed rewritten); h=$(head_of "$r")
g "$s" checkout -q --orphan fresh; commit "$s" a.txt rewritten; g "$s" branch -q -M fresh main
g "$s" push -q --force origin main; out=$(run --repo "$r")
expect_skip "an origin whose history was rewritten (force-push)" "$r" "$h" "diverged from origin/main" "$out"

for marker in MERGE_HEAD CHERRY_PICK_HEAD REVERT_HEAD BISECT_LOG rebase-merge rebase-apply; do
    r=$(new_pair "op-$marker"); h=$(head_of "$r")
    case "$marker" in rebase-*) mkdir "$r/.git/$marker" ;; *) echo "$h" > "$r/.git/$marker" ;; esac
    out=$(run --repo "$r")
    expect_skip "an operation in progress: $marker" "$r" "$h" "operation in progress ($marker)" "$out"
done

r=$(new_pair locked); h=$(head_of "$r"); touch "$r/.git/index.lock"; out=$(run --repo "$r")
if [[ "$(head_of "$r")" == "$h" && -e "$r/.git/index.lock" && "$out" == *"operation in progress (index.lock)"* ]]; then
    pass "another git command running (index.lock), and the lock is not removed"; else bad "index.lock: $out"; fi

r=$(new_pair inway); s=$(seed inway); commit "$s" new.txt upstream; g "$s" push -q origin main
h=$(head_of "$r"); echo MINE > "$r/new.txt"; out=$(run --repo "$r")
if [[ "$(head_of "$r")" == "$h" && "$(cat "$r/new.txt")" == MINE && "$out" == *"refused by git"* ]]; then
    pass "an untracked file origin would overwrite: git refuses, the file is kept"; else bad "inway: $out"; fi

r=$(new_pair au-hit); h=$(head_of "$r"); g "$r" update-index --assume-unchanged a.txt
echo LOCAL > "$r/a.txt"; out=$(run --repo "$r")
if [[ "$(head_of "$r")" == "$h" && "$(cat "$r/a.txt")" == LOCAL && "$out" == *"refused by git"* ]]; then
    pass "an edit hidden by assume-unchanged on a file origin changed: refused, kept"; else bad "au-hit: $out"; fi

r=$(new_pair sw-hit); h=$(head_of "$r"); g "$r" update-index --skip-worktree a.txt
echo LOCAL > "$r/a.txt"; out=$(run --repo "$r")
if [[ "$(head_of "$r")" == "$h" && "$(cat "$r/a.txt")" == LOCAL && "$out" == *"refused by git"* ]]; then
    pass "an edit hidden by skip-worktree on a file origin changed: refused, kept"; else bad "sw-hit: $out"; fi

r="$TMP/noorigin"; git init -q -b main "$r"; commit "$r" a.txt v1; h=$(head_of "$r"); out=$(run --repo "$r")
expect_skip "a repo without origin" "$r" "$h" "no origin remote" "$out"

out=$(run --repo "$TMP/does-not-exist")
[[ "$out" == *"not a git work tree root"* ]] && pass "a path that does not exist" || bad "missing path: $out"

r=$(new_pair sub); mkdir -p "$r/pkg"; h=$(head_of "$r"); out=$(run --repo "$r/pkg")
expect_skip "a sub-directory of a repo (not its root)" "$r" "$h" "not a git work tree root" "$out"

r=$(new_pair nofetch); g "$r" remote set-url origin "$TMP/gone.git"; h=$(head_of "$r"); out=$(run --repo "$r")
expect_skip "a failing fetch" "$r" "$h" "fetch of origin/main failed" "$out"

# origin/main naming a blob: rev-list cannot count, which must read as "skip",
# never as "0 behind = up to date" (dry run, so no fetch repairs the ref).
r=$(new_pair uncomparable); h=$(head_of "$r")
g "$r" update-ref refs/remotes/origin/main "$(git -C "$r" rev-parse HEAD:b.txt)"
out=$(run --dry-run --repo "$r")
expect_skip "an origin/<trunk> git cannot compare with HEAD" "$r" "$h" "cannot compare HEAD with origin/main" "$out"

r=$(new_pair fulldisk); h=$(head_of "$r")
out=$(WQM_AUTOPULL_MIN_FREE_GB=99999999 bash "$SCRIPT" --repo "$r" 2>&1)
expect_skip "too little free disk for the fast-forward" "$r" "$h" "GiB free" "$out"

echo " fetch semantics"
r=$(new_pair fetched-dirty); echo x > "$r/a.txt"; run --repo "$r" >/dev/null
[[ "$(origin_of "$r")" == "$(tip fetched-dirty)" ]] && pass "a skipped dirty checkout still has origin/<trunk> fetched" || bad "dirty: origin/main stale"

r=$(new_pair fetched-branch); g "$r" checkout -q -b feature; run --repo "$r" >/dev/null
[[ "$(origin_of "$r")" == "$(tip fetched-branch)" ]] && pass "a checkout on another branch still has origin/<trunk> fetched" || bad "branch: origin/main stale"

# Without a fetch, origin/main is still the clone's v1 = HEAD: "up to date" is
# the proof the dry run read the stale ref rather than fetching.
r=$(new_pair dry-nofetch); before=$(origin_of "$r"); h=$(head_of "$r"); out=$(run --dry-run --repo "$r")
if [[ "$(origin_of "$r")" == "$before" && "$(head_of "$r")" == "$h" && "$out" == *"main up to date"* ]]; then
    pass "--dry-run fetches nothing and changes nothing"; else bad "dry-run fetched or moved: $out"; fi

r=$(new_pair dry); g "$r" fetch -q origin; h=$(head_of "$r"); out=$(run --dry-run --repo "$r")
expect_skip "--dry-run reports what it would do from the refs on disk" "$r" "$h" "would fast-forward main 3 commit(s), 1 file(s)" "$out"

echo " big jumps wait for memory headroom"
r=$(new_pair big-roomy); out=$(run_with "${ROOMY[@]}" WQM_AUTOPULL_BIG_JUMP_FILES=1 --repo "$r")
expect_ff "a big jump with memory headroom is fast-forwarded" "$r" big-roomy "main fast-forwarded 3 commit(s), 1 file(s)" "$out"

r=$(new_pair big-tight); h=$(head_of "$r"); out=$(run_with "${TIGHT[@]}" WQM_AUTOPULL_BIG_JUMP_FILES=1 --repo "$r")
expect_skip "a big jump without headroom is held, with the preflight's reason" "$r" "$h" \
    "big jump (3 commit(s), 1 file(s), ≥ 1) held: memory is short: host is charged 75.0 GB for the VM — above VM_MAX_FOOTPRINT_GB=70; retried next run" "$out"
[[ "$(origin_of "$r")" == "$(tip big-tight)" ]] && pass "...and origin/<trunk> was still fetched" || bad "big-tight: origin/main stale"

r=$(new_pair big-unmeasured); h=$(head_of "$r"); out=$(run_with WQM_AUTOPULL_BIG_JUMP_FILES=1 --repo "$r")
expect_skip "a big jump when memory cannot be measured is held" "$r" "$h" "held: memory cannot be measured (host-memory-preflight exit 2)" "$out"

r=$(new_pair big-soft); h=$(head_of "$r")
out=$(run_with "${TIGHT[@]}" PREFLIGHT_SOFT=1 WQM_AUTOPULL_BIG_JUMP_FILES=1 --repo "$r")
expect_skip "PREFLIGHT_SOFT=1 in the environment does not wave a big jump through" "$r" "$h" "held: memory is short" "$out"

r=$(new_pair small-tight); out=$(run_with "${TIGHT[@]}" WQM_AUTOPULL_BIG_JUMP_FILES=2 --repo "$r")
expect_ff "a jump under the threshold goes even when memory is short" "$r" small-tight "fast-forwarded 3 commit(s), 1 file(s)" "$out"

r=$(new_pair gate-off); out=$(run_with "${TIGHT[@]}" WQM_AUTOPULL_BIG_JUMP_FILES=0 --repo "$r")
expect_ff "WQM_AUTOPULL_BIG_JUMP_FILES=0 turns the check off" "$r" gate-off "fast-forwarded" "$out"

r=$(new_pair def-499); grow def-499 498; out=$(run_with "${TIGHT[@]}" --repo "$r")
expect_ff "by default a 499-file jump goes even when memory is short" "$r" def-499 "fast-forwarded 4 commit(s), 499 file(s)" "$out"

r=$(new_pair def-500); grow def-500 499; h=$(head_of "$r"); out=$(run_with "${TIGHT[@]}" --repo "$r")
expect_skip "by default a 500-file jump waits for headroom" "$r" "$h" "big jump (4 commit(s), 500 file(s), ≥ 500) held" "$out"

r=$(new_pair def-junk); grow def-junk 499; h=$(head_of "$r")
out=$(run_with "${TIGHT[@]}" WQM_AUTOPULL_BIG_JUMP_FILES=lots --repo "$r")
expect_skip "a non-numeric threshold means the default (500), never off" "$r" "$h" "≥ 500) held" "$out"

# "09" is 9, not a bad octal number that makes the comparison error out (and
# the jump pass as small).
r=$(new_pair lead-zero); grow lead-zero 8; h=$(head_of "$r")
out=$(run_with "${TIGHT[@]}" WQM_AUTOPULL_BIG_JUMP_FILES=09 --repo "$r")
expect_skip "a threshold with a leading zero is read as decimal" "$r" "$h" "big jump (4 commit(s), 9 file(s), ≥ 9) held" "$out"

r=$(new_pair big-dry); g "$r" fetch -q origin; h=$(head_of "$r")
out=$(run_with "${TIGHT[@]}" WQM_AUTOPULL_BIG_JUMP_FILES=1 --dry-run --repo "$r")
expect_skip "--dry-run reports a hold and changes nothing" "$r" "$h" "would hold main — big jump (3 commit(s), 1 file(s), ≥ 1): memory is short" "$out"

# The changed files cannot be listed (origin's root tree is missing; the commits
# are there, so rev-list still counts): that must never read as a small jump.
r=$(new_pair unlisted); g "$r" fetch -q origin; h=$(head_of "$r")
t=$(git -C "$r" rev-parse "origin/main^{tree}"); obj="$r/.git/objects/${t:0:2}/${t:2}"
if [[ -f "$obj" ]]; then
    rm -f "$obj"; out=$(run_with "${TIGHT[@]}" --dry-run --repo "$r")
    expect_skip "a jump whose files git cannot list counts as big" "$r" "$h" "big jump (3 commit(s), ? file(s), ≥ 500)" "$out"
else bad "unlisted: origin's tree is not a loose object ($obj)"; fi

echo " process"
r=$(new_pair busy); h=$(head_of "$r")
# Both sides open the script read-only (as the script itself does): `flock
# <path>` would create the file if the path were wrong.
( flock 9; exec sleep 5 ) 9<"$SCRIPT" & holder=$!
for _ in $(seq 1 50); do ( flock -n 9 ) 9<"$SCRIPT" 2>/dev/null || break; sleep 0.1; done
out=$(run --repo "$r"); rc=$?
kill "$holder" 2>/dev/null; wait "$holder" 2>/dev/null
if [[ $rc -eq 0 && "$(head_of "$r")" == "$h" && "$out" == *"another run holds the lock"* ]]; then
    pass "a second run while one holds the lock exits 0 and touches nothing"; else bad "lock: rc=$rc $out"; fi

bash "$SCRIPT" --bogus >/dev/null 2>&1; rc=$?
[[ $rc -eq 2 ]] && pass "a usage error exits 2" || bad "usage rc=$rc"

# This selftest, pointed at a root without the script, fails and writes
# nothing into that root. One level deep only (the nested run skips this case,
# so it can never recurse), and bounded, so a regression fails the build
# instead of hanging it.
if [[ -z "${WQM_AUTOPULL_SELFTEST_NESTED:-}" ]]; then
    nr="$TMP/no-root"; mkdir -p "$nr/scripts"
    WQM_AUTOPULL_SELFTEST_NESTED=1 timeout 120 bash "$0" "$nr" >/dev/null 2>&1; rc=$?
    if [[ $rc -ne 0 && $rc -ne 124 && -z "$(ls -A "$nr/scripts")" ]]; then
        pass "the selftest on a root without the script fails and writes nothing there"
    else bad "no-root: rc=$rc, left: [$(ls -A "$nr/scripts")]"; fi
fi

a=$(new_pair multi-a); b=$(new_pair multi-b); hb=$(head_of "$b"); echo edit > "$b/a.txt"
out=$(run --repo "$a" --repo "$b" --repo "$TMP/multi-missing"); rc=$?
lines=$(grep -c '' <<<"$out"); stamped=$(grep -cE '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z ' <<<"$out")
if [[ $rc -eq 0 && $lines -eq 3 && $stamped -eq 3 && "$(head_of "$a")" == "$(tip multi-a)" && "$(head_of "$b")" == "$hb" ]]; then
    pass "a mixed run: exit 0, one timestamped line per repo, each repo on its own"; else bad "multi: rc=$rc lines=$lines $out"; fi

if [[ $fail -ne 0 ]]; then echo "trunk-autopull selftest FAILED"; exit 1; fi
if [[ $cases -ne $EXPECTED_CASES ]]; then
    echo "trunk-autopull selftest ran $cases cases, expected $EXPECTED_CASES"; exit 1
fi
echo "$cases cases passed"
