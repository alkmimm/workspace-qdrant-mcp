# Search Quality — CodeRankEmbed Embedding Swap: Outcomes & Lessons (2026-06-16)

A working session that fixed the long-standing **recall-bound** search quality by
swapping the dense embedding model to a code-specialized one, then restored the
CPU failover and measured the Portuguese trade-off on a real corpus. Companion
docs: [embeddings.md](../deployment/embeddings.md) (deployment reference),
[benchmarking guide](../testing/semantic-search-benchmarking.md),
[2026-05-29 outcomes](2026-05-29-search-quality-session-outcomes.md) (prior baseline).

## What this session did

1. **Researched** the code-retrieval embedding landscape → `nomic-ai/CodeRankEmbed`
   (137M, 768d, MIT, NomicBERT) as the best self-hostable code-specialized model.
2. **Swapped the dense model** `BAAI/bge-m3` (1024d) → `CodeRankEmbed` (768d) and
   reran the destructive reembed (all 11 watched projects → **116,409 points**,
   0 failed, ~30 min on the GPU at ~5 items/s — pipeline-bound, GPU only ~33%).
3. **Measured** the 46-query benchmark before↔after; **re-tuned** fusion.
4. **Quantified the Portuguese trade-off** on the real example-monorepo corpus.
5. **Restored the CPU failover** (Infinity-on-CPU) after the ONNX export proved
   infeasible, and validated GPU→CPU failover + recovery on the live stack.

## Measured effect (46-query benchmark, `semantic` mode, rerank off)

| metric | BGE-M3 (before) | CodeRankEmbed (after) |
|---|---|---|
| recall@10 | 58.7 | **81.5** (clears the ≥70 gate) |
| top-10 | 67.4 | **87.0** |
| top-3 | 63.0 | **76.1** |
| top-1 | 47.8 | **52.2** |
| MRR | 0.54 | **0.67** |
| verdict | **POOR** | **mixed** |

Per-category top-10: `orig`/`sym`/`doc`/`real` **100%**, `impl` 92.9%, **`pt` 37.5%**
(the only weak category — see the cross-lingual finding below). This is the single
largest search-quality lift in the project's history; prior cheap levers (dedup,
overfetch, path-boost, dense-dominant fusion, reranker) had stalled because the
bottleneck was **representation**, not ranking.

## Lessons learned

1. **Representation is the recall lever for code.** When the system is
   *recall-bound* (the gold chunk is never in the candidate pool — confirmed by a
   depth sweep: recall flat at ~58–63% across k=10…100 of a 500-fetch), no
   reranker or fusion tuning can help. The only fix is a better embedding. A
   *general/multilingual* model (all-MiniLM → e5 → BGE-M3) underperforms on code;
   a *code-specialized* one (CodeRankEmbed) was **+22.8 recall@10**.

2. **Sparse/BM25 becomes near-useless once the dense model is strong on code.**
   With BGE-M3, dense-dominant `hybrid` ≈ `semantic`. With CodeRankEmbed, `hybrid`
   (72.8) fell well below `semantic` (81.5), and sweeping `WQM_KEYWORD_WEIGHT`
   0.25→0.10 moved hybrid only +1pt. **For code-heavy projects, `mode: "semantic"`
   is the best mode.** `KEYWORD_WEIGHT` was kept at the dense-dominant **0.25**
   (not lowered) so the sparse leg can still rescue exact-term / non-English
   matches in other tenants — do not over-fit it to one English repo.

