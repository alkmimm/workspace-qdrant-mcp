# Code-Graph Resolution & Centrality Roadmap

**Date:** 2026-06-24
**Source:** 13-agent SOTA research workflow (community survey + adversarial verification + source-grounded synthesis).
**Scope:** the compiler-free tree-sitter code graph (`graph_nodes`/`graph_edges`, `resolve_stub_edges`, centrality algorithms).

## 0. The single root cause (verified in source)

All four observed symptoms trace to **one closure**: `pick` in
`src/rust/daemon/core/src/graph/sqlite_store.rs:450-467` (inside `resolve_stub_edges`).
For each name-only CALLS/CONTAINS/USES_TYPE stub it resolves the target only by:

1. a definition **in the caller's own file**, else
2. a **globally-unique tenant-wide** name (`match pool.as_slice() { [(nid,_)] => Some, _ => None }`).

Every **ambiguous** name (>1 candidate) returns `None` → the loop `continue`s → the
edge stays anchored to an empty-file_path stub. There is a test
(`test_resolve_stub_edges_skips_ambiguous`) asserting this is intended.

In a Java/Kotlin monorepo the high-value callees (`build`, `of`, `toString`, `getId`,
`save`, domain verbs) are **precisely** the same-named-across-files bucket. So:

- **impact/usages recall ~0:** reverse reachability has almost nothing to walk — the
  ~53k CALLS edges that "exist" point at empty-file_path stubs.
- **centrality noise:** the *only* names unique enough to resolve to a file-backed node
  are the generic library/util/generated ones (`newBuilder`, `toUpperCase`, `replace`,
  `stream`). They survive resolution, keep a `file_path`, and dominate PageRank.
  **Centrality noise is the flip side of the resolver bug, not an independent problem.**

**Sequencing rule:** do NOT raise impact traversal depth — low recall is a *binding*
deficit, not a depth limit. Fix binding first, then re-measure.

## What the codebase already has (don't rebuild)

| Capability | Location | State |
|---|---|---|
| Empty-file_path stub drop from centrality | `algorithms/mod.rs` (stub skip) | ✅ done |
| Centrality exclusion env var (file-path substrings) | `algorithms/mod.rs` `WQM_GRAPH_CENTRALITY_EXCLUDE` | ✅ but cannot demote a generic *symbol name* in a legit file |
| Shared adjacency loader | `algorithms/mod.rs` `load_adjacency_graph` | ✅ one change is uniform |
| IMPORTS edges | `extractor/import_parsers.rs` | ⚠️ produced, never consulted by `pick`; **package/FQN discarded** |
| CONTAINS edges (class membership) | graph schema | ✅ usable for scope ancestry |
| LSP call-hierarchy pass | `lsp/.../call_hierarchy.rs` | ✅ exists; empty-on-cold |
| Edge `weight` + `metadata_json` columns | `schema.rs` `weight REAL DEFAULT 1.0` | ⚠️ `weight` hardwired 1.0 — **reusable for confidence, NO migration needed** |

**Two constraints (verified):**
- **Call-site multiplicity is destroyed at ingestion.** `compute_edge_id =
  SHA256(source|target|edge_type)` (no call-site) + `INSERT OR IGNORE` → A→B is **one**
  edge. ⇒ weight PageRank by **in-degree** (distinct callers), NOT call frequency.
- **USES_TYPE is a signature heuristic, not resolved receiver types.** Receiver-type
  disambiguation is new work, not reuse.

## Ranked roadmap (impact-per-effort)

