# R8 — LSP-Authoritative CALLS Edges (hybrid: precise where available, fuzzy fallback)

**Date:** 2026-07-01
**Why:** the [community/SOTA survey](2026-07-01-code-graph-resolution-community-survey.md)
concluded that method-call homonym ambiguity (`build`×300 in DOC-V2) is
**structurally unsolvable by a by-name/CST fuzzy resolver** and requires
type-awareness. The industry answer is a **hybrid** (Sourcegraph's model): precise
where a type-aware source exists, fuzzy fallback elsewhere. We already have the
LSP foundation and a Dart language server in the image — this plan wires it into
an authoritative edge source.

## Current state (grounded in code)

- `lsp/project_manager/call_hierarchy.rs` — **built + tested**:
  `resolved_outgoing_calls(file,line,col)` (prepareCallHierarchy +
  outgoingCalls) returns the REAL callee `{name, file, line}`; `resolved_call_edges`
  builds a CALLS edge to the callee's REAL `node_id` (not a stub), skipping
  out-of-project callees.
- `strategies/processing/file/graph_ingest.rs::resolve_calls_via_lsp` — **wired**
  into `ingest_graph_edges`, but with two gaps below.
- Prereq: the image bundles the **Dart SDK** (`dart language-server --lsp`,
  v3.7.2) — Dart/Flutter LSP is available.

## Gaps (why it doesn't help DOC-V2 today)

1. **Additive, not authoritative.** The LSP edge is ADDED next to the tree-sitter
   stub; `resolve_stub_edges` still fans the stub out to ~300 candidates, drowning
   the precise edge. The LSP edge must SUPPRESS the fuzzy stub it supersedes.
2. **Cold at ingestion.** `resolve_calls_via_lsp` is gated on
   `is_server_ready_for_file`, but the LSP is not warm during a cold ingest/reembed
   (the file's own comment says so), so on DOC-V2's reembed it was a **no-op**.
   Needs a **warm backfill pass** that runs after the LSP has indexed.
3. **Activation.** LSP starts only via gRPC project activation; the uplift idle
   pass never re-runs it. Dart LSP for DOC-V2 must be activated + warmed.

## Increments

- **R8.1 — Authoritative suppression (THIS increment).** When the LSP resolves a
  caller's outgoing calls, drop the tree-sitter fuzzy stub CALLS edges from that
  caller for the resolved callee NAMES (keep fuzzy stubs for names the LSP could
  not resolve — stdlib/unresolved — as the fallback). Precise-where-available,
  fuzzy-fallback at per-call granularity. Small, pure, unit-testable. Takes effect
  wherever the LSP is warm at ingest (incremental edits post-warmup); it is the
  correctness foundation R8.2 builds on.
- **R8.2 — Warm backfill idle pass.** A background pass that, once the LSP is
  indexed for a project, re-resolves calls over already-ingested files and stamps
  authoritative edges (delete this file's fuzzy CALLS stubs it now resolves, add
  the precise edges), without a reembed. This is what delivers the DOC-V2 win.
- **R8.3 — Activation for target projects.** Ensure the Dart LSP is activated +
  warmed for DOC-V2 (and typed languages generally); avoid the dormant-LSP trap.
- **R8.4 — Measure.** Hand-label a set of ambiguous DOC-V2 calls; measure
  precision/recall of fuzzy vs import-anchored vs R8-LSP, so the
  keep-heuristics-vs-adopt-indexer call rests on our numbers (survey recommendation #4).

## Verification findings (2026-07-01 — probe of the live daemon)

Before building the backfill loop we probed the running daemon's LSP metrics/logs
(survey rec #4: measure before investing). **Decisive — and it reshapes R8.2/R8.3:**

- `memexd_lsp_server_state{language="dart"} 0` — **the Dart LSP is NOT running.**
  Active servers: `c`, `python`, `shellscript` (3 total).
- `memexd_lsp_active_servers 3` + repeated log `Global LSP server cap reached;
  refusing to start new server … running=3 max=3` — there is a **global cap of 3
  LSP servers**, already saturated. Dart (and Go/Rust/TS/Java/…) are refused.
- The registrations logged are all for the *active* repo tenant (`367157a01d98`),
  not DOC-V2 — DOC-V2's LSP is not registered/warm.

**Implication:** the R8.2 backfill loop, built naively, would deliver **zero** —
`is_server_ready_for_file` returns false for Dart. The real linchpin is a
**server-slot / activation policy**, not the loop:
- Raise / rework the `max=3` cap (it is a memory guard — LSP servers are heavy),
  or make it **on-demand per (tenant, language) for a bounded backfill window**
  (start Dart → resolve the tenant's callers → stop), or a small dedicated slot.
- Register + warm the Dart LSP for the target tenant (DOC-V2), then run backfill.

So the corrected sequence is: **R8.3 activation/cap policy FIRST** (it gates
everything), then **R8.2 backfill** using the DB primitive below.

## Status

- **DB primitive DONE:** `GraphStore::make_calls_authoritative(tenant, caller,
  source_file, resolved_names, precise_targets)` — deletes the caller's fuzzy
  CALLS to any node named a resolved name, inserts the precise LSP edges (weight
  1.0, `resolution:"lsp"`, file-owned). This is the authoritative-write the
  backfill needs; correct regardless of the activation work. Unit-tested
  (`test_make_calls_authoritative_replaces_fanout_with_precise`).
- **R8.1 DONE** (merged #191).
- **NEXT — R8.3 (activation/cap)** is now the gating step, per the probe above.

## Non-goals / guardrails

- Keep tree-sitter as the always-on baseline for the ~43 languages without a
  server (and where the LSP is cold). R8 only OVERRIDES per-call when the LSP
  actually resolved that call.
- The fan-out ceiling ([#190]) and R2.5/R4 tiers stay — they are the fuzzy-tier
  hygiene for the fallback path.
- Never block ingestion on the LSP; every LSP step is best-effort/no-op on failure.
