# Agent token-economy harness

Experimental harness measuring **cost per completed task** for an agent
working on this repository, with and without the workspace-qdrant MCP
server. It joins two measurement planes per run:

| Plane | Source | What it gives |
|---|---|---|
| Client (ground truth) | `claude -p --output-format json` result | Real billed tokens (input / output / cache read / cache creation), cost USD, turns, duration, session id |
| Server | daemon `search_events` rows in the run's time window | MCP calls by op, `bytes_in`/`bytes_out` (spec 20), truncated hits, followup/escalation linkage (schema v44+) |

Runs are strictly sequential, so a `[t_start−1s, t_end+1s]` window
attributes server events unambiguously on a single-user deployment; with
schema v44+ every event also carries the MCP HTTP session id for a strict
per-session join.

## Conditions

Only the tool surface / instructions vary — same tasks, same repo, same model:

| Condition | Tools | Notes |
|---|---|---|
| `native` | `Read,Grep,Glob` | no MCP server (`--strict-mcp-config` with an empty registry) |
| `mcp` | native + `search`/`grep`/`retrieve`/`list`/`graph` | server defaults (`concise` shaping) |
| `mcp-packed` | same as `mcp` | system-prompt instruction to request `responseFormat:"packed"` |

## Tasks

`tasks.json` — read-only localization / comprehension / impact questions
about this repo with ground-truthed answers (`expected_paths` substrings the
final answer must mention). Tasks are stable on `main` and mutate nothing,
so runs are repeatable without resets.

## Running

```bash
# from the repo root, inside WSL (needs: claude CLI authenticated,
# MCP_HTTP_TOKEN in the environment, the wqm stack up)
python3 benchmarks/agent-economy/run_harness.py --dry-run       # sanity
python3 benchmarks/agent-economy/run_harness.py --only loc-shaping --conditions native,mcp
python3 benchmarks/agent-economy/run_harness.py --repeat 3      # full grid

python3 benchmarks/agent-economy/analyze.py benchmarks/agent-economy/results/runs.jsonl --csv results/runs.csv
```

Knobs: `--model` (CLI model override), `--max-turns` (default 20),
`--timeout` (per run, default 600s), `WQM_MCP_URL` / `WQM_MEMEXD_CONTAINER`
env overrides.

## Metrics and hygiene

- **Headline metric**: `cost_per_completed_usd` = total spend ÷ successful
  runs — a cheap condition that fails is not cheap.
- Cache tokens are reported separately (`cache_read` is ~10× cheaper than
  raw input); `cost_usd` from the CLI already prices them correctly.
- Success is a substring check of `expected_paths` against the final
  answer — deliberately coarse; treat near-misses by inspecting
  `answer_excerpt` in `runs.jsonl`.
- Before a measurement campaign: freeze the deployment (`tool_version`),
  check index health (`make reindex-status`, branch-drift probe), and note
  the model id. Record them alongside the results.
- `results/` is git-ignored: runs are deployment- and price-specific data,
  not repo content.
