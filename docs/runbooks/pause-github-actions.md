# Runbook: Pause / Resume GitHub Actions

Temporarily stop **all** GitHub Actions from running — useful while iterating
heavily on a long-lived branch (e.g. `fork/fixes`, which gets no CI of its own),
to avoid burning Actions minutes on a large promote PR, or to silence noisy runs
while a fix is in progress.

> **Prerequisite:** the [`gh`](https://cli.github.com/) CLI, authenticated
> (`gh auth status`). Run the commands from inside a clone of the repo so `gh`
> infers the repository automatically — otherwise add `--repo OWNER/REPO`.

> **Note:** disabling a workflow stops **future** triggers (push / PR /
> schedule). It does **not** cancel runs that are already in progress — cancel
> those separately (see below).

## Pause everything (disable all workflows)

```bash
gh api repos/{owner}/{repo}/actions/workflows --jq '.workflows[].id' \
  | while read -r id; do gh workflow disable "$id"; done
```

Then cancel any in-progress or queued runs so nothing keeps executing:

```bash
gh run list --limit 50 --json databaseId,status \
  --jq '.[] | select(.status=="in_progress" or .status=="queued") | .databaseId' \
  | while read -r rid; do gh run cancel "$rid"; done
```

## Resume everything (re-enable all workflows)

```bash
gh api repos/{owner}/{repo}/actions/workflows --jq '.workflows[].id' \
  | while read -r id; do gh workflow enable "$id"; done
```

## Single workflow

By file name (preferred — stable across workflow recreation) or by display name:

```bash
gh workflow disable ci.yml      # or: gh workflow disable "CI"
gh workflow enable  ci.yml      # or: gh workflow enable  "CI"
```

## Check current state

```bash
# State of every workflow (active | disabled_manually | disabled_inactivity)
gh api repos/{owner}/{repo}/actions/workflows --jq '.workflows[] | "\(.state)\t\(.name)"'

# How many are still active
gh api repos/{owner}/{repo}/actions/workflows \
  --jq '[.workflows[] | select(.state=="active")] | length'

# Any runs still executing
gh run list --limit 20 --json status,workflowName \
  --jq '.[] | select(.status=="in_progress" or .status=="queued") | .workflowName'
```

## Notes

- The `{owner}/{repo}` placeholders are filled automatically by `gh` when you
  omit them and run from inside the repo clone.
- Pausing affects the whole repository, not a single branch or PR. PRs simply
  show no checks until the relevant workflows are re-enabled.
- Re-enabling does **not** retroactively run skipped triggers — push a new
  commit (or use `gh workflow run <file>` for `workflow_dispatch` workflows) if
  you want a fresh run.
- `CI` (`ci.yml`) only triggers on PRs to `main`/`dev`, so work merged to the
  long-lived `fork/fixes` branch is validated locally via `scripts/validate.sh`
  (the containerized clippy + build gate) rather than by Actions.
