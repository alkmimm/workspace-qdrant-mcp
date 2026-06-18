# Cross-Branch File Dedup

**Status**: **Layer 1 (skip the embed) — SHIPPED.** Layer 2 (share one point
across branches) — designed, not implemented.

> **Update 2026-05-31.** Layer 1 below is implemented and wired into the
> ingestion path; the per-file embed is skipped on cross-branch duplicates by
> copying the existing Qdrant vectors under a new `base_point`. A second,
> related mechanism — in-place `tracked_files` re-keying on `git checkout` — also
> shipped. Both are described in [What shipped](#what-shipped). Layer 2 (drop
> `branch` from `base_point` and share a single point via a `branches[]` payload
> array) remains the open follow-up. The original design text is preserved below
> for rationale.

## Problem

When a user switches branches (`git checkout main` → `git checkout fork/fixes`),
the daemon re-indexes the working tree under the new branch label. Concretely:

- `tracked_files` gets a new row per file with `branch=fork/fixes`.
- `compute_base_point` includes `branch` in its hash, so the new tracked_files
  row gets a different `base_point` than the `main` row for the same physical
  content.
- Qdrant gets a fresh set of points (one per chunk) with new point_ids.
- The expensive **embed** step (FastEmbed ONNX inference per chunk) runs again
  for every chunk.

Measured on workspace-qdrant-mcp: **13 out of 15 sample cross-branch pairs have
IDENTICAL file_hash** between `main` and `fork/fixes` (87% are wasted
re-ingestion). Only 2/15 are real edits.

## Why the architecture mandates this today

1. `base_point = SHA256(tenant_id | branch | relative_path | file_hash)[:32]`
   — branch is in the hash, so identical content on different branches gets
   different base_points.
2. Qdrant payload stores `branch` as a single scalar; search filters by
   `payload.branch == "X"` for branch-scoped queries.
3. `file_metadata.branch` (search.db) plays the same role for FTS5 search.
4. Branch deletion is implemented as `DELETE WHERE branch = ?` — depends on
   each branch owning its own rows.

So the cost of branch isolation is the cost of branch re-ingestion.

## Schema infrastructure already present

- `tracked_files.base_point` (nullable) — exists, populated at insert.
- `idx_tracked_files_base_point` — index on base_point.
- `idx_tracked_files_refcount` on `(base_point, watch_folder_id)` — hints
  that ref-counted sharing was anticipated.
- `idx_tracked_files_dedup` on `(watch_folder_id, relative_path, file_hash)`
  — added 2026-05-27 for fast pre-ingestion lookup.
- `watch_folders.is_active` is already a counter (`SET is_active = is_active + 1`),
  so the codebase has precedent for refcount-style sharing.

## What shipped

Two complementary mechanisms, both of which reuse the dense+sparse vectors
verbatim and skip the dominant per-file cost (FastEmbed ONNX inference):

### A. Ingestion fast-path — `branch_dedup.rs` (this spec's Layer 1)

[`strategies/processing/file/branch_dedup.rs`](../../src/rust/daemon/core/src/strategies/processing/file/branch_dedup.rs),
called from [`ingest.rs`](../../src/rust/daemon/core/src/strategies/processing/file/ingest.rs)
before parse/embed. When a `file/add` or `file/update` arrives and another branch
already has the same `(watch_folder_id, relative_path, file_hash)`:

1. One SQL probe on `idx_tracked_files_dedup` finds the existing `base_point`.
2. `scroll_with_filter_and_vectors` pulls the old points **with vectors**.
3. `rekey_point` re-keys each point to the new `base_point` + branch (reusing the
   dense/sparse vectors and payload verbatim; only `point_id`, `base_point`,
   `branch`, `absolute_path` change) and re-upserts them.
4. A new `tracked_files` row is inserted for the branch with `source="dedup_clone"`
   and the same `chunk_count`.
5. Parse + embed are **skipped**. FTS5 (`search.db`) IS re-indexed for the new
   branch — search filters by `fm.branch = ?`, so FTS rows can't be shared.

