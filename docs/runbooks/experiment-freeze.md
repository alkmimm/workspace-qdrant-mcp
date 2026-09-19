# Freezing the stack for a measurement

How to run workspace-qdrant-mcp as the **retrieval infrastructure of an
experiment** — a benchmark, an A/B, a thesis comparison of retrieval
strategies — without the conveniences of an interactive deployment turning
into confounds. Written from the 2026-09-16 validity audit; every item below
names the mechanism it neutralizes.

The interactive product is tuned to *degrade gracefully and stay helpful*:
optional stages fail open, notes an agent wrote resurface in its next search,
the catalog steers the agent toward the strongest tool. Each of those is a
variable a comparison must hold constant. None of them is a bug — which is
why they are knobs, not defaults.

## 1. What a condition is

A condition is **one `mcp` container with its own env**, all of them sharing
one daemon + one Qdrant + one index. The surface an agent sees is a property
of the container, not of the agent's prompt or of a client-side allowlist
(that only blocks *calls*; the agent still reads ~15k tokens of catalog and
an instruction to "call `search` FIRST").

| Condition | `WQM_MCP_TOOLS` | Instructions file says |
|---|---|---|
| A — native | (no MCP server configured) | — |
| B — lexical | `grep,list,retrieve` | grep for identifiers, list for layout, retrieve by path |
| C — hybrid/semantic | `search,grep,list,retrieve` | search for concepts, grep for identifiers |
| D — C + graph | `search,grep,list,retrieve,graph` | …plus graph for callers/impact |

Per container, in its env:

```dotenv
WQM_MCP_TOOLS=grep,list,retrieve                 # the surface
WQM_MCP_INSTRUCTIONS_FILE=/etc/wqm/instructions-B.txt   # a prompt that matches it
WQM_SEARCH_SCRATCHPAD_LANE=0                     # no cross-run memory in results
WQM_QDRANT_EXACT_SEARCH=1                        # ANN off (see §3)
```

Mount the instructions file into the container; an empty file sends no
instructions at all; a missing file is a startup error by design. `help`,
`rules`, `store`, `scratchpad` are deliberately absent from every row: the
built-in instructions tell agents to write rules and notes, and both persist
across sessions.

Run the containers on different ports from one compose project (copy the
`mcp` service with a suffix) or bring one up per condition sequentially —
the daemon/Qdrant side is untouched either way.

## 2. Record the identity of what ran

Nothing in the results is usable without this. `make verify-deploy` now fails
when the running server cannot name its commit (a bare `docker compose build`
produces `BUILD_SHA=unknown`; the Makefile passes it).

```bash
make verify-deploy                      # [OK] wqm-mcp built from commit <sha>
docker exec wqm-memexd memexd --version
docker image inspect -f '{{.Id}}' workspace-qdrant-mcp:local workspace-qdrant-mcp-memexd:local
```

Copy into the experiment package, verbatim:

- `docker/.env` (every `WQM_*` knob — the ranking constants live there:
  `WQM_SEARCH_RERANK(_WEIGHT)`, `WQM_QUERY_TRANSLATE`, `WQM_TRANSLATE_*`,
  `WQM_SEARCH_DERANK(_PENALTY)`, `WQM_KEYWORD_WEIGHT`, `WQM_SPARSE_ONLY_WEIGHT`,
  `WQM_PATH_BOOST_ALPHA`, `WQM_SYMBOL_MATCH_BOOST`,
  `WQM_IMPLEMENTATION_INTENT_*`, `WQM_GRAPH_*`).
- `state/memexd/config.yaml` (embedding model, `output_dim`, prefixes — these
  are config-file only and define the vectors).
- `state/memexd/global.wqmignore`, the repo's `.wqmignore`, and
  `assets/default_configuration.yaml` `exclude_directories` /
  `allowed_extensions` — together they decide what is *never* indexed (§4).
