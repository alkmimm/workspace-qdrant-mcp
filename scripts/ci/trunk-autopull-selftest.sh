#!/usr/bin/env bash
# Selftest for scripts/trunk-autopull.sh: builds throwaway origin/clone pairs
# and asserts the script fast-forwards ONLY the safe case and leaves every
# other checkout byte-for-byte where it was. Runs in the static-checks stage
# (docker/Dockerfile.memexd) and by hand:  bash scripts/ci/trunk-autopull-selftest.sh [<repo root>]
set -u

ROOT="${1:-$(cd "$(dirname "$0")/../.." && pwd)}"
SCRIPT="$ROOT/scripts/trunk-autopull.sh"
EXPECTED_CASES=9

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
export XDG_RUNTIME_DIR="$TMP/run"; mkdir -p "$XDG_RUNTIME_DIR"
# Hermetic git: no user/system config, a fixed identity for the commits.
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t

fail=0; cases=0
pass() { cases=$((cases + 1)); echo "  ok   $1"; }
bad() { cases=$((cases + 1)); fail=1; echo "  FAIL $1"; }

g() { git -C "$1" "${@:2}" >/dev/null 2>&1; }
commit() { echo "$3" > "$1/$2"; g "$1" add "$2"; g "$1" commit -q -m "$3"; }

# A bare origin on `main`, a clone of it, then 3 more origin commits the clone
# has not seen. Echoes the clone path.
new_pair() {
    local name="$1" seed="$TMP/$1-seed" origin="$TMP/$1.git" clone="$TMP/$1"
    git init -q -b main "$seed"
    commit "$seed" a.txt v1
    git clone -q --bare "$seed" "$origin"
    git clone -q "$origin" "$clone"
    g "$seed" remote add origin "$origin"
    for v in v2 v3 v4; do commit "$seed" a.txt "$v"; done
    g "$seed" push -q origin main
    echo "$clone"
}
head_of() { git -C "$1" rev-parse HEAD; }
run() { bash "$SCRIPT" "$@" 2>&1; }

echo "trunk-autopull selftest ($SCRIPT)"

# 1. clean, behind -> fast-forwarded to origin/main.
r=$(new_pair clean)
out=$(run --repo "$r")
if [[ "$(head_of "$r")" == "$(git -C "$r" rev-parse origin/main)" && "$out" == *"fast-forwarded 3 commit(s)"* ]]; then
    pass "clean checkout behind origin is fast-forwarded"; else bad "clean: $out"; fi

# 2. a modified tracked file -> skipped, HEAD and the edit untouched.
r=$(new_pair dirty); h=$(head_of "$r"); echo local-edit > "$r/a.txt"
out=$(run --repo "$r")
if [[ "$(head_of "$r")" == "$h" && "$(cat "$r/a.txt")" == local-edit && "$out" == *"tracked files modified"* ]]; then
    pass "dirty tracked file is skipped untouched"; else bad "dirty: $out"; fi

# 3. checkout on another branch -> skipped.
r=$(new_pair branch); g "$r" checkout -q -b feature; h=$(head_of "$r")
out=$(run --repo "$r")
if [[ "$(head_of "$r")" == "$h" && "$(git -C "$r" rev-parse --abbrev-ref HEAD)" == feature && "$out" == *"not the trunk"* ]]; then
    pass "checkout on another branch is skipped"; else bad "branch: $out"; fi

# 4. a local commit not on origin -> diverged, skipped.
r=$(new_pair diverged); commit "$r" b.txt local; h=$(head_of "$r")
out=$(run --repo "$r")
if [[ "$(head_of "$r")" == "$h" && "$out" == *"diverged from origin/main: 1 ahead, 3 behind"* ]]; then
    pass "diverged checkout is skipped"; else bad "diverged: $out"; fi

# 5. a merge in progress (MERGE_HEAD) -> skipped.
r=$(new_pair merging); h=$(head_of "$r"); echo "$h" > "$r/.git/MERGE_HEAD"
out=$(run --repo "$r")
if [[ "$(head_of "$r")" == "$h" && "$out" == *"operation in progress (MERGE_HEAD)"* ]]; then
    pass "mid-merge checkout is skipped"; else bad "merging: $out"; fi

# 6. only an untracked file -> still fast-forwarded, the file kept.
r=$(new_pair untracked); echo scratch > "$r/notes.txt"
out=$(run --repo "$r")
if [[ "$(head_of "$r")" == "$(git -C "$r" rev-parse origin/main)" && -f "$r/notes.txt" && "$out" == *"fast-forwarded"* ]]; then
    pass "untracked file does not block the fast-forward"; else bad "untracked: $out"; fi

# 7. no origin remote -> skipped.
r="$TMP/noorigin"; git init -q -b main "$r"; commit "$r" a.txt v1; h=$(head_of "$r")
out=$(run --repo "$r")
if [[ "$(head_of "$r")" == "$h" && "$out" == *"no origin remote"* ]]; then
    pass "repo without origin is skipped"; else bad "noorigin: $out"; fi

# 8. --dry-run -> reports, changes nothing (origin ref fetched beforehand so
#    the dry run has something to report).
r=$(new_pair dry); g "$r" fetch -q origin; h=$(head_of "$r")
out=$(run --dry-run --repo "$r")
if [[ "$(head_of "$r")" == "$h" && "$out" == *"would fast-forward main 3 commit(s)"* ]]; then
    pass "--dry-run changes nothing"; else bad "dry-run: $out"; fi

# 9. wqm.trunk wins over origin/HEAD: trunk = release, checkout on release.
r=$(new_pair trunkcfg); seed="$TMP/trunkcfg-seed"
g "$seed" checkout -q -b release; commit "$seed" r.txt r1; g "$seed" push -q origin release
g "$r" fetch -q origin; g "$r" checkout -q -b release origin/release
commit "$seed" r.txt r2; g "$seed" push -q origin release
printf '[wqm]\n\ttrunk = release\n' >> "$r/.git/config"
out=$(run --repo "$r")
if [[ "$(head_of "$r")" == "$(git -C "$r" rev-parse origin/release)" && "$out" == *"release fast-forwarded 1 commit(s)"* ]]; then
    pass "wqm.trunk selects the trunk"; else bad "trunkcfg: $out"; fi

if [[ $fail -ne 0 ]]; then echo "trunk-autopull selftest FAILED"; exit 1; fi
if [[ $cases -ne $EXPECTED_CASES ]]; then
    echo "trunk-autopull selftest ran $cases cases, expected $EXPECTED_CASES"; exit 1
fi
echo "$cases cases passed"