On a 0-point scroll (stale row / partial cleanup) it falls back to a full ingest.

### B. Branch-switch re-key — `branch_switch/handlers.rs`

On `git checkout`, the switch handler diffs the two trees. Files **in** the diff
are enqueued for full ingest. Files **not** in the diff (byte-identical to the
old branch) are enqueued as `Add` ops on the new branch via
[`branch_switch/db.rs`](../../src/rust/daemon/core/src/branch_switch/db.rs)
(`fetch_unchanged_relative_paths`) + `enqueue_unchanged_file` — they then hit
mechanism **A** above, which copies the existing Qdrant points + FTS5 rows under
the new branch's `base_point` (no parse, no embed).

> **History / fix 2026-06-18.** The original mechanism B `UPDATE`d
> `tracked_files` in place (`SET branch, base_point`) via a temp-table join — SQL
> only. That relabelled the bookkeeping but never re-keyed the actual Qdrant
> points or `search.db` rows, which stayed under the *old* branch's `base_point`
> / `branch`. Since both `search` (Qdrant `payload.branch`) and `grep`
> (`file_metadata.branch`) filter by branch, every unchanged file came up
> **empty** on the new branch. The fix routes unchanged files through
> mechanism A instead (enqueue as `Add`, not `Update` — the Update pre-flight's
> non-branch-scoped defensive delete would wipe the source branch's points
> before dedup could scroll them). Net effect: the *embed* is still skipped, but
> the points/rows are physically copied so the new branch is searchable.

Net effect: switching or branching skips the (dominant) embed cost; only
genuinely changed files pay the full pipeline. Storage still duplicates per
branch — that is what Layer 2 removes.

## Original design (Layer 1 as proposed, plus the open Layer 2)

### Layer 1: Skip the embed step (cheap) — SHIPPED, see [What shipped](#what-shipped)

Before `parse_document` in [`strategies/processing/file/ingest.rs`](../src/rust/daemon/core/src/strategies/processing/file/ingest.rs):

```rust
let existing = sqlx::query_as::<_, (String, i64)>(
    "SELECT base_point, file_id FROM tracked_files \
     WHERE watch_folder_id = ?1 AND relative_path = ?2 AND file_hash = ?3 \
       AND branch != ?4 AND base_point IS NOT NULL \
     ORDER BY updated_at DESC LIMIT 1",
)
.bind(watch_folder_id).bind(relative_path).bind(&file_hash).bind(&item.branch)
.fetch_optional(pool).await?;
```

If found:
1. Compute the **new** base_point (current branch).
2. Call `storage_client.scroll_points(filter_by_base_point=existing.0)` → get
   vectors + content + payload.
3. Re-upsert under new base_point with payload updated (`branch =
   current_branch`, `base_point = new`).
4. Insert tracked_files row with new base_point + current branch +
   `chunk_count` copied from existing.
5. Skip parse + embed entirely.
6. Run FTS5 indexing as usual (new file_id, current branch).

Cost reduction: ~80% per duplicate file (embed dominates the per-file cost).

### Layer 2: Share base_point across branches (architectural)

Bigger lift; requires schema migration.

- Drop `branch` from `compute_base_point` formula (breaking change for
  existing base_points — migration script needed).
- Move `branch` out of Qdrant payload's single-value field; add
  `branches: Vec<String>` array. Search filter becomes
  `branch IN payload.branches`.
- `file_metadata.branches` likewise.
- On ingestion of identical content for a new branch: scroll points, append
  branch to `branches` array (`set_payload`).
- On branch deletion: remove branch from each point's `branches` array;
  delete the point only when `branches` becomes empty.
- Refcount on `tracked_files.base_point` already supports the SQLite side.

This eliminates storage duplication entirely. ~500 lines + migration.

## Layer 2 — concrete implementation plan (drafted 2026-06-18)

