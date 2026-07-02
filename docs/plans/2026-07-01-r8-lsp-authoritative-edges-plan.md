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
- **R8.3a DONE (core-crate backfill):** `graph::lsp_backfill` —
  `run_backfill_tenant(store, lsp, tenant, project_root)` iterates the tenant's
  callers, resolves each via `resolved_outgoing_calls`, and stamps authoritative
  edges. Correctness-critical, unit-tested piece: `authoritative_args_for_caller`
  resolves an LSP `(name, file, line)` to the **real callee node** by `(file,
  name)` + nearest `start_line` — so a Dart `.build()` binds to its **Method**
  node, not a Function-typed guess (the whole point of R8). Dormant until wired.
- **R8.3b DONE (memexd task, flag-gated OFF — deploy-verified):**
  `background::start_graph_lsp_backfill`, wired in `main.rs`; behind
  `WQM_GRAPH_LSP_BACKFILL=1`. A periodic task, serial across tenants, that:
  (1) reads each tenant's `project_root` from `watch_folders`; (2) starts the
  tenant's dominant language server via `start_server` — **the cap is the
  linchpin** (`max_global_servers`, default 3, saturated): give the backfill a
  slot by raising `WQM_LSP_MAX_SERVERS` for the run, or a dedicated transient
  slot, and never stop a live enrichment server; (3) waits for warmup (start_server
  already blocks on the start semaphore); (4) calls `run_backfill_tenant`;
  (5) `stop_server`. Then **R8.4 measure** on DOC-V2.
- **Open subtleties for R8.3b (noted so the build is de-risked):** caller `column`
  is read best-effort from source (`symbol_column_in_line`); `start_line`
  0-indexing must match the LSP; dominant-language detection from
  `graph_nodes.language`; the whole thing is deploy-verified (no unit test spins a
  real Dart LSP) — enable on DOC-V2 first and watch the `resolution:"lsp"` edge
  count before trusting it broadly.

## Non-goals / guardrails

- Keep tree-sitter as the always-on baseline for the ~43 languages without a
  server (and where the LSP is cold). R8 only OVERRIDES per-call when the LSP
  actually resolved that call.
- The fan-out ceiling ([#190]) and R2.5/R4 tiers stay — they are the fuzzy-tier
  hygiene for the fallback path.
- Never block ingestion on the LSP; every LSP step is best-effort/no-op on failure.

## R9 (candidata) — references-based authoritative edges · Status: ☐ (registrada 2026-07-02)

Origem: investigação F2.1 (`2026-07-01-usability-graph-followups-plan.md` §5) —
ao aposentar o enrichment de chunk-payload, o insumo LSP dele revelou-se
reaproveitável: `textDocument/references` no símbolo de um nó do grafo devolve
as referências **entrantes** (quem usa o símbolo) — a direção reversa do
`callHierarchy/outgoingCalls` do R8.

**Por que vale considerar:**
- `references` é core request do protocolo (suporte quase universal);
  `callHierarchy` é capability opcional — R9 cobre servidores onde o R8 não
  consegue. **Sonda discriminante: Dart** (o problema aberto "resolve 0 mesmo
  warm" é via callHierarchy; se `references` responder, R9 destrava DOC-V2).
- Valida/suprime arestas fuzzy pelo lado de USO (complementa a supressão
  autoritativa do R8.1, que age pelo lado da chamada).

**Como (esboço):** na mesma lane de warm-backfill do R8.3b (didOpen + posições
0-based + health já corretos), por nó definido: `references(file, line, col)` →
para cada referência entrante, resolver o nó CALLER por `(file, nearest
start_line)` (mesma mecânica de `authoritative_args_for_caller`, invertida) →
aresta autoritativa `resolution:"lsp-refs"`. Insumo de código: os métodos de
consulta KEPT em `lsp/project_manager/{enrichment,imports}.rs`
(`get_references`/`get_type_info`/`resolve_imports`) — ANTES de usar, corrigir
os 4 defeitos herdados documentados na decisão F2.1: sem didOpen, off-by-one
1/0-based, erro de transporte engolido (`Ok(vec![])`), e `find_server_for_file`
ignorando `project_id`.

**Gate:** sonda Dart primeiro (script standalone como o que provou o
callHierarchy no R8.4); só construir se a sonda responder onde o callHierarchy
falha OU se a validação-por-uso medir ganho de precisão real. Nota de
semântica: `references` inclui usos não-chamada (type refs) — mapear para
`USES_TYPE`/`CALLS` pelo contexto, ou emitir uma aresta `REFERENCES` dedicada.