3. **An English-centric code model's "Portuguese regression" is cross-lingual
   ONLY — measure before fearing it.** On the real example-monorepo corpus:
   - query-PT → **doc-PT** (same-language prose, example-monorepo's actual use): **100%
     recall** (12 known-item queries, top-1 100%, MRR 1.0). Unaffected.
   - query-PT → **code-EN** (cross-lingual): **16.7%** recall@10; the *same 12
     queries in English* → **75%**. So the gap is purely linguistic, and is
     avoidable by querying code in English (already the tool's own guidance). A
     PT→EN query-translation leg would recover it (+58pts) but adds a translation
     dependency to the search hot-path for a niche — deferred as low priority.

4. **CodeRankEmbed (custom NomicBERT) cannot be served by TEI and resists ONNX
   export.** TEI's CPU image is ONNX-only; the model ships safetensors. Three
   export attempts each hit a different wall: (a) the custom `state_dict`
   loader wants `pytorch_model.bin`; (b) after a `.bin` workaround, `optimum`
   refuses `nomic_bert` ("unsupported arch, needs a bespoke `custom_onnx_configs`");
   (c) a manual `torch.onnx.export` hit a `transformers` dynamic-module hashing
   bug on the local custom `.py`. The pragmatic CPU fallback is **Infinity on
   `--device cpu`** (PyTorch serves safetensors directly). Failover GPU→CPU and
   automatic recovery were validated live (logs: *"switching to fallback"* →
   *"Primary embedding endpoint recovered — leaving fallback"*).

5. **Operational gotchas for a model/dimension change:**
   - A **dim change** (1024→768) trips the startup dim-mismatch guard; start
     memexd once with `--bootstrap-reembed` to migrate, then `wqm admin reembed
     --confirm`, drain, remove the flag.
   - **Reembed is global** across all watched tenants — one shared dense vector
     space, so a model swap cannot be scoped to a single project.
   - The **query prefix is applied daemon-side** (`EmbedText` gRPC), so an
     asymmetric prompt (`query_prefix` set, `document_prefix` empty) takes effect
     from config alone — **no rebuild**.
   - **Do not create/switch a git branch in the watched repo right before
     benchmarking** — branch detection filters search to the new branch and
     craters recall (a false "search broke").

6. **The offline `search_eval` harness is the gate.** Inline `cases` + `rerank`
   on/off drove the whole loop — baseline, the `KEYWORD_WEIGHT` sweep, and the
   example-monorepo cross-lingual quantification — all without redeploys.

## How the swap was done (runbook recap)

Config is gitignored deploy-state; the durable record is
[embeddings.md](../deployment/embeddings.md) § "Code-aware dense model".
- `docker/.env`: `WQM_EMBEDDING_SIDECAR_MODEL=nomic-ai/CodeRankEmbed`,
  `COMPOSE_PROFILES=embeddings-cpu,embeddings-gpu`.
- `state/memexd/config.yaml`: `model: nomic-ai/CodeRankEmbed`, `output_dim: 768`,
  `query_prefix: "Represent this query for searching relevant code: "`,
  `document_prefix: ""`, `fallback_base_url: http://wqm-embeddings:80`.
- `docker-compose.yml`: the `embeddings` (CPU) sidecar swapped TEI → Infinity
  `--device cpu` (committed `e5063533d3`).

## Next steps / follow-ups

1. **Merge [PR #117](https://github.com/alkmimm/workspace-qdrant-mcp/pull/117)**
   (docs + compose). Direct push / self-merge to `main` is intentionally
   guardrailed — a human merges.
2. **(Low priority) PT→EN cross-lingual mitigation:** a multi-query leg that runs
   the original PT query + an EN translation as separate legs, RRF-merged (no
   reembed). Recovers the +58pt niche; costs a translation dependency. The niche
   is avoidable by querying code in English, so only worth it if PT code search
   becomes a real workflow.
3. **TEI CPU fallback (if ever wanted):** write a bespoke `NomicBertOnnxConfig`
   for optimum (uncertain — the rotary ops may not trace). Infinity-CPU makes
   this unnecessary today.
4. **A code-AND-multilingual model** would remove the trade-off, but no SOTA
   self-hostable option exists today; revisit as the landscape evolves.

## Watch-outs / debt

- The swap is **global**: it changed embeddings for every watched project, not
  just code ones. example-monorepo docs were validated (100%); other doc-heavy / non-English
  corpora are unmeasured.
- `KEYWORD_WEIGHT=0.25` was kept after only a quick 0.25/0.10 sweep — a fuller
  sweep could squeeze a bit more out of `hybrid`, but `semantic` is the ceiling.
- Rollback is a second full reembed: revert `config.yaml` + `docker/.env` to
  `BAAI/bge-m3` / 1024 / empty prefix, restore the TEI CPU sidecar, reembed.