**Status: Stage 1 SHIPPED + deployed + reembed-verified (2026-06-18); Stage 2
planned.** This is a single coherent breaking change: `base_point` becomes
branch-agnostic, so identical content on N branches maps to ONE physical Qdrant
point and ONE DB row, with the branch set carried alongside. Because the
identity hash changes, **a full reembed is inherent** (every existing point_id
changes) — acceptable here (pre-release, no users; "NO MIGRATION EFFORT").

- **Stage 1 (vector dedup core) — DONE** (commit `42fe7c9c78`): branch-agnostic
  `base_point`, Qdrant payload `branch` is now an ARRAY, dedup appends a branch
  to the shared points via `set_payload` (no vector copy), `has_other_references`
  ref-counts by `file_id`, delete/update drop a branch from the array instead of
  wiping shared points. `tracked_files`/`file_metadata` rows stay PER-BRANCH (all
  sharing one `base_point`). Verified live: reembed → points carry `branch=['main']`.
- **Stage 2 (one-row collapse + FTS5 dedup) — planned**, see blueprint below.

### Data model

- `compute_base_point` → `SHA256(tenant_id | relative_path | file_hash)` (drop
  `branch`). Mirror in TS `utils/base-point.ts`. Golden vectors:
  `compute_base_point("test_tenant","src/example.rs","abc123hash")` =
  `d08103c2d8f553544dabeb4737fd32b4`; `compute_point_id(bp,0)` =
  `72deff25f27560ca51dcab1c6b373b8b`.
- **Qdrant payload**: scalar `branch` → `branches: [string]` array. Search filter
  `payload.branch == X` → `Condition::matches("branches", X)` (Qdrant keyword
  match against an array is "contains").
- **SQLite (`tracked_files`, `file_metadata`)**: one row per *content version* of
  a path, with a JSON `branches` array column mirroring the Qdrant payload.
  `UNIQUE(watch_folder_id, relative_path, file_hash)` (was `…, branch`). Drop the
  scalar `branch` column + `idx_*_branch`. Branch-scoped reads use
  `EXISTS(SELECT 1 FROM json_each(branches) WHERE value = ?)`.
- **Ingest of identical content on a new branch** = append the branch to the
  existing point's `branches[]` (`set_payload`) + add it to the row's `branches`
  JSON. No vector copy (replaces the Layer-1 scroll-and-copy in `branch_dedup`).
- **Branch delete** = remove the branch from every `branches[]`/JSON set; GC the
  point + row only when the set becomes empty.

### Cross-language impact map (file:line — verified 2026-06-18)

`compute_base_point` callers (drop `branch` arg): `common/src/hashing.rs:155`
(def + tests), `daemon/core/.../file/ingest.rs:449`, `update_preamble.rs:39`,
`zero_byte.rs:52`, `branch_dedup.rs:154`, `ingestion.rs:86`,
`schema_version/v19.rs:62`, `tests/base_point_property_tests.rs:33`,
`branch_switch/tests.rs:73`. TS: `utils/base-point.ts` (+ its golden test).

Qdrant payload `branch` writers → `branches[]`: `file/chunk_embed/payload.rs:41`,
`shared/payload_builder.rs:53`, `branch_dedup.rs:445` (rekey),
`strategies/processing/text.rs:{101,224,236,309,324}`, `url.rs:269`,
`tenant/library.rs:202`.

SQLite `tracked_files.branch`: schema `tracked_files_schema/schema.rs:{177,200,212}`
(v37 DDL + UNIQUE + idx); `operations.rs` (`lookup_tracked_file:111`,
`insert_tracked_file:161`, `get_tracked_file_paths:423`,
`get_tracked_files_by_prefix:467`, row decoder:42); `transactions.rs:{16,47}`;
`reconcile.rs:{35,78}`; `branch_dedup.rs:{88,204}`; `delete.rs:{47,368,453}`;
`store_track.rs:{71,375}`; `file/mod.rs:{453,527}`; `zero_byte.rs:{96,130}`;
`types.rs:123` (`TrackedFile.branch`). Migration: add v41 (rebuild table to new
shape + `branches` JSON; register in `schema_version`).

