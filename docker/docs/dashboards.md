# Grafana dashboards

Pre-built dashboards are provisioned automatically when Grafana starts with the
config in `docker/grafana/provisioning/`. The provider loads every `*.json` in
`docker/grafana/dashboards/`, so a new dashboard is picked up just by dropping the
file there. All dashboards auto-refresh every 30 seconds.

Import JSON files from `docker/grafana/dashboards/` if auto-provisioning is not
in use (Dashboards -> Import in the Grafana UI).

## Dashboard list

| File | UID | Title | Focus |
|---|---|---|---|
| `system-overview.json` | `wqm-system` | WQM — System Overview | Service health, queue freshness, MCP activity, indexed project inventory, error rates |
| `memexd.json` | `wqm-memexd` | WQM — memexd Daemon | Target health, uptime, stale queue items, oldest pending age |
| `mcp-server.json` | `wqm-mcp-server` | WQM — MCP Server | Tool rates, durations, daemon fallbacks, recent tool logs, cache ratio |
| `mcp-http.json` | `wqm-mcp-http` | WQM — MCP HTTP Transport | Request mix, auth failures, rate limits |
| `token-economy.json` | `wqm-token-economy` | WQM — Token Economy | Response-byte savings ratio, followup/escalation friction, shape-mode (packed/truncate) adoption |
| `qdrant.json` | `wqm-qdrant` | WQM — Qdrant | Collections, target health, resource saturation, REST/gRPC throughput and latency |

(The stack also ships `graph.json`, `lsp.json`, `chunking-coverage.json`,
`fts5-pressure.json`, `project-language.json`, `system-overview` links and
`traces.json`, which follow the same provisioning path.)

Navigation links in the System Overview header jump directly to the other four
dashboards.

---

## WQM — System Overview (`system-overview.json`)

Start here. It gives a health snapshot across the stack and surfaces the most
actionable queue and MCP signals.

### Panel inventory

**Row 1 - Service health and queue state**

| Panel | Metric | Interpretation |
|---|---|---|
| memexd Target Health | `up{job="memexd"}` | GREEN = UP / RED = DOWN. Uses an instant query so stale series do not duplicate the current state. |
| MCP Target Health | `up{job="mcp"}` | GREEN = UP / RED = DOWN. Uses an instant query so stale series do not duplicate the current state. |
| Qdrant Target Health | `up{job="qdrant"}` | GREEN = UP / RED = DOWN. Uses an instant query so stale series do not duplicate the current state. |
| Oldest Pending Queue Item Age | `wqm_queue_oldest_pending_age_seconds` | High values indicate a queue stall. |
| Stale Queue Items | `sum(memexd_unified_queue_stale_items)` | Non-zero means recovered leases are lagging behind. |
| Active MCP Sessions | `wqm_mcp_session_count` | Connected MCP sessions currently active. |

**Row 2 - Activity and queue freshness**

| Panel | Query summary | Interpretation |
|---|---|---|
| MCP Tool Invocation Rate | `sum(rate(wqm_mcp_tool_invocations_total[2m]))` | Overall call rate across all tools. |
| Queue Health Trend | `wqm_queue_oldest_pending_age_seconds` + `sum(memexd_unified_queue_stale_items)` | Lets you see whether queue age and stale items are trending up together. |

**Row 3 - Recent errors**

| Panel | Query summary | Interpretation |
|---|---|---|
| Recent Error Rates | `wqm_mcp_tool_invocations_total{status="error"}` + `wqm_mcp_daemon_fallback_total` + `wqm_mcp_http_auth_failures_total` + `wqm_mcp_http_rate_limited_total` | Highlights tool failures, daemon reachability issues, auth failures, and rate limiting. |

**Row 4 - Indexed projects**

| Panel | Query summary | Interpretation |
|---|---|---|
| Indexed Projects | `memexd_indexed_project_tracked_files` + `memexd_indexed_project_points` + `memexd_indexed_project_last_scan_seconds` + `memexd_indexed_project_last_activity_seconds` | Live per-project inventory exported from the daemon's `watch_folders` table, with document counts, point counts, status flags, and scan/activity timestamps for the table view. |

---

## WQM — memexd Daemon (`memexd.json`)

The daemon dashboard is intentionally lean in this fork. The current exported
surface is target health, uptime and queue freshness.

### Panel inventory

| Panel | Metric | Interpretation |
|---|---|---|
| memexd Target Health | `up{job="memexd"}` | Prometheus scrape health for the memexd job. |
| Daemon Uptime | `max(memexd_uptime_seconds)` | Low values mean the daemon restarted recently. |
| Stale Queue Items | `sum(memexd_unified_queue_stale_items)` | Non-zero means queue recovery is behind. |
| Oldest Pending Queue Item Age | `wqm_queue_oldest_pending_age_seconds` | High values indicate backlog or a stuck processor. |
| Stale Queue Items Trend | `sum(memexd_unified_queue_stale_items)` | Trend line for lease recovery lag. |
| Oldest Pending Age Trend | `wqm_queue_oldest_pending_age_seconds` | Trend line for queue stall detection. |

