# Plano: follow-ups da avaliação de usabilidade + análise do grafo

> **Criado:** 2026-07-01 · **Status global:** F0 a fazer (deploy-only, ~minutos) · F1 a fazer (1 PR pequeno) · F2 investigação com gate de decisão.
> **Origem:** sessão de avaliação 2026-07-01 — todas as medições abaixo foram feitas ao vivo contra o stack em produção (não são estimativas).
> **Tracker vivo** — marque o status de cada item conforme avança.
> Legenda: `☐` a fazer · `◐` em andamento · `☑` concluído · `⊘` descartado / fora deste plano

## 0. Contexto e medições-fonte (2026-07-01)

- Stack 100% healthy, 9 tenants indexados a 100% (0 pending/failed); `verify-deploy` verde.
- `search_eval` (46 queries): semantic top1 60.9 / top3 78.3 / top10 89.1 / MRR 0.71 @ rerank w=0.10.
- Probe controlado de subagente (Explore, pergunta neutra, sem preâmbulo): **0/43 tool calls no MCP**
  (20 Bash / 18 Read / 4 Grep / 1 Glob, 68k tokens, 6,4 min) vs **1 chamada `search` (~90ms)** para o
  mesmo conteúdo. Mecanismo: tools deferred expõem só NOMES; instruções do servidor e CLAUDE.md não
  propagam a subagentes.
- Graph: 7 ações operacionais, latências 0ms–1.1s. `relations`/`impact`/`usages` corretos com ruído
  homônimo ETIQUETADO (confidence 0.167/0.1 vs 0.7 dos edges reais). `hotspots`/`bridges` poluídos por
  genéricos e fixtures de teste. Uplift idle roda com `updated=0` (44 passes/6h).
  `lsp_enrichments_total`: pending 7793 / skipped 18184 / **nenhuma série success**.
- DOC-V2: 55.5k nós / 838.7k arestas (735.9k CALLS ≈ 13/nó — teto fuzzy conhecido, ver plano R8).

### Fechado na própria sessão de 2026-07-01 (não repriorizar)

- ☑ `WQM_SEARCH_RERANK_WEIGHT` 0.05→0.10 em `docker/.env` + recreate mcp + verificação fim-a-fim
  (`applied.rerankWeight: 0.1`). Ganho medido: top1 +2.2pp, MRR +0.01.
- ☑ CLAUDE.md — seção "Subagent Dispatch — MCP Preamble (mandatory)" com o preâmbulo canônico.
- ☑ `docker/.env.example` — comentário da era BGE-M3 reescrito (0.10 p/ CodeRankEmbed + aviso).
- ☑ **DOC-V2 Java 22k nós = LEGÍTIMO** (diagnóstico via `list` no tenant: `doc-backend/application/src/main/java/`,
  1689 arquivos — backend Java real do monorepo, não vendor). Nenhuma ação.

---

## 1. Priorização

| Item | Título | Fase | Esforço | Risco | ROI | Status |
|------|--------|------|---------|-------|-----|--------|
| **F0.1** | Knobs de centralidade no `docker/.env` (deploy-only) | 0 | S | baixo | **alto** | ☑ (§8) |
| **F0.2** | PR docs: CLAUDE.md + `.env.example` (já editados no working tree) | 0 | S | ~zero | médio | ☐ |
| **F0.3** | GC de watches `local_*` órfãos (worktrees `.claude/` mortos) | 0–1 | S–M | baixo | baixo-médio | ☐ |
| **F1.1** | `minConfidence` em `relations`/`impact`/`usages` (daemon-side) | 1 | M | baixo-méd | **alto** | ☐ |
| **F1.2** | Documentar semântica de confidence na descrição da tool `graph` | 1 | S | baixo | médio | ☐ |
| **F2.1** | Diagnóstico uplift no-op + decisão: enrichment de chunk × R8-edges | 2 | M (investigação) | médio | alto (destrava) | ☐ |
| **F2.2** | Atribuir `language` a nós file/module (~20-25% "unknown") | 2 | S–M | baixo | baixo | ☐ |
| — | Dart warm-resolve = 0 | — | — | — | — | ⊘ já coberto por `2026-07-01-r8-lsp-authoritative-edges-plan.md` |
| — | PT cross-lingual (top3 25%) | — | — | — | — | ⊘ já coberto por P2.8/P2.9 do plano de ergonomia 2026-06-22 |