search.db `file_metadata.branch`: `code_lines_schema.rs:{158,203,223,313}`
(schema, idx, UPSERT, `FTS5_SEARCH_BY_PROJECT_BRANCH_SQL`); query builders
`text_search/exact_search/query_builder.rs:73`, `text_search/regex_search/query.rs:67`;
FTS writer in `branch_dedup.rs:247` + the batch writer; metrics
`monitoring/metrics_core.rs:215` + `file_metadata_stats_by_tenant_branch`. Bump
search.db schema version + migration.

Branch lifecycle / delete: `startup/reconciliation/branch_prune.rs:{157,223,252}`
(per-branch enqueue-delete → "remove branch from set + GC when empty"); ref-count
`file/delete.rs:{105,174}` (`has_other_references` by base_point already exists —
extend to count branch-set membership). proto `workspace_daemon.proto:1496`
(`ProjectPayload.branch` → `repeated branches`) + TextSearch branch field.

TS MCP server: `tools/branch-scope.ts`, `tools/search-filters.ts`,
`tools/search-qdrant.ts` (filter `branch` → `branches` contains), payload reads,
`utils/base-point.ts`.

### Ordered, compile-green stages (each ends validate-green)

1. **Vector-dedup core (deployable on its own):** `base_point` branch-agnostic +
   Qdrant `branches[]` payload + dedup-by-append (`set_payload`, no copy) +
   search filter `branches∈X` (TS) + delete removes-from-array + proto. Keeps
   per-branch `tracked_files`/`file_metadata` rows (all now sharing one
   `base_point`). This alone stops the **vector** storage doubling.
2. **One-row collapse:** `tracked_files` + `file_metadata` keyed by content with
   `branches` JSON; lookups/inserts/prune/metrics via `json_each`; v41 + search.db
   migration. This delivers the literal "one row, many branches" model and stops
   FTS5 text duplication.

### Verification

- Rust: `docker build --target validate -f docker/Dockerfile.memexd` (clippy
  `--lib --bins --tests -D warnings`) + targeted `cargo test` for hashing /
  tracked_files / branch_switch / search_db.
- TS: `npm run typecheck` + the base-point + search-filter vitest suites.
- Live: `make redeploy`, then a forced reembed, then assert (a) `points_count`
  no longer grows when checking out a second branch with identical content, and
  (b) `search`/`grep` on each branch return that branch's files and **not** files
  that exist only on the other branch (branch isolation preserved).

### Stage 2 — execution blueprint (one validate-green commit, then deploy + 2nd reembed)

All-or-nothing: the daemon won't compile/run until every item lands. **Churn-reducer:**
keep every caller's `branch` argument; absorb the branch-set entirely in the
operations layer, so the ~18 ingest call-sites are untouched.

**Schema (state.db, migration v41 — rebuild + clear; reembed repopulates):**
- `tracked_files`: drop scalar `branch`; add `branches TEXT NOT NULL DEFAULT '[]'`
  (JSON array). `UNIQUE(watch_folder_id, relative_path, file_hash)` (was `…, branch`).
  Drop `idx_tracked_files_branch`. One row per content version of a path.
- v41 `up()`: mirror `v37::rebuild_tracked_files` (FK OFF → rename→create new DDL→
  drop old→recreate indexes) + `DELETE FROM qdrant_chunks`. Bump
  `CURRENT_SCHEMA_VERSION=41`, `mod v41;` + `registry.register(Box::new(v41::V41Migration))`.

**Schema (search.db, its own migration runner — `search_db/migrations.rs`):**
- `file_metadata`: drop scalar `branch`; add `branches TEXT NOT NULL DEFAULT '[]'`.
  Keyed by `file_id` (= content-row). `code_lines` already shared by `file_id`.
- `UPSERT_FILE_METADATA_SQL`: merge the branch into `branches`
  (`json_insert`/`json_group_array(DISTINCT …)`), don't overwrite.