- `CHUNKER_LOGIC_VERSION` (`src/rust/daemon/core/src/tree_sitter/chunker/fingerprint.rs`),
  the Qdrant image tag, the client (agent) build.

## 3. Pin the pipeline, and read what it reports

Three stages fail open. Since this audit each search response carries a
`pipeline` block saying what actually ran:

```json
"pipeline": { "translation": "already-english", "rerank": "applied" }
```

| Field | Values | Pin it by |
|---|---|---|
| `translation` | `disabled` · `no-translator` · `already-english` · `translated` · `translation-failed` (timeout/HTTP) · `leg-skipped` (degraded embedding) | `WQM_QUERY_TRANSLATE=0` for a condition without it; otherwise **drop any run** whose responses show `translation-failed`/`leg-skipped` — the 7B CPU translator has an 8 s timeout and loses under load |
| `rerank` | `off` · `applied` · `failed` · `skipped` | `WQM_SEARCH_RERANK=0` or keep it and drop runs with `failed` |

Not reported (no proto field yet): **which embedding backend served the
query** — the daemon fails over GPU→CPU on the same model, and the two are
not bit-identical. Pin one: point `embedding.base_url` at a single backend
and leave `fallback_base_url` empty for the duration.

`WQM_QDRANT_EXACT_SEARCH=1` removes the last source of physical-state
dependence: HNSW is randomized per build and rebuilt by the optimizer, so the
same vector can return a different top-k after a segment merge.

## 4. Prove coverage; do not trust `indexing.percent`

`indexing: {percent: 100}` counts the queue of what the walk *saw*. On this
repo the walk saw 1 943 of 2 261 tracked files (a daemon-wide ignore rule
excluded `tests/language-support/`), and the response said 100%. For a target
repo, before the first run:

```bash
git -C <repo> ls-files | wc -l                                  # ground truth
# indexed view, current branch:
curl -s -H 'Content-Type: application/json' \
  -X POST localhost:6335/admin/api/tools/invoke \
  -d '{"tool":"list","args":{"format":"summary","cwd":"<repo>"}}' | jq .stats.files
```

