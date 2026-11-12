# Keeping watched projects' main checkouts current (trunk autopull)

The daemon indexes what is **on disk**. Nothing used to keep a watched
project's main checkout up to date, so it drifted: on 2026-11-12 DOC-V2's main
checkout was 412 commits behind `origin/main` and emnify's 274. Every search on
those trunks answered from old code, and the worktree reconcile diffed each
agent branch against the stale HEAD.

A trunk checked out in a linked worktree is already followed from git (#449,
`git::followed_trunk`). The main folder is the user's working copy, so the
daemon never moves it. `scripts/trunk-autopull.sh` does, on a timer, but only
when it is safe.

## What it does

The script runs hourly and acts on every watched project: the `projects` watch
folders that are enabled and are not worktrees, read from the daemon's
registry.

For each one it first runs `git fetch origin <trunk>`. A fetch moves only the
remote-tracking ref, never the working tree, so it runs even for a checkout the
script then leaves alone. That keeps the daemon's view of origin fresh: the
trunk followed from git (#449) and the checkout-lag warning (#456) both read
`origin/<trunk>` as of the last fetch.

It then runs `git merge --ff-only origin/<trunk>` with hooks disabled. Only that
fast-forward ever moves HEAD, and it skips a repo, logging the reason and
changing nothing, unless all of these hold:

| Rule | Skip message |
|---|---|
| an `origin` remote exists | `no origin remote` |
| the trunk resolves, in the daemon's order: `git config wqm.trunk` when it names a branch the repo has, else `origin/HEAD`, else `main` or `master` | `no trunk` |
| the fetch works (non-interactive, 120 s cap) | `fetch of origin/<trunk> failed` |
| HEAD is the trunk branch | `checkout is on '<branch>', not the trunk` |
| no merge, rebase, cherry-pick, revert or bisect is in progress, and no other git command holds `index.lock` | `operation in progress (<marker>)` |
| no tracked file is modified or staged (untracked files are fine) | `tracked files modified or staged` |
| HEAD has no commits that origin lacks | `diverged from origin/<trunk>: N ahead, M behind` |
| a big jump (≥ `WQM_AUTOPULL_BIG_JUMP_FILES` changed files, default 500) has memory headroom | `big jump (…) held: memory is short: <reason>` |
| the checkout's filesystem has `WQM_AUTOPULL_MIN_FREE_GB` free (default 8) | `only N GiB free` |

Each run writes one line per repo, for example:

```
2026-11-12T15:02:38Z v0-bws-training: main fast-forwarded 8 commit(s), 81 file(s) (036a5c5f..9cea3dea)
2026-11-12T15:02:37Z tecsul: skip — checkout is on 'dev-alberto', not the trunk 'release/platform'
2026-11-12T16:02:30Z DOC-V2: skip — big jump (412 commit(s), 3013 file(s), ≥ 500) held: memory is short: host is charged 74.6 GB for the VM — above VM_MAX_FOOTPRINT_GB=70; retried next run (…)
```

The daemon then sees ordinary file changes and re-indexes only the files that
changed.

### Big jumps wait for memory headroom

Re-indexing a fast-forward costs in proportion to the files it changes. A
checkout kept current moves in small steps: over the 60 days to 2026-11-12, the
files changed per active hour on a trunk had a p99 of 65–170 on most repos. A
catch-up is another matter. On 2026-11-12 the first run moved emnify by 764
files, and two minutes later the memory guard stopped the stack (VM charged
80.3 GB, on a host already near its line). DOC-V2's pending catch-up was 3,013
files.

So a jump of at least `WQM_AUTOPULL_BIG_JUMP_FILES` changed files (default 500)
also needs memory headroom. It goes ahead only when
`scripts/host-memory-preflight.sh` passes, the same check `make stack-up` runs
before a start: the VM charged at most 70 GB, the host with at least 24 GB free,
no memory pressure. Those lines sit 10 GB under the guard's stop line.

- **Held, not dropped:** the fetch still runs, and every hourly run tries
  again. At a quiet hour the VM hands its page cache back and the check passes.
- **Unmeasurable memory holds it too**, as unknown free disk does. So does a
  change list git cannot produce: it never counts as small.
- **Small jumps never ask.** At the default threshold this holds only
  catch-ups and the odd release promotion (bws-engineer: 300–905 files).
- **To pull a held repo now**, at a moment you choose:
  `WQM_AUTOPULL_BIG_JUMP_FILES=0 make autopull`. `0` turns the check off for
  that run.
- **A dry run** (`make autopull-dry-run`) runs the same check and reports
  `would hold`.

A repo held for hours stays visible: the `checkout_stale` warning and the
`MainCheckoutBehindOrigin` alert (#456) keep reporting it.

## Install, check, run by hand

```bash
make autopull-dry-run     # what it would do now; fetches nothing, changes nothing
make autopull-install     # hourly system timer running as you (sudo once)
make autopull-status      # timer state + the last run's lines
journalctl -u wqm-trunk-autopull -n 50 -o cat
make autopull             # one run now
```

The timer is a **system** unit that runs as the user, like `wqm-memory-guard`.
At WSL boot there is no login session, so a `systemctl --user` timer would not
fire. `Persistent=true` catches up a slot missed while the VM was down (if the
stack is not up yet, that run finds no project list and does nothing). Two runs
never overlap: the script locks its own file, and a second run exits at once.

All watched repos use HTTPS remotes with the `gh` credential helper (checked
2026-11-12), which works without a login session; a remote that needs an SSH
agent fails the fetch and is reported.

To keep a repo out without touching the daemon, leave its main checkout on
another branch: the script only acts on a checkout sitting on its trunk.

Two cases are reported but never resolved, because they are the user's call:
- **Fetch fails:** an SSH key with a passphrase or no agent, or an expired
  credential. Fix the credential.
- **Diverged:** local commits on the trunk. Push them, or move them to a
  branch.

`scripts/ci/trunk-autopull-selftest.sh` exercises every rule against
throwaway repos. It runs in the `static-checks` stage of
`docker/Dockerfile.memexd` on every image build.
