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
| no merge, rebase, cherry-pick, revert or bisect is in progress | `operation in progress (<marker>)` |
| no tracked file is modified or staged (untracked files are fine) | `tracked files modified or staged` |
| HEAD has no commits that origin lacks | `diverged from origin/<trunk>: N ahead, M behind` |

Each run writes one line per repo, for example:

```
2026-11-12T13:00:04Z DOC-V2: main fast-forwarded 412 commit(s) (1a2b3c4..5d6e7f8)
2026-11-12T13:00:05Z tecsul: skip — checkout is on 'master', not the trunk 'release/platform'
```

The daemon then sees ordinary file changes and re-indexes only the files that
changed. The first pull of a checkout that is far behind queues that one batch
of work.

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