**Racional da ordem:** F0.1 é a maior razão ROI/esforço do lote (1 linha de env conserta a cara do
`hotspots`/`bridges` para TODOS os tenants). F1.1 remove o único atrito de qualidade nas ações
por-símbolo. F2.1 é investigação: decide antes de codar para não duplicar o R8.

---

## 2. Roadmap

```
FASE 0 — deploy-only + PR docs (hoje, sem código)
  F0.1 ── docker/.env + recreate memexd + re-probe registrado aqui
  F0.2 ── branch + PR (docs-only)
  F0.3 ── decidir canal (ver card): one-off manual OU startup-GC pequeno (vira F1)

FASE 1 — 1 PR pequeno (proto + daemon + TS, file-disjoint de F0)
  F1.1 → F1.2   (mesma PR; descrição referencia o param novo)

FASE 2 — investigação gated
  F2.1 ── diagnóstico (≤1h) → DECISÃO registrada neste doc → só então PR
  F2.2 ── oportunista (pode pegar carona na PR de F2.1 se tocar o mesmo módulo)
```

**Gates:** F0 → `make verify-deploy` + re-probe antes/depois colado no tracker. F1 → `tsc --noEmit` +
suíte vitest COMPLETA + `cargo test` (alvos graph) + gate `validate` container. F2.1 → decisão
escrita na §5 antes de qualquer código. PRs file-disjoint; fork `alkmimm/` apenas; sem push em
branch após squash-merge.

---

## 3. Card F0.1 — Knobs de centralidade (deploy-only) · Status: ☐

**Objetivo.** Tirar genéricos (`with_capacity`, `as_ref`, `find`) e símbolos de tests/fixtures
(`Error` de `fixture_data/sample_rust.rs`) do top-10 de `hotspots`/`bridges`, usando os filtros que
JÁ EXISTEM no daemon e estão sem valor no deploy.

**Fundamentação no código** (`src/rust/daemon/core/src/graph/algorithms/mod.rs`):
- `centrality_usage_threshold()` (linha ~123): filtro por in-degree (edges weight≥0.6); default
  `max(50, min(total/150, 125))`. O comentário de calibração (3 tenants) registra **pico de domínio
  real ~111-113 in-degree e piso genérico ~118+** — o cap 125 deixa passar a faixa 118-125, que é
  exatamente onde `as_ref`/`with_capacity` vivem neste repo.
- `is_centrality_excluded()` (linha ~61): substring match sobre `file_path`, **só centralidade**
  (search/grep/relations/impact veem o grafo completo — linha ~227).
- `centrality_generic_threshold()` (linha ~90): ubiquidade de DEFINIÇÃO — default dinâmico ok, não mexer.
- `centrality_manual_skip_symbols()` (linha ~71): lista manual por nome — reserva empírica.
- Todos são `OnceLock` (parse 1×/processo) → **recreate do memexd obrigatório** após mudar env.

**Mudança (docker/.env):**

```bash
# antes
WQM_GRAPH_CENTRALITY_EXCLUDE=old_project/,docs/archive/,OuterClass,.pb.,_pb2,.g.dart,.freezed.dart,/generated/
# (GENERIC_THRESHOLD / SKIP_SYMBOLS / USAGE_THRESHOLD ausentes → defaults)

# depois
WQM_GRAPH_CENTRALITY_EXCLUDE=old_project/,docs/archive/,OuterClass,.pb.,_pb2,.g.dart,.freezed.dart,/generated/,shared-test-utils/,fixture_data/,/tests/,tests.rs,_test.rs,.test.ts,.spec.ts
WQM_GRAPH_CENTRALITY_USAGE_THRESHOLD=115   # dentro do gap medido 113↔118
```