---

## WQM — MCP Server (`mcp-server.json`)

This is the server-level MCP dashboard. It tracks tool usage, latency, daemon
fallbacks, cache ratios and the recent structured tool-call logs shipped to
Loki from `mcp-server.jsonl`.

### Panel inventory

**Row 1 - Session and call rates**

| Panel | Metric | Interpretation |
|---|---|---|
| Active MCP Sessions | `wqm_mcp_session_count` | Open MCP sessions. |
| Tool Invocation Rate | `sum by (tool) (rate(wqm_mcp_tool_invocations_total[2m]))` | Per-tool call rate. |
| Tool Error Rate | `sum by (tool) (rate(wqm_mcp_tool_invocations_total{status="error"}[2m]))` | Errors per tool. |

**Row 2 - Latency**

| Panel | Query | Interpretation |
|---|---|---|
| Tool P99 Duration | `histogram_quantile(0.99, sum by (tool, le) (rate(wqm_mcp_tool_duration_seconds_bucket[5m])))` | 99th percentile latency per tool. |
| Tool Duration Heatmap | `p50`, `p95`, `p99` over `wqm_mcp_tool_duration_seconds_bucket` | Compares latency spread across tools. |

**Row 3 - Connectivity and cache**

| Panel | Metric | Interpretation |
|---|---|---|
| Daemon Fallback Rate | `sum by (tool, reason) (rate(wqm_mcp_daemon_fallback_total[2m]))` | Non-zero means the MCP server could not reach memexd. |
| Cache Hit Ratio | `wqm_mcp_cache_hits_total` / `wqm_mcp_cache_hits_total + wqm_mcp_cache_misses_total` | Present for future cache work; should remain zero until a cache exists. |

**Row 4 - Cumulative breakdown**

| Panel | Query | Interpretation |
|---|---|---|
| Tool Success / Error Breakdown | `sum by (tool, status) (increase(wqm_mcp_tool_invocations_total[$__range]))` | Absolute count totals for the selected window. |

**Row 5 - Recent tool calls**

| Panel | Query | Interpretation |
|---|---|---|
| Recent MCP Tool Calls | `{job="mcp-logs", container="wqm-mcp"} | json | msg="Tool called"` | Loki log view of the structured tool-call entries written by the MCP server. This is the best place to inspect the latest invocations, durations and success/failure state. |

---

## WQM — MCP HTTP Transport (`mcp-http.json`)

HTTP mode adds a transport-level view of the MCP server.

### Panel inventory

| Panel | Metric | Interpretation |
|---|---|---|
| Request rate by path | `sum by (path) (rate(wqm_mcp_http_requests_total[5m]))` | `/mcp`, `/healthz` and other paths. |
| Request rate by status class | `sum by (status_class) (rate(wqm_mcp_http_requests_total[5m]))` | 2xx / 4xx / 5xx split. |
| Auth failures by reason | `sum by (reason) (rate(wqm_mcp_http_auth_failures_total[5m]))` | Token problems and other auth failures. |
| Rate-limit hits | `rate(wqm_mcp_http_rate_limited_total[5m])` | Requests throttled by the IP limiter. |
| Total requests | `sum(increase(wqm_mcp_http_requests_total[24h]))` | Request volume over the selected window. |
| Auth failure share | `sum(rate(wqm_mcp_http_auth_failures_total[5m])) / sum(rate(wqm_mcp_http_requests_total[5m]))` | Useful when tokens are misconfigured. |
| 5xx share | `sum(rate(wqm_mcp_http_requests_total{status_class="5xx"}[5m])) / sum(rate(wqm_mcp_http_requests_total[5m]))` | Server-side HTTP failures. |
| Healthz share | `sum(rate(wqm_mcp_http_requests_total{path="/healthz"}[5m])) / sum(rate(wqm_mcp_http_requests_total[5m]))` | Helps distinguish probes from actual tool traffic. |

---

## WQM — Token Economy (`token-economy.json`)

Surfaces the spec-20 token-economy signals that previously lived only in the
`memexd` SQLite `token_savings` view (reachable via `wqm admin token-savings`
and the `benchmarks/agent-economy` harness). A background sampler in the daemon
(`start_token_economy_sampler`, 60s) aggregates the view over a rolling 24h
window into Prometheus gauges, joining back to `search_events` to recover the
`actor` label the view drops.

**All series are 24h snapshot gauges — plot them directly, never wrap them in
`rate()`.** Panels default to agent traffic (`actor="claude"`); the
eval/benchmark harness and CLI are `actor="user"` and are excluded from the
ratio panels so a benchmark run cannot skew the savings number.

### Panel inventory