Diff the two lists (`list format=flat` pages through the indexed paths).
Watch for `exclude_directories` matching a **path component** anywhere:
`bin`, `build`, `dist`, `out`, `coverage`, `vendor`, `deps`, `tmp` — a Rust
`src/bin/`, a Go `vendor/`, a package literally named `coverage` are silently
absent, and native grep (condition A) sees them. It happened (2026-09-19): the
global `out/` rule hid every hexagonal `adapters/out` / `ports/out` package of
a Java monorepo — 415 git-tracked sources, plus 16 Angular `state/` folders —
until an agent noticed `LorawanSender.java` in native grep but not in the
index. `global.wqmignore` now re-includes those names below any `src/`
directory (`!**/src/**/out/` and siblings); after editing the file, a memexd
restart runs `[ignore_sync]` and enqueues the newly eligible files as
`missing` (817 that day, drained in 10 minutes — recovery is a `stat` per
file since #399). Measure it before a run — `make coverage-audit` (`scripts/index-coverage-audit.py`)
compares `git ls-files` with `tracked_files` per project and sorts every
missing file into the gate that dropped it (`global`, `wqmignore`,
`gitignore`, `allowlist`, `size`); two buckets are defects and exit 1:
`ELIGIBLE` (the walk should have taken it) and `allowlist-drift`
(`default_configuration.yaml` promises an extension the compiled allowlist in
`allowed_extensions/extensions.rs` rejects — the daemon never reads the YAML
list; 500 files across the watched repos on 2026-09-19). The one-liner
version for a single rule:

```bash
# git-tracked files hidden by a plain 'name/' rule of global.wqmignore
git -C <repo> ls-files | awk -v r="$(grep -E '^[A-Za-z_.-]+/$' state/memexd/global.wqmignore | tr -d / | paste -sd'|')" \
  'BEGIN{n=split(r,R,"|")} {for(i=1;i<=n;i++) if (index("/"$0"/","/"R[i]"/")) {c[R[i]]++; break}} END{for(k in c) print k, c[k]}'
```

## 5. Freshness, tenant, branch

- **Freshness.** File changes reach the index after `debounce: 10s` + the
  queue; burst indexing is suppressed when `load_1m / cores > 0.6`. After a
  checkout or an agent edit, wait for `workspace_index indexing_status` to
  drain **and** probe a string you just wrote with `grep` before releasing the
  agent. Native tools see the disk immediately; this is a structural edge for
  condition A in edit→verify loops — measure the lag, report it.
- **Tenant.** `tenant_id = hash(remote_url)` for the first clone; later clones
  of the same remote get `hash(remote|disambiguation_path)`, which depends on
  the set of clones registered at the time. Node ids and point ids derive from
  it. One clone per repo per daemon; pass `projectId` explicitly; record it.
- **Detached HEAD** (`git checkout <sha>`) is stamped as branch `main` by the
  daemon and read without a branch filter. Fine for a fresh clone per task;
  for a shared clone alternating commits, content from the previous commit
  stays under `main` until reconciliation.
- **Branch churn.** Deleting a branch does not immediately drop its chunks
  (prune runs at startup, capped 500 files/cycle — issue #381). A run that
  creates and deletes a branch per task leaves the agent's own edits in the
  index, and `search`/`grep` auto-widen to all branches on an empty result.
  Do not churn branches; or check `/admin/api/branches/coverage` reports zero
  orphans before each block.
- **Worktrees** (`.claude/worktrees/*`) are indexed under the main checkout
  with branch membership and a history of defects (#325/#326/#334). Use plain
  clones.

## 6. Graph (condition D)

- `usages`/`impact` are deterministic since #371 (depth → confidence →
  node id). The node-id tiebreak is a content hash, so a truncated page is a
  *stable* sample, not the "most relevant" one — use `topK:0` or say so.
- `hotspots`/`bridges`/`modules`/`cycles` are deterministic since this audit
  (sorted sources, sorted adjacency, node-id tiebreaks).
- `bridges` and `modules` run under a 20 s wall-clock budget. A cut run now
  answers with `partial: true` and a leading `hint`; **discard those samples**
  or bound `bridges` with `maxSamples` ≤ the reported `sources_processed` so
  every run walks the same source set.
- `WQM_GRAPH_LSP_BACKFILL=1` rewrites CALLS edges asynchronously after
  indexing (confidence values move). Either set it to `0` for the experiment
  or wait for the backfill to finish before freezing.
- `usages` returns test callers too (no `excludeTests` on graph); the
  `REFERENCES` edge exists for Dart only. Language asymmetry is part of the
  treatment — state it.

## 7. Measure tokens at the client

`search_events.bytes_out` is the sum of *content bodies* (match lines, chunk
text) — no JSON envelope, hints or metadata — and `graph`/`rules`/`scratchpad`
/`help` op events carry **no bytes at all**. `bytes_in` is a counterfactual
("what a file read would have cost"), not a measurement. Use the client's
billed tokens (`claude -p --output-format json` → `usage`) as the response
variable, `bytes_out` at most as a covariate, and report the fixed per-session
cost of each condition separately: the advertised catalog
(`tools/list`, ~53 KB for the full 12 tools), the instructions, and any
`rules list` the prompt asks for.

## 7b. Host memory (WSL2) — the stack must never take the host down

On 2026-09-16 the WSL2 VM sat at its `memory=96GB` ceiling for hours with
50–61 GiB of **page cache** (processes never exceeded 17 GiB); the Windows
host ran out, failed to page its own files (`InPageError`), froze the VM and
rebooted — every other workload in the VM went down with this one. The cache
came from `make redeploy` (two ~10 GiB database copies), container builds
and the daemon's indexing I/O, and `autoMemoryReclaim=gradual` did not hand
it back. Per-container `mem_limit` is deliberately not the tool (see the note
on `memexd` in `docker-compose.yml`); these are:

- `make preflight` — refuses to start when the Windows host has < 24 GB free,
  the host is charged > 60 GB for the VM (the `vmmemWSL` working set — guest
  page cache included until the guest idles long enough for
  `autoMemoryReclaim` to return it; measured ≈ guest `MemTotal−MemFree` + 3–7
  GB), or the guest's PSI `full avg60` is > 10 %. Without a host view it falls
  back to the guest footprint (> 60 GB) and page cache (> 40 GB). Swap in use
  is printed but never fails the check: `autoMemoryReclaim` swaps idle pages
  out on purpose (8.8 GB after an idle night at PSI 0.06 %). All thresholds
  are env-tunable; `PREFLIGHT_SOFT=1` turns every refusal into a warning (a
  `make validate` leaves ~50 GB of reclaimable cargo cache that only the
  operator can recognise as residue). `stack-up` and `redeploy` run it first.
- `make stack-guard` — background watchdog; stops **this** compose project
  (never the others) when the host charge for the VM passes
  `VM_GUARD_STOP_GB` (70; guest footprint when the host cannot be asked) or
  host free memory drops below `HOST_GUARD_MIN_FREE_GB` (16), for 2 polls in
  a row. Log: `.wqm-fork/logs/memory-guard.log`. `make stack-guard-stop` ends
  it. Restart it after editing the scripts — it keeps the old code in memory.
- `redeploy` no longer streams the databases through the cache unless it
  must: the snapshot copy drops its pages as it goes (#393) and the migration
  rehearsal first probes the new binary's schema targets on an empty
  directory (~4 s) and skips the ~10 GiB copy when every live store is
  already at target (`SKIP: memexd 49/49, search 10/10, graph 6/6`). What
  remains is the image build; prefer `stack-up` when nothing changed.
- The daemon itself is the largest cache churner and is where the durable
  fixes go: a restart no longer re-reads every tracked file (mtime fast path,
  #397 — recovery, progressive-scan re-add and branch dedup all prove a file
  unchanged by mtime before hashing it), and the GPU embedder's allocator is
  capped so a full VRAM never spills into host memory
  (`PYTORCH_CUDA_ALLOC_CONF`). Watch the recovery log line
  `unchanged (N by mtime, no read)` after a restart.
- Trim `COMPOSE_PROFILES`: `embeddings-cpu` (warm standby, 8 GiB cap) is
  unnecessary once the experiment pins one embedding backend.
- `~/.wslconfig` stays at `memory=96GB`: the other workloads in this VM need
  it, so lowering the cap is not the fix. `autoMemoryReclaim=dropcache`
  (returns cache as soon as the guest idles instead of gradually) is the one
  optional host-side tweak; it needs `wsl --shutdown`, so schedule it when
  nothing else runs in the VM. Neither mode reclaims while the guest CPU is
  busy — a long indexing drain or build pins the cache until it ends, which
  is why the guard exists.

## 8. Checklist

- [ ] `make verify-deploy` green, commit recorded for mcp + memexd images
- [ ] one `mcp` container per condition, `WQM_MCP_TOOLS` + instructions file set, catalog size measured per condition
- [ ] `WQM_SEARCH_SCRATCHPAD_LANE=0`; `scratchpad`/`rules` collections emptied or a throwaway tenant
- [ ] `WQM_QDRANT_EXACT_SEARCH=1`; translation and rerank pinned on or off; embedding fallback disabled
- [ ] coverage diff (git ls-files × indexed) archived per target repo
- [ ] one clone per repo, `projectId` recorded, no worktrees, no branch churn, zero orphans
- [ ] index drained + marker probe before every run; lag recorded
- [ ] graph: `partial:true` samples excluded; LSP backfill settled or off
- [ ] every response's `pipeline` block archived with the run