- FTS5 search constants (`FTS5_SEARCH_BY_PROJECT_BRANCH_SQL`) + query builders
  (`text_search/{exact,regex}_search`): `AND EXISTS(SELECT 1 FROM json_each(fm.branches) WHERE value=?)`.

**Operations layer (the only Rust logic that changes):**
- `operations.rs`/`transactions.rs`:
  - decoder → `TrackedFile.branches: Vec<String>` (parse JSON; was `branch: Option<String>`).
  - `lookup_tracked_file(watch, rel, branch)` (sig unchanged): `… AND EXISTS(SELECT 1 FROM json_each(branches) WHERE value=?3)`.
  - `insert_tracked_file_tx(… branch …)` (sig unchanged): `INSERT … branches=json_array(?branch)
    ON CONFLICT(watch_folder_id, relative_path, file_hash) DO UPDATE SET
    branches=(SELECT json_group_array(DISTINCT value) FROM (SELECT value FROM json_each(tracked_files.branches) UNION SELECT ?branch)) RETURNING file_id`.
  - `get_tracked_file_paths`/`get_tracked_files_by_prefix`: return `branches` (or the joined set).
- `types.rs`: `TrackedFile.branch → branches: Vec<String>` (grep readers — currently
  near-zero; callers use `item.branch`, not `existing.branch`).

**Delete / GC / dedup (adapt the Stage-1 logic):**
- Remove-branch-from-content now means: `UPDATE … SET branches = (json minus ?branch)`
  for the row; if `json_array_length(branches)=0` → delete the row (CASCADE
  qdrant_chunks) → then Stage-1 `has_other_references(base_point, file_id)` decides the
  Qdrant point (other watch-folder clone keeps it; else delete). Still also
  `remove_branch_from_base_point` on the Qdrant array.
- `update_preamble`: on content change, remove `item.branch` from the OLD content-row's
  `branches`; GC if empty.
- `branch_dedup`: with the upsert-merge insert, the SQL append is automatic; keep the
  Qdrant `add_branch_to_base_point` + drop the per-branch `tracked_files` insert (now
  an upsert). Likely simplifies.
- `branch_prune`: `… WHERE EXISTS(json_each(branches) … = ?branch)` → strip branch from
  each row's set, GC empties.

**Metrics:** `indexed_files_count{tenant,branch}` exporter → `json_each(branches)` so a
shared row counts once per branch it holds.

**Verify:** validate gate; targeted tests (tracked_files upsert-merge, lookup-by-branch,
prune-strips-set, FTS5 branch∈branches). Then deploy → v41 + search.db migration clear
the tables → **2nd full reembed** repopulates one row per content with its branch set
(run only after the Stage-1 reembed drains).

## Recommendation

~~Implement **Layer 1** first.~~ **Done** — see [What shipped](#what-shipped).
Layer 1 addressed the dominant cost (embed) without schema changes and is
backward-compatible.

**Open decision — Layer 2.** Layer 1 still **duplicates storage**: each branch
keeps its own copy of every point (same vectors, different `point_id`), so
`points_count` grows with `branches × unique-content`, not just unique content.
Layer 2 (share one point via a `branches[]` payload array) removes that
duplication but needs the `base_point` formula change + a migration. Pursue it
only if Qdrant storage from many long-lived branches becomes a real cost — the
compute waste (the expensive part) is already gone.

## Acceptance signals (post-Layer-1) — expected (criteria, not yet re-measured)

- Switching `main` → `fork/fixes` on workspace-qdrant-mcp re-ingests the
  ~28 modified files at full cost, but the ~1935 unchanged files complete
  in <100ms each (vs. ~2-3s for embed today).
- `wqm_unified_queue_processing_time_seconds` for `file/add` op drops by ~80%
  on branch-switch bursts.
- `tracked_files` still grows linearly with branch count (no schema change),
  but Qdrant `points_count` grows only with unique content (since we
  scroll-and-copy from existing).