Aplicação: `docker compose --env-file docker/.env -f docker-compose.yml up -d --force-recreate memexd`
(knobs são do serviço memexd, docker-compose.yml:229-234) + `make verify-deploy` (o check §2 passa a
mostrar os valores).

**Critérios de aceite.**
- Re-probe `graph hotspots topK:10` e `bridges topK:10 maxSamples:200` no tenant 367157a01d98:
  zero símbolos com path em tests/fixtures; `with_capacity`/`as_ref`/`find` fora do top-10.
- Sobreviventes esperados continuam lá (`GraphDbResult`, `GraphDbError`, `SchemaError`, `ProjectDetector`).
- Antes/depois colado neste doc (§8). Se sobrar genérico pontual, aí sim `SKIP_SYMBOLS=<nome>` (não antes).
- Efeito é global (todos os tenants) — conferir DOC-V2 `hotspots` também (excludes de teste valem lá).

**Esforço/Risco.** S / baixo (reversível por env; afeta só centralidade). **Rollback:** remover as
linhas + recreate.

---

## 4. Card F1.1 + F1.2 — `minConfidence` nas leituras do grafo · Status: ☐

**Objetivo.** `relations` devolveu, junto de 2 CALLS reais @0.7, **6 candidatos `is_empty`
@0.1667** (fan-out 1/N do resolvedor por nome) e `impact` devolve indirect_reference @0.1. O dado
para filtrar JÁ EXISTE no edge; falta o agente poder pedir "só o que é confiável" sem pagar os
tokens do ruído.

**Desenho (segue a regra de projeto `grpc-list-cap`: bound na FONTE, não client-side):**
- Proto (`src/rust/daemon/proto/workspace_daemon.proto`): campo opcional `min_confidence` (float;
  0/ausente = sem filtro, comportamento atual inalterado) em `QueryRelatedRequest` e
  `ImpactAnalysisRequest` (cobre impact e usages, que compartilham o request).
- Daemon: aplicar o corte DENTRO da travessia BFS (não pós-corte), para que `topK` nearest-first
  seja preenchido com nós que passam o filtro; `total`/`total_impacted` continuam reportando o
  universo pós-filtro.
- TS: `minConfidence?: number` no schema da tool `graph` (ações relations/impact/usages), threading
  no client gRPC + cópia do proto TS + interface; ler totais do proto, não do array.
- **F1.2 (mesma PR):** descrição da tool ganha a semântica: *"cada nó/edge traz `confidence`:
  ~0.7+ = resolução única; ~1/N (ex.: 0.167) = candidato homônimo (fan-out); indirects de impact
  usam 0.1. Passe `minConfidence: 0.5` para modo preciso."*

**Testes (critério da regra grpc-list-cap):** regression TS asserting que `minConfidence` chega ao
daemon (padrão de `graph.test.ts`); unit Rust na travessia (grafo sintético com edge 0.7 + fan-out
1/6 → filtro 0.5 devolve só o 0.7); sem o param, snapshot idêntico ao atual.

**Aceite fim-a-fim.** `graph relations symbol:reconcile_branch_membership minConfidence:0.5` →
retorna `enqueue_unchanged_file` + `fetch_paths_missing_branch` (+ stubs externos conf 1.0), SEM os
6 `is_empty`. Reconectar o cliente MCP após deploy (cache de ListTools) para a descrição nova valer.

**Esforço/Risco.** M / baixo-médio (proto muda → rebuild das duas imagens; sem migração — projeto
sem usuários).

---

## 5. Card F2.1 — Uplift no-op + destino do enrichment de chunk · Status: ☐ (investigação)

**Evidência (2026-07-01).**
- Logs: 44× "Uplift pass complete: … updated=0 … errors=0" em 6h — o loop roda e não produz nada.
- Métricas: `memexd_lsp_enrichments_total{pending}=7793`, `{skipped}=18184`, **sem série success** —
  nenhum chunk jamais completou enrichment LSP (nem no ingest, nem via uplift).