| # | Change | Tier | Effort | Fixes | Escalation |
|---|---|---|---|---|---|
| **R1** ⭐ | **Stop dropping ambiguous edges → keep-all-candidates + confidence weight** (`weight=1/N`; impact/usages traverse at a floor, centrality strict) | 1 | M | impact/usages (the 53k) | no |
| **R10** | Confidence in `weight` + `metadata_json.resolution`; per-language "% CALLS resolved to non-stub" eval gate | x | S | measurement | no |
| **R3** | Centrality: **symbol-name** stoplist + IDF sort (`pagerank × log(total/count(name))`) + GENERATED/TEST/VENDORED tags | 1 | S | hotspots noise | no |
| **R2** | `pick` scope-aware via CONTAINS ancestry (call in class C prefers C.m) | 1 | M | precision | no |
| **R6** | OVERRIDES/IMPLEMENTS edges (CHA-lite) + reverse impact traversal | 2 | M | polymorphism | no |
| **R5** | Arity-bucketed overload fan-out | 2 | S–M | overloads | no |
| **R8** | Wire warm-LSP to stamp/override authoritative edges + sample ground truth | 3 | S | type residue | LSP |
| **R4** | Import-anchored resolution — **needs retaining package/FQN extraction** | 2 | M–L | cross-pkg collisions only | no |
| **R7/R9** | Receiver-type-flow-lite / stack-graphs model (evaluate, Java/Kotlin, flag) | 3 | L/XL | precision / end-state | partial |

**Start: R1 + R10 + R3** — convert dropped CALLS into ranked traversable edges, get the
metric to prove it, de-noise hotspots. All tree-sitter-only, no new extraction.

## How the community does it (grounding)

- **aider repo-map** (near-identical data model): never drops ambiguous — links to all
  name-matched candidates, weights by mention count, personalized PageRank; ubiquitous
  names (`toString`/`build`) are **demoted** because their edges spread thin.
- **GitHub stack-graphs + tree-sitter-graph** (arXiv 2211.01224): compiler-free,
  file-incremental cross-file resolution via push/pop symbol-stack over IMPORTS. The
  principled end-state — **borrow the model (precedence-weighted multi-candidate), not
  the per-language `.tsg` engine**.
- **Joern/CPG**: compiler-free call graph that **collects candidates** vs drops.
- **SCIP/Sourcegraph**: "precise vs search-based, **never return zero**" degradation.
- **Theoretical ceiling (Visser 2015, type-dependent name resolution):** overload-by-type
  and virtual-dispatch→concrete-impl are **unsolvable from a CST** → that residue is the
  LSP/SCIP tier. Don't promise CHA-grade recall from an untyped CST.

## Downgraded / dropped by the adversarial pass (do not cite)

1. "Weighted PageRank by call frequency is a cheap loader fix" → **FALSE here**
   (`compute_edge_id` has no call-site; `INSERT OR IGNORE`). Use **in-degree**, not
   call-frequency.
2. "Import-scoping is THE highest-leverage, no-new-extraction fix" → **overstated 2×**:
   needs FQN retention (discarded both sides) AND only resolves cross-package collisions.
   **R1 (keep-all), not import-scoping, restores the 53k.**
3. Fabricated stats — `34%→76%`, ACER's six confidence constants, `>90% Sourcegraph` —
   **not in sources; do not cite.** The qualitative levers are real; the numbers are not.

## Status updates