| Panel | Query summary | Interpretation |
|---|---|---|
| Savings Ratio (24h, agents) | `1 - sum(bytes_out) / sum(bytes_in)` | Headline number. Higher is better; green ≥ 0.7. |
| Bytes Saved / ≈ Tokens Saved | `sum(bytes_in) - sum(bytes_out)` (and `/4`) | Absolute saving; the `/4` matches the CLI's tokenizer-independent proxy. |
| Followup Rate | `sum(memexd_mcp_token_followup_recent) / sum(memexd_mcp_token_calls_recent)` | LOWER is better — a saving that forces a re-query is not a saving (spec 20 §1.2). |
| Escalation Rate | `sum(memexd_mcp_token_escalation_recent) / sum(memexd_mcp_token_calls_recent)` | Truncated results the agent had to expand via retrieve. |
| Savings Ratio by Tool | `1 - (memexd_mcp_token_bytes_out_recent / clamp_min(memexd_mcp_token_bytes_in_recent, 1))` | A tool trending to 0 ships its whole payload; negative = shaping overhead grew the response. |
| Response Bytes In vs Out | `sum(memexd_mcp_token_bytes_in_recent)` vs `..._bytes_out_recent` | The gap is the saving. |
| Followup & Escalation by Tool | per-tool followup/escalation over calls | High savings + high followup on the same tool = over-shaping. |
| Shape-mode Adoption | `sum by (shape_mode) (memexd_mcp_token_calls_by_shape_recent)` | How much traffic goes through `packed` / `truncate` vs `none`. |
| Token-tracked Calls by Tool | `memexd_mcp_token_calls_recent{actor="claude"}` | Volume/denominator for the rates. |
| Calls by Tool & Actor | `memexd_mcp_token_calls_recent` | All actors — separates real agent traffic from the harness. |

---

## WQM — Qdrant (`qdrant.json`)

The Qdrant dashboard uses the native Qdrant metrics exposed by the container.
It now shows both a collection catalog and a live inventory, so the
collection names are easy to read while the counts stay dynamic.

### Panel inventory

**Row 1 - Database state**

| Panel | Metric | Interpretation |
|---|---|---|
| Collections | `collections_total` | Number of Qdrant collections. |
| Total Vectors | `collections_vector_total` | Total vectors across all collections. |
| Total Points | `sum(collection_points)` | Approximate total point count. |
| Qdrant Target Health | `up{job="qdrant"}` | Prometheus scrape health, shown with an instant query so the current target does not appear duplicated. |
| Resident Memory | `memory_resident_bytes` | Working set size. |
| Open FD Saturation | `process_open_fds / process_max_fds` | File descriptor pressure. |

**Row 2 - Collection catalog**

| Panel | Content | Interpretation |
|---|---|---|
| Collection Catalog | Markdown table with the canonical collection names (`projects`, `libraries`, `rules`, `scratchpad`, `images`). | Static legend for the collections that hold project data, docs, rules, scratch notes and images. It also notes that project/branch/worktree registry data lives in `watch_folders` and `.wqm-fork/indexed-projects.json`. |

**Row 3 - Collection inventory**

| Panel | Query | Interpretation |
|---|---|---|
| Collection Inventory | `label_replace(sum by (id) (collection_points), "collection", "$1", "id", "(.+)")`, `sum by (collection) (collection_vectors)`, `label_replace(sum by (id) (collection_running_optimizations), "collection", "$1", "id", "(.+)")`, `label_replace(sum by (id) (collection_indexed_only_excluded_points), "collection", "$1", "id", "(.+)")` | Table of collections with points, vectors, running optimisations and excluded points, sorted by Points descending. |

**Row 4 - REST traffic**

| Panel | Query | Interpretation |
|---|---|---|
| REST Request Rate | `sum by (endpoint) (rate(rest_responses_total[2m]))` | HTTP throughput per endpoint. |
| REST P99 Latency | `histogram_quantile(0.99, sum by (endpoint, le) (rate(rest_responses_duration_seconds_bucket[5m])))` | 99th percentile REST latency. |

**Row 5 - gRPC traffic**

| Panel | Query | Interpretation |
|---|---|---|
| gRPC Request Rate | `sum by (endpoint) (rate(grpc_responses_total[2m]))` | gRPC throughput per endpoint. |
| gRPC P99 Latency | `histogram_quantile(0.99, sum by (endpoint, le) (rate(grpc_responses_duration_seconds_bucket[5m])))` | 99th percentile gRPC latency. |

**Row 6 - Compaction**

| Panel | Metric | Interpretation |
|---|---|---|
| Running Optimisations | `sum by (id) (collection_running_optimizations)` | Ongoing optimisation tasks per collection. |

---

## Navigation tips

- Use the time range picker to zoom into incident windows.
- In table panels, click a column header to sort.
- In time series panels, click a legend entry to isolate that series.

## Notes

- The server dashboard now uses `mcp-server.json`.
- The memexd dashboard now reflects the lean telemetry surface exposed by the current daemon build.

_workspace-qdrant-mcp v0.1.3 - documentation updated 2026-05-24_
