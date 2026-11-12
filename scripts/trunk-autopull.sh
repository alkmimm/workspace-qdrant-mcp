#!/usr/bin/env bash
# Fast-forward the MAIN checkout of every watched project to origin/<trunk>,
# only when that is provably safe. Run hourly by wqm-trunk-autopull.timer.
#
# Why this exists: the daemon indexes what is on disk, and nothing kept the
# main checkouts current. On 2026-11-12 DOC-V2's main checkout sat 412 commits
# behind origin/main (emnify 274), so every search on its trunk answered from
# code months old, and the worktree reconcile diffed every agent branch against
# that stale HEAD (38k redundant queue items; fixed separately in #454).
#
# Every repo with an `origin` remote and a resolvable trunk (`git config
# wqm.trunk` when it names a branch the repo has, else origin/HEAD, else
# main/master — the daemon's `default_branch` order) is FETCHED first:
# `git fetch origin <trunk>` moves only the remote-tracking ref, never the
# working tree, and keeps the daemon's view of origin fresh even for a checkout
# this script then leaves alone (the trunk followed from git, and the
# checkout-lag warning, both read origin/<trunk> as of the last fetch).
#
# The checkout is the user's working copy, so this never decides anything for
# them. It advances a repo only when ALL hold, and otherwise logs why it did
# not and changes nothing:
#   - HEAD is the trunk branch (a checkout on another branch, or detached, is
#     left alone);
#   - no merge, rebase, am, cherry-pick, revert or bisect is in progress, and
#     no other git command holds the index lock;
#   - no tracked file is modified or staged (untracked files do not block: a
#     fast-forward that would overwrite one fails on its own, untouched);
#   - after the fetch, HEAD is an ancestor of origin/<trunk> (local commits
#     not on origin, or an origin rewritten under them = diverged, skipped);
#   - the checkout's filesystem has WQM_AUTOPULL_MIN_FREE_GB free (default 8).
# The advance is `git merge --ff-only` with hooks disabled (also a repo's own
# core.hooksPath, e.g. husky): no merge commit even under merge.ff=false, no
# rebase, no user hook runs. git refuses it, changing nothing, when it would
# overwrite an untracked file or an edit hidden by assume-unchanged or
# skip-worktree. The daemon then sees ordinary file changes and re-indexes
# only what changed. scripts/ci/trunk-autopull-selftest.sh pins every rule.
#
# Usage:
#   trunk-autopull.sh [--dry-run] [--repo <path>]...
#     --repo     act on these repos instead of the daemon's watched projects
#                (repeatable; used by scripts/ci/trunk-autopull-selftest.sh)
#     --dry-run  report what would happen; fetches nothing, changes nothing
#                (the numbers use the origin refs already on disk)
# Exit: 0 always once arguments parse (skips are logged, not errors); 2 on a
# usage error. Two runs never overlap (flock); the second exits 0.
set -u

DRY_RUN=0
REPOS=()
usage() { echo "usage: $0 [--dry-run] [--repo <path>]..." >&2; exit 2; }
while [[ $# -gt 0 ]]; do
    case "$1" in
        --dry-run) DRY_RUN=1; shift ;;
        --repo) [[ $# -ge 2 ]] || usage; REPOS+=("$2"); shift 2 ;;
        -h|--help) usage ;;
        *) usage ;;
    esac
done

log() { printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*"; }

# Free space a fast-forward needs on the checkout's filesystem (GiB).
MIN_FREE_GB="${WQM_AUTOPULL_MIN_FREE_GB:-8}"
[[ "$MIN_FREE_GB" =~ ^[0-9]+$ ]] || MIN_FREE_GB=8
# Reads never take git's optional locks (the index refresh `git status` does),
# so a check never collides with a git command the user is running.
export GIT_OPTIONAL_LOCKS=0

# The lock is the script file itself, opened read-only: no lock file to create,
# and none another user (a root run) could leave behind unwritable.
exec 9<"${BASH_SOURCE[0]}" || { log "cannot open ${BASH_SOURCE[0]} to lock it"; exit 0; }
if ! flock -n 9; then
    log "another run holds the lock; nothing to do"
    exit 0
fi

# The watched projects' MAIN folders, from the daemon's own registry. The
# container sees the same host paths (the dev root is bind-mounted at its host
# path), so the paths are usable here as-is.
watched_repos() {
    if ! docker inspect -f '{{.State.Running}}' wqm-memexd 2>/dev/null | grep -qx true; then
        log "wqm-memexd is not running; no project list, nothing to do" >&2
        return 1
    fi
    docker exec -i wqm-memexd python3 - <<'PY'
import sqlite3, sys
c = sqlite3.connect("file:/var/lib/memexd/memexd.db?mode=ro", uri=True)
cols = {r[1] for r in c.execute("PRAGMA table_info(watch_folders)")}
need = {"path", "collection", "enabled", "parent_watch_id", "is_worktree"}
if not need <= cols:
    sys.exit("watch_folders lacks " + ", ".join(sorted(need - cols)))
for (p,) in c.execute(
    "SELECT DISTINCT path FROM watch_folders WHERE collection = 'projects' "
    "AND enabled = 1 AND parent_watch_id IS NULL AND is_worktree = 0 ORDER BY path"):
    print(p)
PY
}

# The trunk branch name for repo $1, or nothing.
trunk_of() {
    local repo="$1" t
    t=$(git -C "$repo" config --get wqm.trunk 2>/dev/null)
    # Honoured only when it names a branch the repo has, locally or on origin,
    # as the daemon does: a typo falls back instead of disagreeing with it.
    if [[ -n "$t" ]] && ! git -C "$repo" show-ref --verify --quiet "refs/heads/$t" \
        && ! git -C "$repo" show-ref --verify --quiet "refs/remotes/origin/$t"; then
        t=""
    fi
    if [[ -z "$t" ]]; then
        t=$(git -C "$repo" symbolic-ref --quiet --short refs/remotes/origin/HEAD 2>/dev/null)
        t="${t#origin/}"
    fi
    if [[ -z "$t" ]]; then
        for cand in main master; do
            if git -C "$repo" show-ref --verify --quiet "refs/heads/$cand"; then t="$cand"; break; fi
        done
    fi
    printf '%s' "$t"
}

# One repo: prints exactly one log line.
autopull_one() {
    local repo="$1" name trunk head gitdir marker status counts old new n ahead behind kb free_gb
    name=$(basename "$repo")
    if [[ "$(git -C "$repo" rev-parse --is-inside-work-tree 2>/dev/null)" != true ]] \
        || [[ "$(git -C "$repo" rev-parse --show-toplevel 2>/dev/null)" != "$(cd "$repo" 2>/dev/null && pwd -P)" ]]; then
        log "$name: skip — not a git work tree root ($repo)"; return
    fi
    if ! git -C "$repo" remote get-url origin >/dev/null 2>&1; then
        log "$name: skip — no origin remote"; return
    fi
    trunk=$(trunk_of "$repo")
    if [[ -z "$trunk" ]]; then
        log "$name: skip — no trunk (set it with: git config wqm.trunk <branch>)"; return
    fi
    if [[ $DRY_RUN -eq 0 ]]; then
        # An explicit refspec: origin/<trunk> is updated even when the repo's
        # remote.origin.fetch names other branches only.
        if ! GIT_TERMINAL_PROMPT=0 GIT_SSH_COMMAND='ssh -o BatchMode=yes' \
            timeout 120 git -C "$repo" fetch --quiet origin \
            "+refs/heads/$trunk:refs/remotes/origin/$trunk" 2>/dev/null; then
            log "$name: skip — fetch of origin/$trunk failed"; return
        fi
    fi
    head=$(git -C "$repo" symbolic-ref --quiet --short HEAD 2>/dev/null)
    if [[ "$head" != "$trunk" ]]; then
        log "$name: skip — checkout is on '${head:-detached HEAD}', not the trunk '$trunk'"; return
    fi
    gitdir=$(git -C "$repo" rev-parse --absolute-git-dir 2>/dev/null)
    # index.lock: another git command is running in this checkout right now.
    for marker in MERGE_HEAD CHERRY_PICK_HEAD REVERT_HEAD BISECT_LOG rebase-merge rebase-apply index.lock; do
        if [[ -e "$gitdir/$marker" ]]; then
            log "$name: skip — operation in progress ($marker)"; return
        fi
    done
    if ! status=$(git -C "$repo" status --porcelain --untracked-files=no 2>/dev/null); then
        log "$name: skip — git status failed"; return
    fi
    if [[ -n "$status" ]]; then
        log "$name: skip — tracked files modified or staged"; return
    fi
    if ! git -C "$repo" rev-parse --verify --quiet "refs/remotes/origin/$trunk" >/dev/null; then
        log "$name: skip — origin/$trunk does not exist"; return
    fi
    if ! counts=$(git -C "$repo" rev-list --left-right --count "HEAD...origin/$trunk" 2>/dev/null) \
        || ! read -r ahead behind <<<"$counts" \
        || ! [[ "$ahead" =~ ^[0-9]+$ && "$behind" =~ ^[0-9]+$ ]]; then
        log "$name: skip — cannot compare HEAD with origin/$trunk (shallow clone?)"; return
    fi
    if [[ "$ahead" -gt 0 ]]; then
        log "$name: skip — diverged from origin/$trunk: $ahead ahead, $behind behind"; return
    fi
    if [[ "$behind" -eq 0 ]]; then
        log "$name: $trunk up to date"; return
    fi
    old=$(git -C "$repo" rev-parse --short HEAD)
    if [[ $DRY_RUN -eq 1 ]]; then
        log "$name: would fast-forward $trunk $behind commit(s) from $old (dry run, origin refs not fetched)"; return
    fi
    # A large fast-forward writes many files; never on a nearly full disk (the
    # C: incident). Free space of the filesystem holding the checkout, in GiB.
    # Unknown free space counts as none: skip rather than guess.
    kb=$(df -Pk "$repo" 2>/dev/null | awk 'NR==2 {print $4}')
    [[ "$kb" =~ ^[0-9]+$ ]] || kb=0
    free_gb=$(( kb / 1048576 ))
    if [[ "$free_gb" -lt "$MIN_FREE_GB" ]]; then
        log "$name: skip — only ${free_gb} GiB free (< ${MIN_FREE_GB}); nothing changed"; return
    fi
    # git itself refuses, changing nothing, when the update would overwrite an
    # untracked file or a local edit hidden by assume-unchanged/skip-worktree,
    # or when another git command took the index lock in the meantime.
    if ! git -C "$repo" -c core.hooksPath=/dev/null merge --ff-only --quiet "origin/$trunk" >/dev/null 2>&1; then
        log "$name: skip — fast-forward refused by git (a local file in the way, or another git command running); nothing changed"; return
    fi
    new=$(git -C "$repo" rev-parse --short HEAD)
    n="$behind"
    log "$name: $trunk fast-forwarded $n commit(s) ($old..$new)"
}

if [[ ${#REPOS[@]} -eq 0 ]]; then
    list=$(watched_repos) || exit 0
    mapfile -t REPOS <<<"$list"
fi
for repo in "${REPOS[@]}"; do
    [[ -n "$repo" ]] || continue
    autopull_one "$repo"
done
exit 0