- **2026-07-01 — R2.5 proximity precedence shipped (CALLS fan-out dedup).** The
  R1 keep-all-candidates tier is correct for recall but inflates the physical
  CALLS edge count ~8× on DOC-V2: every ambiguous callee (`save`, `build`, `of`,
  `getId`, domain verbs) fans out to N file-backed candidates at weight `1/N`.
  That inflation is invisible to centrality (already gated at `weight >= 0.6`, so
  `1/N ≤ 0.5` is excluded) but bloats `graph stats` and dilutes impact/usages
  precision. Added a tier BETWEEN tenant-unique and keep-all in `pick_all`
  (`sqlite_store.rs`): among the ambiguous pool, keep only the DEEPEST
  shared-directory-prefix bucket vs the caller's file — the intra-package
  definition is overwhelmingly the true callee of an unqualified same-name call.
  **Only a UNIQUE deepest-prefix candidate collapses** → one edge at `0.85`
  (enters centrality as a real edge, like tenant-unique `0.7`); everything else
  (non-unique deepest bucket, or all candidates at the same shallow prefix)
  falls through to the unchanged keep-all `1/N`. Repo-root files share depth 0 so
  the guard `max_depth >= 1` leaves the no-signal case exactly as before (the
  existing `test_resolve_stub_edges_keeps_ambiguous` is untouched). Uses only the
  `file_path` already on both nodes — no new extraction, no reembed; a graph
  re-extraction applies it to the existing corpus, fresh ingest picks it up
  automatically. This is a partial, path-proximity proxy for R4 (full import/FQN
  anchoring remains the cross-package end-state).

  **Post-review narrowing (10-finder review, same day).** The first cut also had
  a `1/k` "narrower same-package overload set" branch and applied to every stub
  type. Two changes landed from the review: (1) the `1/k` branch was **removed** —
  collapsing only on a *unique* deepest match, so a non-unique bucket never drops
  a cross-package candidate (preserves R1 recall on a non-decisive signal); (2)
  proximity is now **CALLS/USES_TYPE only** (`!container_only`) — a CONTAINS parent
  is structurally 1:1 and must not be guessed, so Pass 2 keeps its own-file /
  tenant-unique contract (a proximity `0.85` would otherwise have cleared Pass 2's
  `>= 0.7` gate and grafted a member onto the wrong same-named container).
  Regression tests in `graph/shared.rs`:
  `test_resolve_stub_edges_proximity_collapses_fanout` (unique → 0.85, far pruned),
  `_keeps_same_package_overloads` + `_keeps_all_when_bucket_not_unique` (keep-all
  fall-through, weight `< 0.6`), `_not_applied_to_contains` (Pass 2 gate).

- **2026-06-25 — R3 second axis (use-ubiquity) shipped.** R3's first cut (#164,
  deployed) demotes by **definition** count, which is structurally blind to the
  dominant live noise: a name **defined once** (`def_count == 1`) whose bare name
  collides with a stdlib builtin (`collect`, `iter`, `Result`, `send`, `bind`,
  `insert`, `read`…). The by-name stub resolver repoints every same-named stdlib
  call onto that single node via the tenant-unique tier (weight 0.7), so it enters
  centrality with implausible in-degree (measured live on `367157a01d98`:
  `Result`=1044, `iter`=813, `collect`=761, `bind`=501, `insert`=394 — all
  `def_count == 1`). Added a **use-ubiquity axis** to `load_adjacency_graph`: drop a
  NODE whose high-confidence (weight ≥ 0.6) in-degree exceeds a corpus-derived
  threshold `max(50, total/150)`, env-tunable via
  `WQM_GRAPH_CENTRALITY_USAGE_THRESHOLD` (0 disables). Centrality-only (search /
  grep / relations / impact / usages unaffected — they bypass this loader). The
  live in-degree distribution separates cleanly: real domain fns (`enqueue_unified`
  =111, `search_exact`=81) sit below the noise floor. Expected to also unglue the
  giant catch-all `modules` community (these hubs were its glue).

## Key files

- Resolver root cause: `src/rust/daemon/core/src/graph/sqlite_store.rs` (`pick` L450-467;
  Pass-1/Pass-2 L473-566)
- Centrality loader (shared): `src/rust/daemon/core/src/graph/algorithms/mod.rs`
- Edge-id / dedup (multiplicity-destroying): `src/rust/daemon/core/src/graph/mod.rs`
  (`compute_edge_id`)
- Import parsers (qualifier discarded): `src/rust/daemon/core/src/graph/extractor/import_parsers.rs`
- Call/type extractor: `src/rust/daemon/core/src/graph/extractor/{mod,type_analysis}.rs`
- PageRank: `src/rust/daemon/core/src/graph/algorithms/pagerank.rs`
- LSP rung: `src/rust/daemon/core/src/lsp/project_manager/call_hierarchy.rs`
- Schema (weight column): `src/rust/daemon/core/src/graph/schema.rs`
- Per-language registry (for `centrality_skip_names`): `language_registry.yaml`
