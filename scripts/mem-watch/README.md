# mem-watch — memexd memory-growth watch

A tiny soak harness for the one signal the Prometheus stack does **not** carry:
memexd's resident memory over days. memexd exports only app-level `memexd_*`
metrics (no process RSS), so without this there is nothing to look at after a
slow climb. Background: `docs/deployment/reliability.md` → *Memory-growth watch*.

## Files

| File | Role |
|------|------|
| `sampler.sh` | append one CSV row per interval: host MemAvailable + per-container `cur`/`anon` GiB |
| `analyze.py` | linear-regression slope on **anon** memory → verdict + leak projection |

Ground truth is the cgroup v2 accounting (`memory.current` + the `anon` line of
`memory.stat`), resolved from each container's init PID so it works under both
docker cgroup drivers; it falls back to `docker stats` when the cgroup files are
unreachable (e.g. Docker Desktop), recording the working set with anon blank.

## Key idea

`memory.current` is mostly **reclaimable page cache** — it looks alarming but is
not a leak. The non-reclaimable part is **anon**. Always read the trend on
`*_anon_gib`; `analyze.py` does this for you.

## Usage (Linux / WSL, where the container stack runs)

```bash
# start the background sampler (detached, survives the shell; 5-min cadence)
make mem-watch-start          # or: setsid nohup bash scripts/mem-watch/sampler.sh &

# later — on demand — read the trend
make mem-watch                # or: python3 scripts/mem-watch/analyze.py

# stop it
make mem-watch-stop
```

A slow leak (GiB/day) needs a few hours of samples to separate from the normal
indexing churn, so let it run a while before trusting the slope.

## Config (env overrides)

| Var | Default | Meaning |
|-----|---------|---------|
| `WQM_MEM_WATCH_DIR` | `$HOME/.wqm-mem-watch` | output dir (`samples.csv`, `sampler.pid`) |
| `WQM_MEM_WATCH_INTERVAL` | `300` | seconds between rows |
| `WQM_MEM_WATCH_CONTAINERS` | wqm services | space-separated container names |

Output lives **outside** the repo by default, so nothing is committed by mistake.
A single-instance lock (`sampler.pid`) prevents duplicate samplers.

## Remediation

If anon climbs toward restart territory, `docker compose restart memexd` drops it
back (recovers the working set, drains the queue) without data loss.
