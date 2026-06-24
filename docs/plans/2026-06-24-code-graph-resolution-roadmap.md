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