- Código: `metadata_uplift.rs` importa só `LexiconManager` + storage (linha 16-17) — o uplift
  re-tenta TAGS, **não re-tenta LSP** (apesar do doc-comment prometer "re-attempts enrichment").
- `find_points_needing_uplift` (linha ~94-101) pula pontos com `uplift_generation >= current_generation`,
  e `UpliftConfig::default().current_generation = 1` — **hipótese A:** se ninguém incrementa a
  generation entre passes, após a 1ª passada todo candidato vira `skipped` para sempre (bate com
  scanned=N/updated=0/skipped=N).
- Comentário linha 67: `pending` = "LSP server não estava pronto no processamento inicial" —
  **hipótese B:** o ingest em massa roda antes do warm-up do LSP (grace Dart180/Rust120 do PR #194),
  então TUDO nasce pending e nada volta para re-tentar.

**Passos (≤1h, nesta ordem):**
1. Confirmar hipótese A: quem chama `find_points_needing_uplift` e se `current_generation`
   incrementa (`unified_queue_processor/processing_loop/idle_work.rs`).
2. Inventariar CONSUMIDORES dos campos LSP do payload de chunk (o que `lsp_payload.rs` grava além
   do status — hover/types/refs?). Se **ninguém consome** (busca não usa, graph usa a via R8):
   → **Opção (a) APOSENTAR** o enrichment de chunk-payload: remover status/metric/uplift-LSP,
   documentar que a via LSP oficial é R8-edges (`resolved_outgoing_calls`). Simplifica e apaga o
   sinal falso de "pending eterno".
3. Se houver consumidor real: → **Opção (b) LIGAR** o uplift ao `ProjectLspManager` (re-tentar
   `pending` quando `memexd_lsp_server_state{language}=1`), com incremento de generation por passe
   e backoff. Cuidado para não competir com o backfill R8 (mesma infra didOpen do fix #193).

**Gate:** decisão (a)×(b) registrada AQUI antes de abrir PR. Não misturar com F1.1.

**Aceite.** (a): métrica/status removidos, `Uplift pass` some do idle-log ou vira tags-only
explícito; (b): série `success` crescendo e `pending` drenando em janela de 24h.

---

## 6. Card F0.3 — GC de watches `local_*` órfãos · Status: ☐

**Sintoma.** `make reindex-status` lista 3 tenants `local_*` 100%/0 arquivos apontando para
`.claude/worktrees/{_rebase109,gallant-banach-b5329f,dazzling-almeida-865f63}` — worktrees efêmeros
já removidos do disco. Inertes (0 itens), só poluem status/registry.

**Mecanismos existentes** (não inventar novo): `ArchiveWatchData/ArchiveWatchResult` no write_actor
(`src/rust/daemon/core/src/write_actor/commands.rs`) e `PathValidator` (v0.1.3 — a memória do
projeto registra que o WatchManager nunca o consome; o orphan-watcher do PR #91 só auto-DESABILITA).

**Opções.**
- (i) **One-off manual** (resolve hoje): enfileirar archive para os 3 tenants via canal admin
  (admin HTTP em :6335; endpoints atuais só pause/resume — `admin/static/app.js:582,593` — então o
  one-off provavelmente é um script curto contra o SQLite copiado, padrão `docker cp` + python3 da
  memória branch-prune, OU um endpoint novo mínimo).
- (ii) **Startup-GC permanente** (recomendado como F1-carona): na inicialização, para watches com
  `tenant_id LIKE 'local_%'` E path inexistente E `done=0` → archive automático via write_actor.
  Guard triplo para nunca comer projeto real: só `local_*` + path morto + zero itens rastreados.

**Aceite.** `make reindex-status` sem entradas mortas; criar+remover um worktree `.claude/` novo e
verificar que o registro some no restart seguinte; watch de projeto real com path temporariamente
desmontado NÃO é arquivado (guard `done=0`).

---

## 7. Card F2.2 — `language` em nós file/module · Status: ☐

**Sintoma.** `memexd_graph_nodes_by_language` mostra ~20-25% "unknown" em todo tenant (este repo:
5326/23.8k ≈ nós `file` 2222 + `module` 2002 + stubs). O gauge é emitido em
`src/rust/daemon/memexd/src/background.rs:1107+` a partir da coluna `language` de `graph_nodes`.

**Fix.** Atribuir language na CRIAÇÃO do nó file/module (derivar da extensão via language registry —
mesma fonte do chunker). Sem migração (regra do projeto): nós existentes convergem no próximo
scan/reembed de cada arquivo. Stubs continuam unknown por definição (sem arquivo).

**Aceite.** Após rescan de um projeto, novos nós file/module carregam language; "unknown" no tenant
tende a ≈ contagem de stubs. Dashboard Project×Language (do PR #193) fica fiel.

---

## 8. Registro antes/depois — F0.1 EXECUTADO 2026-07-01 ☑

Aplicado em 2 iterações (cada uma = recreate memexd, knobs são `OnceLock`):
**iter-1** = excludes de teste/fixture + `USAGE_THRESHOLD=115`; **iter-2** =
`SKIP_SYMBOLS=as_ref,with_capacity,deserialize,Deserialize,from_str,extend`.
Nós considerados na centralidade: 19.478 → 16.906 (excludes) → 16.883 (skip).

```
hotspots ANTES : GraphDbResult, format_utc, connect, GraphDbError, now_utc,
  with_capacity*, find*(test), Error*(fixture), as_ref*, SchemaError            (*ruído)
hotspots iter-1: GraphDbResult, with_capacity*, GraphDbError, connect, from_user_input,
  ConfigPathError, Deserialize*, as_ref*, SchemaError, deserialize*   (fixture/test OUT;
  format_utc/now_utc/find/Error OUT; sobrou std/serde generic)
hotspots DEPOIS: GraphDbResult, GraphDbError, connect, ConfigPathError, SchemaError,
  output(mod), get_database_path, cmp, extension, LanguageMap   (só domínio real ✓)

bridges  ANTES : Error*(fixture), as_ref*, ProjectDetector, sleep*, sample_rust.rs*(file),
  with_capacity*, now_utc, timeout, find*(test), extract_object
bridges  DEPOIS: extract_object, basename, connect_readonly, timeout, output(mod),
  extension, cmp, show.rs(file), session-lifecycle.ts(file), routes.ts(file)
  (funções/arquivos reais; nenhum fixture/std-generic ✓)
```

**Veredicto.** Objetivo atingido: zero símbolos de teste/fixture e zero std/serde
generics no top-10 de ambos. Resíduo (`cmp`, `extension`, `basename`, `timeout`) são
funções REAIS em código-fonte real — legítimo para betweenness (surface de conectores).
NÃO baixar `USAGE_THRESHOLD` abaixo de 115 (pico de domínio real medido ~113 seria
comido). Efeito é global; conferir DOC-V2 fica como spot-check opcional.

---

## 9. Fora deste plano (ponteiros)

- **Homônimos de método / inflação CALLS (~13/nó em DOC-V2):** teto estrutural fuzzy — a alavanca é
  a trilha R8/LSP (`docs/plans/2026-07-01-r8-lsp-authoritative-edges-plan.md`), incluindo o item
  aberto "Dart resolve 0 mesmo warm". Este plano NÃO adiciona heurísticas novas (decisão R7: stop).
- **PT cross-lingual (bucket pt top3 25%):** mitigação em vigor (queries em EN nas descrições +
  preâmbulo); fix profundo = P2.8 multi-query gated por P2.9 (plano de ergonomia 2026-06-22, §8).
- **Adoção de MCP por subagentes:** resolvido no nível de processo (CLAUDE.md, sessão 2026-07-01);
  a solução definitiva seria harness-side (propagação de instruções a subagentes) — fora do nosso
  controle; reavaliar se o cliente evoluir.
