# Plano: melhorias de ergonomia para agentes — workspace-qdrant-mcp

> **Criado:** 2026-06-22 · **Status global:** onda 1 **MERGED** em `main` (squash, commit `1d81a23`) — [#136](https://github.com/alkmimm/workspace-qdrant-mcp/pull/136) (P0.x), [#137](https://github.com/alkmimm/workspace-qdrant-mcp/pull/137) (P1.7), [#138](https://github.com/alkmimm/workspace-qdrant-mcp/pull/138) (P1.6.x). **Deploy OK** (`make redeploy`) + gate `validate` ✓ (clippy `-D warnings` limpo, cobre #138) + A/B do P1.7 rodado. **P1.7 tuned p/ w=0.10** ([#139](https://github.com/alkmimm/workspace-qdrant-mcp/pull/139) MERGED + deployado, live). Onda 1 + tuning **completos e no ar**. Onda 2 (P1.4/P1.5, P2.8/P2.9) a fazer (ver §8).
> **Tracker vivo** — marque o status de cada item conforme avança (legenda abaixo).
> Legenda de status: `☐` a fazer · `◐` em andamento · `☑` concluído · `⊘` descartado

## 0. Contexto e fontes

Este plano cruza **SOTA externo** (pesquisa multi-fonte verificada adversarialmente: MCP spec
2025-06-18, blogs de engenharia Anthropic, arXiv 2605.15184 grep-vs-vector, arXiv 2602.03442 A-RAG,
Perplexity AI-first search) com um **gap analysis interno** do repo. Cada item abaixo foi **aterrado no
código real** (arquivos + linhas citados) e passou por um **revisor adversarial** que validou que os
trechos existem, compilam e que os critérios de aceite são testáveis. As correções do revisor já estão
incorporadas.

**Princípios herdados (CLAUDE.md):** sem esforço de migração / sem usuários; edição cirúrgica em arquivos
quote-heavy; ao corrigir comportamento compartilhado, replicar em `search`/`grep`/`list`/`retrieve`/CLI/daemon
ou justificar a diferença; corrigir testes que quebrarem; build é container-first (`make redeploy`).

**Validado pelo SOTA (NÃO mexer):** hybrid dense+sparse + RRF (`RRF_K=60`), chunking semântico tree-sitter,
truncamento por hit + `summary` + strip de metadata de ranking, `grep` como tool de primeira classe,
`readOnlyHint` para auto-approve (PR #130).

---

## 1. Priorização

Critério: **ROI medido ÷ esforço ÷ risco**. A literatura coloca *riqueza da definição de tool* como a
alavanca de maior retorno comprovado (79.5%→88.1% em seleção de tool; 72%→90% em parâmetros complexos;
−40% no tempo de tarefa ao reescrever **uma** descrição). Por isso a Fase 0 (P0) vem primeiro: é `S` de
esforço, risco baixo e ataca exatamente essa alavanca.

| Item | Título | Fase | Esforço | Risco | ROI | Depende de | Status |
|------|--------|------|---------|-------|-----|------------|--------|
| **P0.1** | `destructiveHint`/`idempotentHint` nas tools que mutam | 0 | S | baixo | alto | — | ☐ |
| **P0.2** | Exemplos de chamada + fronteiras nas descrições | 0 | S | baixo | **muito alto** | — | ☐ |
| **P0.3** | Corrigir default `store` (`?? 'library'`) + split store/scratchpad | 0 | S–M | médio | alto | P0.2 (merge) | ☐ |
| **P1.7** | Reranker soft-default ON (w=0.05) | 1 | S | baixo | médio-alto | — | ☐ |
| **P1.4** | `outputSchema` + `structuredContent` (tools read-only) | 1 | M | baixo-méd | alto | — | ☐ |
| **P1.5** | `responseFormat` + paginação + teto global de bytes | 1 | M | médio | alto | P1.4 | ☐ |
| **P1.6.a** | CLI `wqm admin search-latency` (p50/p95/p99) | 1 | M | baixo | alto | — | ☐ |
| **P1.6.b** | Reconstrução de cadeia de tool-calls | 1 | M | médio | médio | P1.6.a | ☐ |
| **P1.6.c** | Proxy de auto-approve (inter-call gap) | 1 | S | baixo | médio | P1.6.a | ☐ |
| **P2.8** | Multi-query na perna densa (experimental) | 2 | L | médio-alto | incerto | — | ☐ |
| **P2.9** | Experimento grep-vs-vetor + inline-vs-file | 2 | M | baixo | médio | — | ☐ |
| **P3** | Primitivas de orquestração (só sob demanda) | 3 | L | alto | **não comprovado** | — | ⊘ adiado |

---

## 2. Roadmap por fase (sequenciamento)

```
FASE 0 — Tool ergonomics (1 PR, ~1 dia)        → maior ROI, risco baixo
  P0.1  ──┐
  P0.2  ──┼─ independentes (P0.3 coordena texto com P0.2)
  P0.3  ──┘

FASE 1 — Output, economia de token, observabilidade, rerank (2–4 PRs)
  P1.7  ── quick win isolado (flip + A/B no search_eval)
  P1.4 → P1.5            (outputSchema antes da paginação que muda a resposta)
  P1.6.a → P1.6.b
        └→ P1.6.c        (ambos estendem o mesmo comando novo)

FASE 2 — Experimentos / apostas (medir antes de adotar)
  P2.9  ── desenho de experimento (quase sem código)
  P2.8  ── lane experimental OFF por padrão, gated por eval

FASE 3 — Orquestração: NÃO construir agora (ver §6)
```

**Gate entre fases:** cada fase só "fecha" quando `tsc --noEmit` (exactOptionalPropertyTypes ON) + suíte
vitest + `cargo test` (alvos tocados) passam, e — para itens de retrieval — quando o `search_eval` A/B
está registrado no PR.

---

## 3. Controle de progresso

**Fase 0** — ☑ P0.1 · ☑ P0.2 · ☑ P0.3 → [#136](https://github.com/alkmimm/workspace-qdrant-mcp/pull/136) **MERGED** (tsc+test ✓) · ☑ deploy (`make redeploy`) · ☑ smoke (stack healthy)
**Fase 1** — ☑ P1.7 → [#137](https://github.com/alkmimm/workspace-qdrant-mcp/pull/137) **MERGED** + gate `validate` ✓ · ☐ P1.4 · ☐ P1.5 · ☑ P1.6.a · ☑ P1.6.b · ☑ P1.6.c → [#138](https://github.com/alkmimm/workspace-qdrant-mcp/pull/138) **MERGED** (gate `validate` ✓: clippy `-D warnings` limpo) · ☑ deploy · ☑ eval A/B (P1.7): **w=0.05 ≈ OFF** (semantic flat, hybrid recall −1.1pp); **w=0.10** semantic top1 56.5→58.7 / mrr 0.69→0.70 mas hybrid recall −3.3pp → **tuned p/ w=0.10** [#139](https://github.com/alkmimm/workspace-qdrant-mcp/pull/139) **MERGED** + deployado (`make mcp-rebuild`, stack healthy)
**Fase 2** — ☐ P2.9 (desenho) · ☐ P2.9 (run + writeup) · ☐ P2.8 (impl) · ☐ P2.8 (eval gate)

Atualize as caixas nas tabelas da §1 e §4 conforme cada item progride.

---

## 4. Cards detalhados

> Cada card: **Objetivo · Arquivos · Antes/Depois · Exemplo · Critérios de aceite · Esforço/Risco · Status**.
> Trechos `Antes` são citações verbatim do código atual (verificadas pelo revisor).

---

### P0.1 — `destructiveHint`/`idempotentHint` nas 4 tools que mutam · Status: ☐

**Objetivo.** A interface `McpToolDefinition` **já declara** os dois campos, mas nenhuma tool os seta.
Hoje o cliente não distingue um `delete` destrutivo de um write seguro — e a spec do SDK diz que
`destructiveHint` **default é `true`**, então toda tool que muta e omite o campo é tratada como destrutiva.
Isso embota o auto-approve que o PR #130 construiu.

**Arquivos.**
- `src/typescript/mcp-server/src/tool-definitions/{rules,store,scratchpad,workspace-index}.ts`
- (passthrough confirmado) `src/typescript/mcp-server/src/server.ts:132` retorna `getToolDefinitions()` direto
- `node_modules/@modelcontextprotocol/sdk/.../types.js:1192,1201` declara `destructiveHint`/`idempotentHint` (campos passam; chaves extras seriam descartadas, mas estas são declaradas)
- `tests/tool-definitions.test.ts` (só lê `title`/`readOnlyHint`/`openWorldHint` — não quebra)

**Antes/Depois** (mesma forma nos 4 arquivos; valores por mix de ações):

```ts
// rules.ts — tem 'remove' (deleta) → destructiveHint:true
// store.ts — additive-only (sem delete) → destructiveHint:FALSE  ← o que evita o prompt "destrutivo" falso
// scratchpad.ts — tem 'delete' → destructiveHint:true
// workspace-index.ts — cleanup_orphans/abandon_agent_branch → destructiveHint:true

// store.ts ANTES
  annotations: { title: 'Store note/snippet/library/project', openWorldHint: false },
// store.ts DEPOIS
  annotations: {
    title: 'Store note/snippet/library/project',
    openWorldHint: false,
    destructiveHint: false, // additive only; no delete path
    idempotentHint: false,  // re-storing creates/updates content
  },
```

**Exemplo (ListTools que o cliente recebe).**
```json
{ "name": "store", "annotations": { "openWorldHint": false, "destructiveHint": false, "idempotentHint": false } }
```
Efeito: `store` renderiza como write não-destrutivo (sem prompt assustador), enquanto `rules`/`scratchpad`/`workspace_index` continuam pedindo confirmação no caminho de delete.

**Critérios de aceite.**
- `tsc` compila; `tool-definitions.test.ts` passa sem mudança.
- ListTools retorna `destructiveHint`/`idempotentHint` nas 4 tools; nenhuma delas tem `readOnlyHint===true`.
- `store.destructiveHint===false` e `scratchpad.destructiveHint===rules.destructiveHint===true`.
- Novo teste fixa esses valores.

**Esforço:** S · **Risco:** baixo (metadata aditiva; só acertar `store=false`).

---

### P0.2 — Exemplos de chamada + fronteiras inter-tool nas descrições · Status: ☐

**Objetivo.** Alavanca de maior evidência quantitativa de toda a pesquisa. O SDK TS **não tem** campo
`examples`/`sample` (verificado: `ToolSchema` usa `.catchall(z.unknown())`, mas nenhum cliente lê chave
extra) → os exemplos vão **inline na string `description`**.

**Arquivos.** `tool-definitions/{store,scratchpad,workspace-index}.ts` (descrições); `search.ts:19` como
referência de estilo direto.

**Depois** (append de uma cláusula `Examples:` + `Boundary:` curta por descrição):

```
// store.ts (anexar antes do fecho da string)
... project → path. Boundary: para MODIFICAR ou DELETAR uma nota existente use a tool `scratchpad`,
não `store` (store só cria/atualiza). Examples — note: {type:"scratchpad", content:"retry uses backoff", cwd:"/abs/repo"};
library: {type:"library", libraryName:"tokio", content:"...", title:"Tokio"}; project: {type:"project", path:"/abs/repo"};
url: {type:"url", url:"https://docs.rs/tokio"}.

// scratchpad.ts
... Boundary: para CRIAR nota use store(type:"scratchpad"); esta tool nunca cria.
Examples — list: {action:"list"}; delete: {action:"delete", content:"<texto verbatim>"}; update: {action:"update", content:"<old>", newContent:"<new>"}.

// workspace-index.ts
... Boundary: para busca de código/doc use search/grep; para notas use store/scratchpad; esta tool só gerencia o registro de indexação.
Examples — status: {action:"indexing_status", cwd:"/abs/repo"}; list: {action:"list_projects"}; (mutating) add: {action:"add_project", projectPath:"/abs/repo", allowMutation:true}.
```

**Exemplo (efeito no agente).** Lê a descrição enriquecida e emite a chamada mínima correta:
`store({ type:"scratchpad", content:"queue drain stalls under memory pressure", cwd:"/abs/repo" })`.

**Critérios de aceite.**
- Cada descrição contém ≥1 objeto-arg literal + cláusula `Boundary:` nomeando a tool irmã.
- Nenhum exemplo referencia param fora do `inputSchema` da tool.
- Continua string única; suíte passa. Opcional: teste fixando substrings `Examples`/`Boundary`.

**Esforço:** S · **Risco:** baixo (texto). Cuidado: escapar `\"` dentro da string single-quoted; manter
cláusula curta (~2–3 frases) p/ não inflar o custo de ListTools.

---

### P0.3 — Corrigir o default de `store` + clarificar split store/scratchpad · Status: ☐

**Objetivo.** `store({content})` sem `type` **falha hoje** porque cai no branch `library` (que exige
`libraryName`) — a escrita mais natural do agente é o caminho mais quebrado.

**⚠ Correção do revisor (crítica).** O default **NÃO** está no schema — está no runtime, e o servidor **não
valida `required`** (não há Ajv/zod-required em lugar nenhum sob `src/`; o SDK só valida o envelope da
request). Logo `required:['type']` sozinho **não muda o comportamento do servidor**. O default real é:

```ts
// src/typescript/mcp-server/src/tool-dispatcher.ts:45  (ANTES)
const storeType = (args?.['type'] as string) ?? 'library';
// → omitir type cai em library e falha em tools/store.ts:182 ("libraryName is required")
```

**Depois (recomendado — opção A: erro explícito que ensina o agente).**
```ts
// tool-dispatcher.ts:45 (DEPOIS)
const storeType = args?.['type'] as string | undefined;
if (!storeType) {
  return { isError: true, content: [{ type: 'text', text: JSON.stringify({
    success: false,
    error: "store requires `type`: 'scratchpad' (notes), 'library' (docs — needs libraryName), 'url', or 'project'.",
  }) }] };
}
```
+ manter `required: ['type']` no inputSchema (hint client-side) + remover das 2 docs a frase "defaults to
library" + adicionar o cross-ref reverso (store → use `scratchpad` para editar/deletar).

**Alternativa (opção B):** trocar o default para `?? 'scratchpad'` — `store({content})` vira nota
silenciosamente. Mais arriscado (muda roteamento; pode mal-guardar um doc de library). Preferir A.

**Exemplo.** Antes: `store({content:"x"})` → erro confuso "libraryName required". Depois (A):
`store({content:"x"})` → erro claro "store requires `type`…" guiando para `store({type:"scratchpad", content:"x"})`.

**Critérios de aceite.**
- `tool-dispatcher.ts:45` editado (não só schema/docs).
- Nenhuma doc de `store` ainda diz que `type` faz default p/ "library".
- `store` tem cross-ref para a tool `scratchpad` (simétrico ao ponteiro que `scratchpad.ts:12` já tem).
- Chamada sem `type` retorna erro acionável citando `type`, não "libraryName required".
- Novo teste fixa `store.inputSchema.required` contém `type`.

**Esforço:** S–M · **Risco:** médio (muda failure-mode de `store()` bare; confirmar que nada depende do
default `'library'` antes de mergear). **Dependência:** P0.2 (apenas merge-order — ambos editam a mesma
frase da descrição de `store`).

---

### P1.7 — Reranker como soft-default ON (w=0.05) · Status: ☐

**Objetivo.** Os ganhos medidos de top1/MRR em w=0.05 (PR #103) ficam na mesa porque o reranker é
**OFF por padrão**. O blend já está cabeado e é best-effort (erro do daemon → retorna ordem pré-rerank),
então ligar o default não pode quebrar resultados — só adiciona latência/ordem.

**Arquivos.** `src/typescript/mcp-server/src/tools/search-helpers.ts:1205` (gate); `:1026` (`RERANK_WEIGHT=0.05` já é o default); `:1012` (`RERANK_POOL=30`); `tools/search-eval.ts:187` (espelho); docs em `search-types.ts:85-88` e `tool-definitions/search-eval.ts:62-63`.

**Antes/Depois.**
```ts
// search-helpers.ts:1205 ANTES
const rerankDefault = process.env['WQM_SEARCH_RERANK'] === '1';
// DEPOIS — soft default ON; '0' força OFF, '1' força ON; per-call `rerank` ainda sobrepõe
const rerankEnvOverride = process.env['WQM_SEARCH_RERANK'];
const rerankDefault = rerankEnvOverride === undefined ? true : rerankEnvOverride !== '0';

// search-eval.ts:187 — espelhar a MESMA resolução
const appliedRerank = rerank ?? (rerankEnvOverride === undefined ? true : rerankEnvOverride !== '0');
```
**⚠ Correção do revisor:** editar as 2 docstrings **separadamente** — só `search-types.ts:85-88` contém
literalmente "code default is off" (trocar p/ "code default is ON; WQM_SEARCH_RERANK=0 disables"); a de
`search-eval.ts:62-63` tem outro texto (anexar "(code default ON; WQM_SEARCH_RERANK=0 forces off)").

**Exemplo.** `search({query:"where is reciprocal rank fusion applied", fileType:"code"})` agora roda o blend
automaticamente (cada hit ganha `rerankScore`; `score` segue cosseno cru). Kill-switch ops:
`WQM_SEARCH_RERANK=0` no `docker/.env`. A/B sem redeploy: `search_eval{rerank:false}` vs `{rerank:true, rerankWeight:0.05}`.

**Critérios de aceite.**
- `search_eval` sem arg → `applied.rerank===true` (era false); `{rerank:false}`→false; `{rerank:true}`→true.
- `WQM_SEARCH_RERANK=0` + sem arg → `applied.rerank===false`.
- A/B no set de 46 queries (semantic+hybrid): registrar `{top1,top3,top10,recallAt10,mrr,avgLatencyMs}`
  rerank:false vs true@0.05. Aceite: top1/mrr não regridem; recall@10 não cai >1pp; delta de latência no PR.
- Backend de rerank indisponível → search ainda retorna (erro engolido), sem `rerankScore` (teste com mock que lança).

**Esforço:** S · **Risco:** baixo (só flip do boolean; latência +~24ms + warm-up; A/B + kill-switch cobrem).
Follow-up opcional: gatear o default por flag de disponibilidade sondada.

---

### P1.4 — `outputSchema` + `structuredContent` nas tools read-only · Status: ☐

**Objetivo.** Dar ao agente resultado validável por máquina (`search`/`grep`/`list`/`retrieve`/`graph`),
mantendo o fallback de JSON serializado em `TextContent`. **SDK já suporta** (`@modelcontextprotocol/sdk ^1.29.0`:
`outputSchema` na Tool, `structuredContent` no CallToolResult) — sem bump.

**Arquivos.** `tool-definitions/index.ts:33-48` (interface — add `outputSchema?`); novo `tool-definitions/output-schemas.ts`; `tool-definitions/{search,grep,list,retrieve,graph}.ts`; `tool-dispatcher.ts:29-32` (tipo `ToolResult`) e `:131` (builder); `server.ts:155` (tipo de retorno).

**Antes/Depois (builder).**
```ts
// tool-dispatcher.ts:131 ANTES
return { content: [{ type: 'text', text: JSON.stringify(result, null, 2) }] };
// DEPOIS
const STRUCTURED_OUTPUT_TOOLS = new Set(['search','grep','list','retrieve','graph']);
const text = JSON.stringify(result, null, 2);
if (STRUCTURED_OUTPUT_TOOLS.has(toolName) && result && typeof result === 'object' && !Array.isArray(result)) {
  return { content: [{ type:'text', text }], structuredContent: result as Record<string, unknown> };
}
return { content: [{ type:'text', text }] };
```
**⚠ Correção do revisor (crítica):** o payload real de `search` passa por `augmentSearchResults` (injeta
`health`) e carrega `status`/`status_reason`/`indexing`. Um `outputSchema` fechado faria clientes rejeitarem
`structuredContent`. → **adicionar `additionalProperties: true`** no topo de cada output-schema **e** declarar
os opcionais conhecidos (`status`, `status_reason`, `indexing`, `health`). O teste round-trip tem de validar
uma resposta **pós-`augmentSearchResults`**.

```ts
// output-schemas.ts (espelha SearchResult em tools/search-types.ts)
export const SEARCH_HIT_SCHEMA = { type:'object', properties:{
  id:{type:'string'}, score:{type:'number'}, rerankScore:{type:'number'}, collection:{type:'string'},
  location:{type:'string'}, content:{type:'string'}, title:{type:'string'},
  metadata:{type:'object', additionalProperties:true},
}, required:['id','score','collection','content','metadata'] };

export const SEARCH_OUTPUT_SCHEMA = { type:'object', additionalProperties:true, properties:{
  success:{type:'boolean'}, results:{type:'array', items:SEARCH_HIT_SCHEMA}, total:{type:'number'},
  query:{type:'string'}, mode:{type:'string'}, scope:{type:'string'},
  status:{type:'string'}, status_reason:{type:'string'},
  indexing:{type:'object', additionalProperties:true}, health:{type:'object', additionalProperties:true},
  hint:{type:'string'},
}, required:['results','total','query'] };
```

**Critérios de aceite.**
- ListTools traz `outputSchema` **exatamente** nas 5 read-tools; `undefined` nas outras 6 (assert por allowlist, não loop ingênuo — `embedding`/`search_eval` estão no READ_ONLY set mas **não** ganham outputSchema).
- CallTool de `search` retorna `content[0].text` (JSON válido) **e** `structuredContent` deep-equal ao `JSON.parse(text)`.
- `structuredContent` (já passado por `augmentSearchResults`) valida contra o schema.
- `tsc --noEmit` (exactOptionalPropertyTypes ON) + vitest verdes.

**Esforço:** M · **Risco:** baixo-médio (drift schema↔runtime; mitigar com teste round-trip; manter top-level permissivo).

---

### P1.5 — `responseFormat` + paginação + teto global de bytes · Status: ☐

**Objetivo.** Existe cap **por hit** (`DEFAULT_MAX_BYTES_PER_HIT=1500`) mas **não** há teto **por resposta**;
e a paginação é **inconsistente** (`list` tem cursor, `retrieve` tem offset/`hasMore`, `search` não tem nada).
Unificar verbosidade e paginação entre as read-tools (regra de comportamento-compartilhado do CLAUDE.md).

**Arquivos.** `tools/search-types.ts:35-37,95-103,172-190`; `tools/search-shaping.ts:210-260`; `tools/search.ts`; `tool-definitions/search.ts`; `tool-builders/search.ts`; análogos em `list-files-types.ts`/`retrieve-types.ts`/`grep.ts`.

**Depois.**
- **A — `responseFormat`:** add `responseFormat?: 'concise'|'detailed'` em `SearchOptions` + inputSchema + builder. `concise` (default) = trunca corpo p/ `maxBytesPerHit` com marcador `retrieve(...)`; `detailed` = corpo cheio (cap off). (Mantém `summary`/`maxBytesPerHit` como knobs finos.)
- **B — teto global:** `export const DEFAULT_MAX_RESPONSE_BYTES = 24000;` + helper `applyResponseBudget(results, budget)` que dropa hits do fim quando o total de corpo excede o budget (sempre mantém ≥1), reportando `budget_truncated:{dropped, next_offset}`.
- **C — offset no `search`:** add `offset?` (paridade com `retrieve.offset`/`list.cursor`).

**⚠ Correções do revisor.**
1. `shapeHitPayloads` tem **3 branches de saída** (summary/none/truncate), cada um chamando `finalize(...)`.
   Aplicar `applyResponseBudget` **dentro de `finalize`** (não "depois do map") p/ os 3 honrarem o teto, e
   anexar `budget_truncated` ali. `bytesOutShaped` é acumulado **antes** do trim → subtrair os hits dropados
   depois, senão o critério "dropped excluídos de bytesOut" falha.
2. Offset no `search` **não** é drop-in do `retrieve` (que usa scroll): `search` é vetorial. Implementar como
   **slice pós-fusão** de um pool sobre-buscado (fetch `limit+offset`, fatiar **após** a fusão das lanes
   dense/sparse/scratchpad) p/ não dessincronizar as lanes com `includeScratchpad=true`.
3. Adicionar à interface `SearchResponse` (search-types.ts:172-190): `budget_truncated?:{dropped:number; next_offset:number}` e `next_offset?:number` — senão `tsc` (exactOptional) rejeita.

**Exemplo (token delta).** `concise` (3 hits) ≈ 900 tokens; `detailed` (mesma query) ≈ 2.600 (+~190%); o
teto global limita o pior caso a ~6k tokens independente de `limit`, com `budget_truncated.dropped>0` +
`next_offset` p/ o agente paginar em vez de perder hits em silêncio.

**Critérios de aceite.**
- 4 read-tools aceitam `responseFormat`; concise trunca (marcador presente), detailed não; default=concise.
- Resposta cujo corpo somado excede o teto → menos hits + `budget_truncated.dropped>0` + `next_offset` numérico; ≥1 hit sempre.
- `search` aceita `offset` e a página 2 não sobrepõe ids da página 1 (teste **com `includeScratchpad:true`**).
- `responseFormat:detailed` + `maxResponseBytes:0` desliga ambos os caps (escape p/ ler chunk inteiro).
- `search_events.bytes_out` correto após o trim; `tsc` + vitest verdes (testes do boundary de drop e concise/detailed).

**Esforço:** M · **Risco:** médio (offset toca a query path; respeitar branch/tenant/basePoints + lane scratchpad). **Dependência:** P1.4 (o `outputSchema` precisa refletir os campos novos).

---

### P1.6.a — CLI `wqm admin search-latency` (p50/p95/p99) · Status: ☐

**Objetivo.** `search_events.latency_ms` é gravado por linha mas **nada computa percentis** — os p50/p95
só vivem na memória do projeto. Adicionar comando, espelhando a estrutura de `wqm admin token-savings`.

**Arquivos.** novo `src/rust/cli/src/commands/admin/search_latency.rs`; `admin/mod.rs:27`(`mod`),`~:255`(variante),`~:359`(dispatch); reusar `token_savings.rs` (`parse_window:139`, `open_state_db:171`); coluna em `search_events_schema.rs:20`.

**⚠ Correções do revisor.**
1. **SQL:** a versão inline-CASE do rascunho não é válida (mistura window agg dentro de GROUP BY; `INT` não
   existe — é `INTEGER`). Usar **2-CTE join** (nearest-rank, 1-based, clampado) e decodificar percentis como
   `Option<i64>`:
```sql
WITH base AS (
  SELECT {col} AS grp, latency_ms AS v FROM search_events
  WHERE latency_ms IS NOT NULL AND ts >= datetime('now', ?1) {AND project_id = ?2}
),
counts AS (SELECT grp, COUNT(*) AS n FROM base GROUP BY grp),
ranked AS (SELECT grp, v, ROW_NUMBER() OVER (PARTITION BY grp ORDER BY v) AS rn FROM base)
SELECT r.grp, c.n AS calls,
  MAX(CASE WHEN r.rn = MAX(1, CAST(ROUND(0.50*(c.n-1)) AS INTEGER)+1) THEN r.v END) AS p50,
  MAX(CASE WHEN r.rn = MAX(1, CAST(ROUND(0.95*(c.n-1)) AS INTEGER)+1) THEN r.v END) AS p95,
  MAX(CASE WHEN r.rn = MAX(1, CAST(ROUND(0.99*(c.n-1)) AS INTEGER)+1) THEN r.v END) AS p99,
  (SELECT MAX(v) FROM base b WHERE b.grp = r.grp) AS maxv
FROM ranked r JOIN counts c ON c.grp = r.grp
GROUP BY r.grp, c.n ORDER BY calls DESC;
```
   `{col}` vem de um whitelist `tool|actor|op` (sem injeção). Fixar off-by-one com teste (100 linhas 1..100 → p50≈50/51, p95≈95/96, p99≈99/100).
2. **Render:** `print_table_auto<T: Tabled + ColumnHints>` exige `impl ColumnHints for LatencyRow` (espelhar `token_savings.rs:54-61`) — sem isso **não compila**. Definir também `print_table`/`print_json`. Tornar `parse_window`/`open_state_db` `pub(super)`.

**Exemplo.**
```
$ wqm admin search-latency --window 7d --group-by tool
 Group        Calls   p50 ms   p95 ms   p99 ms   max ms
 mcp_qdrant   1,204       25      280      612    1,840
```

**Critérios de aceite.**
- Roda sem flags (tool, 7d). `--group-by actor|op` muda a coluna; outro valor → erro claro (whitelist).
- Percentis em SQLite puro via ROW_NUMBER/COUNT, validados por fixture; `latency_ms IS NULL` excluído.
- `--project`/`--json` funcionam; vazio imprime info/`[]`; DB READ_ONLY + busy_timeout.

**Esforço:** M · **Risco:** nearest-rank é chato (fixar com teste); blast radius baixo (read-only, subcomando novo).

---

### P1.6.b — Reconstrução de cadeia de tool-calls · Status: ☐

**Objetivo.** Entender sequências/reformulações do agente. **Achado-chave (verificado):** `parent_event_id`
e `op='followup'` **nunca são escritos** por nenhum call site → as colunas `had_followup`/`had_escalation`
da view v38 `token_savings` são **sempre false em produção**. O único sinal de encadeamento populado é
`session_id` (+ `ts`).

**Arquivos.** `clients/search-event-queries.ts:24,86`; call sites `tools/{search,grep,retrieve}.ts`, `list-files/index.ts`, `search-exact.ts` (nenhum passa `parentEventId`); `schema_version/v38.rs:71-83` (view); `search_events_schema.rs:11,23`.

**PARTE 1 (entrega já — read-only):** SQL que reconstrói a cadeia por `session_id`+`ts`, classificando
reformulação por diffing de linhas adjacentes (mesmo tool, gap curto, query mudou). Expor como
`wqm admin search-sessions --chain <session_id>`.
```sql
WITH ordered AS (
  SELECT id, ts, tool, op, query_text, result_count, latency_ms, parent_event_id,
    LAG(query_text) OVER w AS prev_query, LAG(op) OVER w AS prev_op,
    (julianday(ts) - julianday(LAG(ts) OVER w))*86400.0 AS gap_s
  FROM search_events WHERE session_id = ?1 WINDOW w AS (ORDER BY ts))
SELECT id, ts, tool, op, substr(COALESCE(query_text,''),1,48) AS query, result_count, latency_ms,
  CASE WHEN prev_op=op AND op IN ('search','search_exact') AND gap_s<60 AND prev_query IS NOT query_text
       THEN 'reformulation'
       WHEN parent_event_id IS NOT NULL THEN 'escalation' ELSE 'root' END AS link
FROM ordered ORDER BY ts;
```

**PARTE 2 (torna escalation real — TS, sem migração):** cabear `parentEventId` nos call sites
(`retrieve`/`open`/`expand` ← eventId da `search` que produziu o hit). `parent_event_id` já existe e a view
já o lê. **Difícil:** o servidor é stateless por call → precisa de um mapa session-scoped "last-search-id";
errar isso mis-atribui pais entre sessões concorrentes. **Manter PARTE 2 separada/opcional.**

**Critérios de aceite.**
- `current_state` documenta que `parent_event_id`/`op='followup'` não são escritos (é achado, não invenção).
- PARTE 1: `--chain` reconstrói por `session_id`+`ts`, classifica reformulação; CTE recursivo de `parent_event_id` fornecido mas marcado inerte até PARTE 2.
- PARTE 2 (se feita): call sites passam `parentEventId`; teste insere search+retrieve(parent=search) e assere `had_escalation=1` na view v38. **Sem migração nova.**

**Esforço:** M · **Risco:** PARTE 1 segura; PARTE 2 é o threading de ids (manter opcional). **Dependência:** P1.6.a.

---

### P1.6.c — Proxy de auto-approve (inter-call gap) · Status: ☐

**Objetivo.** Auto-approve é **inobservável** server-side (a decisão é client-side, antes do call chegar; um
call rejeitado nunca chega). **Não** criar coluna falsa. Entregar **proxy derivado** de `session_id`+`ts`.

**Arquivos.** `telemetry/metrics.ts:33-46` (`toolInvocations`), `:64-81` (cacheHits/Misses **mortos** — não confundir com sinal); `search_events_schema.rs` (sem coluna de approval); `clients/search-event-queries.ts:10-32`.

**Depois (sem migração):** view/flag `--approve-proxy` no comando P1.6.a:
```sql
WITH gaps AS (SELECT session_id, tool,
  (julianday(ts) - julianday(LAG(ts) OVER (PARTITION BY session_id ORDER BY ts)))*86400.0 AS gap_s
  FROM search_events)
SELECT tool, COUNT(*) AS chained_calls,
  ROUND(100.0*SUM(CASE WHEN gap_s < 1.5 THEN 1 ELSE 0 END)/COUNT(*),1) AS fast_followup_pct
FROM gaps WHERE gap_s IS NOT NULL GROUP BY tool;
```
Gap < `AUTO_APPROVE_GAP` (1.5s, constante nomeada/tunável) ≈ cliente auto-aprovou (humano manual adiciona
segundos). **Coluna nova `approval_mode`** só se o lado cliente entrar em escopo (senão vira outra coluna
NULL-eterna como cacheHits); se for, ADD COLUMN simples (padrão v38) + campo em `SearchEventInput` **+** no
objeto `request` literal (`:56-86`) **+** no gRPC/daemon (correção do revisor — a edição da interface sozinha não basta).

**Critérios de aceite.**
- Docs deixam explícito que o servidor não observa auto-approve; proxy rotulado como proxy.
- Proxy só de `session_id`+`ts` (sem migração); threshold = constante documentada.
- `current_state` nota os contadores cacheHits/Misses mortos.

**Esforço:** S · **Risco:** proxy pode enganar (LLM "pensando" infla o gap); risco maior é **super-afirmar** — não vender proxy como taxa real. **Dependência:** P1.6.a.

---

### P2.8 — Multi-query na perna densa (experimental) · Status: ☐

**Objetivo.** A perna densa é **single-shot** (1 embedding/search). A única expansão existente é **sparse-only**
(`expandSparseWithTags`). Adicionar lane densa multi-query **que nunca concatena texto** — N paráfrases,
embeddings separados, RRF-merge.

**Arquivos.** `tools/search-helpers.ts:274-278` (`generateEmbeddings` single-shot), `:771-807` (`QUERY_TOKEN_SYNONYMS`); `tools/search.ts:117-152` (`prepareEmbeddings`); `tools/search-qdrant.ts:43-68,178-213` (`searchDense`, `applyRRFFusion` keyed `${collection}:${id}`); `clients/daemon-client/service-methods.ts:68` (`embedText`).

**Depois (novo módulo `search-multiquery.ts`, env-gated `WQM_MULTIQUERY_N`, default 1=off):**
1. Reformulador determinístico v1 (original + forma de identificador via `extractSupplementalNeedles` + forma EN-normalizada via `QUERY_TOKEN_SYNONYMS`); cap N=3. (LLM-backed depois, mesma interface.)
2. Embeddar **cada** variante (`embedText` por variante) → `number[][]`. Nunca `join`.
3. `searchDense` por vetor → N rankings densos.
4. **Novo** helper `applyMultiRRF(rankings)` soma `1/(RRF_K+rank+1)` entre os N rankings densos → alimenta a
   fusão dense-vs-sparse existente como a **única** perna "semantic" (preserva `KEYWORD_WEIGHT`/`SPARSE_ONLY_WEIGHT`).

**Critérios de aceite.**
- `WQM_MULTIQUERY_N` unset/1 → byte-idêntico ao hoje (`embedText` chamado 1×, teste fixa).
- `N=3` → `embedText` ≤3× e `applyMultiRRF` recebe ≥2 rankings; teste garante que nenhuma variante é join de duas queries (guard anti token-concat).
- `applyMultiRRF` puro, com cobertura (1 ranking = identidade).
- Eval gate (registrar no PR, não bloqueia merge): 46 queries N=1 vs N=3 — semantic top1/top3/top10/recall/mrr + latência + `byCategory`. Lane fica OFF; ON só se recall@10 sobe sem regredir top1 >1pp.

**Esforço:** L · **Risco:** médio-alto (N× latência/carga — manter OFF/capado; paráfrases ruins diluem; não
reescrever `applyRRFFusion`, alimentá-la via `applyMultiRRF`). Pode ajudar o bucket fraco PT→EN. **Dependência:**
nenhuma funcional (medir após P1.7 só p/ baseline estável).

---

### P2.9 — Experimento grep-vs-vetor + inline-vs-file-based · Status: ☐

**Objetivo.** Testar no **golden set próprio** o achado externo (grep ≳ vetor p/ agentes; entrega
inline vs arquivo importa) — com a ressalva honesta de que o estudo externo é QA conversacional, não código.

**Arquivos.** `tools/search-eval.ts:190-270`; `tool-definitions/search-eval.ts:5-73`; `scripts/benchmark-data/semantic-search-quality.yaml`; `benchmarks/semantic-search.ts:76-94,687-731`; `docs/testing/semantic-search-benchmarking.md`.

**Desenho (matriz sobre o set existente):**
- **Eixo 1 — método (já suportado, 1 run):** comparar `modes.exact` (grep/FTS5) vs `modes.semantic` vs `modes.hybrid`. Hipótese: grep vence `sym-` (identificador), vetor vence `impl-`/`pt-` (conceitual).
- **Eixo 2 — forma de entrega:** `summary:false` (inline) vs `summary:true` (só path). Como a forma **não** muda ranking, hit@k/recall/MRR ficam **idênticos** (essa invariância é o achado); o delta é custo em bytes.

**⚠ Correções do revisor.**
1. `byCategory` (search-eval.ts:206-221) só é construído p/ `['semantic','hybrid']` — **exclui `exact`**. Logo o per-categoria grep-vs-vetor **não** é mensurável hoje. Escolher: (a) estender o loop p/ incluir `exact`, **ou** (b) restringir o per-categoria a semantic-vs-hybrid e derivar o contraste grep via `perQuery firstRelevantRank`. Declarar qual no critério.
2. Passthrough de `summary` exige 4 hops: inputSchema (`search-eval.ts`) → `runSearchEval` (`:190-204`) → `SemanticSearchBenchmarkRunConfig` (`semantic-search.ts:~93`) → `buildSearchOptions` (`:~728`, `if (config.summary!==undefined) options.summary=config.summary`).
3. Exemplo: linha summary tem `bytes_out ≈ 0` (summary zera `bytesOutShaped`, search-shaping.ts:230), **não** `N/5`; mencionar que o agente paga o `retrieve()` de follow-up.

**Critérios de aceite.**
- Recipe em `docs/testing/semantic-search-benchmarking.md` com as chamadas exatas e o mapa de campos→hit@1/3/10, recall@10, MRR por método.
- Eixo 1 roda sem mudança de código (1 run dá os 3 métodos). Per-categoria conforme a escolha (a)/(b).
- Eixo 2: passthrough `summary`; teste assere métricas de ranking **idênticas** summary true/false, com `bytes_out` diferente.
- Writeup grava por célula valores + `datasetSource`/`queryCount` e taggea `telemetryActor` distinto.
- Ressalva honesta: estudo externo é QA conversacional; resultado é específico deste corpus; liderar com recall@10/MRR (n pequeno por categoria → hit@1 ruidoso).

**Esforço:** M · **Risco:** baixo (medição; único código é o passthrough aditivo de `summary`). Não super-ler 46 casos.

---

## 5. Estratégia de validação

| Camada | Como |
|--------|------|
| Build TS | `tsc --noEmit` com `exactOptionalPropertyTypes` ON (vide memória TS-host) |
| Testes TS | vitest (precisa do napi addon — extrair do image docker); novos testes por item |
| Build/teste Rust | alvo `validate` no container (`--target validate`, clippy `--tests`); `cargo test` nos pacotes tocados |
| Retrieval | `mcp__workspace-qdrant__search_eval` A/B no set de 46 (obrigatório p/ P1.7; eval-gate p/ P2.8/P2.9) |
| Deploy | `make redeploy` (rebuild + recreate mcp+memexd); smoke: ListTools + `wqm admin search-latency` |
| Pós-deploy | validar no `search_events` que latência/telemetria fluem |

---

## 6. O que NÃO fazer (claims refutados na verificação)

- ❌ **Fan-out multi-agente bate single-agent (90.2%)** — refutado (1-2). Usar orchestrator-workers como
  *estrutura*, não como ganho provado para retrieval. → por isso **P3 (orquestração) fica adiado**: sem
  demanda real e sem evidência. Se um dia houver, a forma certa é expor **primitivas de escopo**
  (filtros tenant/branch/collection + budget) p/ um orquestrador particionar fan-out sem overlap — não um
  orquestrador embutido no MCP.
- ❌ **Evaluator-optimizer = base da verificação adversarial** — refutado (0-3).
- ❌ **RAG hierárquico multi-tool bate RAG flat (94.5% HotpotQA)** — refutado (1-2).
- ⚠ **Tool Search Tool / progressive disclosure (−85% tokens)** — verdadeiro só p/ 10+ tools / >10k tokens de
  defs; servidor único de ~11 tools é o **caso marginal**. **Não priorizar** disclosure progressivo aqui.

---

## 7. Fontes externas (primárias, verificadas)

- MCP spec 2025-06-18 — https://modelcontextprotocol.io/specification/2025-06-18/server/tools
- Writing tools for agents — https://www.anthropic.com/engineering/writing-tools-for-agents
- Advanced tool use — https://www.anthropic.com/engineering/advanced-tool-use
- Building effective agents — https://www.anthropic.com/research/building-effective-agents
- Code execution with MCP — https://www.anthropic.com/engineering/code-execution-with-mcp
- grep-vs-vector (arXiv 2605.15184) — https://arxiv.org/pdf/2605.15184
- A-RAG multi-granularity (arXiv 2602.03442) — https://arxiv.org/html/2602.03442v1
- Perplexity AI-first search — https://research.perplexity.ai/articles/architecting-and-evaluating-an-ai-first-search-api

---

## 8. Execução em `/batch` — agrupamento file-disjoint (onda 1)

Três workers **mutuamente disjuntos em arquivos** → merge limpo, podem rodar em paralelo:

| Worker | Branch | Itens | Arquivos | Validação |
|--------|--------|-------|----------|-----------|
| **A — tool-definitions** | `claude/agent-ergo-tool-definitions` | P0.1+P0.2+P0.3 | `tool-definitions/{store,scratchpad,workspace-index,rules}.ts`, `tool-dispatcher.ts:45`, `tests/tool-definitions.test.ts` | `tsc --noEmit` + tool-definitions test |
| **B — observability** | `claude/agent-ergo-observability` | P1.6.a + P1.6.b(P1) + P1.6.c | `src/rust/cli/.../admin/{search_latency.rs(new),mod.rs,token_savings.rs}` (100% Rust) | `cargo check -p` (best-effort) + unit test do percentil |
| **C — reranker** | `claude/agent-ergo-reranker-default` | P1.7 | `tools/search-helpers.ts:1205`, `tools/search-eval.ts:187`, `search-types.ts:85-88`, `tool-definitions/search-eval.ts` | `tsc` + teste do default; A/B `search_eval` no deploy |

**Onda 2 (depois do merge da onda 1 — conflitam com a onda 1):** P1.4+P1.5 (tocam `tool-dispatcher.ts`↔A e `search-types.ts`↔C) e P2.8/P2.9 (tocam `search-helpers`/`search-eval`↔C). Rodar como segundo `/batch` de 1–2 workers.

> **Pré-requisito de ambiente:** o `/batch` por worktree do harness só funciona com **Claude Code lançado de dentro do WSL** (`wsl -- claude` na pasta ext4). No host Windows, a criação de worktree falha com *dubious ownership* (git Windows vs repo ext4; `git config safe.directory` é deny-ruled). Alternativa no host: criar os worktrees manualmente via `wsl.exe -d ubuntu-24.04 -- bash -lc 'cd <repo> && git worktree add ...'` e despachar agentes escopados a cada path.

---

## 9. Execução em `/batch` — onda 2 (plano)

Quatro itens restantes. Diferente da onda 1, **não são todos file-disjoint** — `tools/search.ts` é editado por P1.5 (offset/paginação) **e** por P2.8 (lane multi-query), e P1.5 depende de P1.4. Matriz de conflito (arquivos compartilhados):

| | D (P1.4+P1.5) | E (P2.8) | F (P2.9) |
|---|---|---|---|
| **D** | — | ⚠ conflita (`tools/search.ts`) | ✅ disjunto |
| **E** | ⚠ conflita | — | ✅ disjunto |
| **F** | ✅ disjunto | ✅ disjunto | — |

→ **D e E não podem rodar em paralelo** (ambos mexem em `tools/search.ts`). `D∥F` e `E∥F` são seguros.

### Workers

| Worker | Branch | Itens | Arquivos principais | Validação |
|--------|--------|-------|---------------------|-----------|
| **D — structured output + token economy** | `claude/agent-ergo-output-schema` | **P1.4 → P1.5** (sequencial dentro do worker; P1.5 depende do P1.4) | `tool-definitions/{index,search,grep,list,retrieve,graph}.ts`, novo `output-schemas.ts`, `tool-dispatcher.ts`, `server.ts`, `tools/{search,search-shaping,search-types}.ts`, `tool-builders/search.ts`, `{list-files,retrieve}-types.ts`, `tools/grep.ts` | `tsc` + vitest: round-trip `outputSchema` vs resposta real **pós-`augmentSearchResults`** (tem `health`/`indexing` → `additionalProperties:true`); boundary do byte-budget; paginação com `includeScratchpad:true` sem desync de lane |
| **F — experimento grep-vs-vetor** | `claude/agent-ergo-retrieval-experiment` | P2.9 | `tools/search-eval.ts`, `tool-definitions/search-eval.ts`, `benchmarks/semantic-search.ts`, `docs/testing/semantic-search-benchmarking.md` | `tsc` + teste: métricas de ranking **idênticas** `summary` true/false; estender `byCategory` p/ incluir `exact`; recipe documentado |
| **E — multi-query na perna densa** | `claude/agent-ergo-multiquery` | P2.8 (experimental, OFF por default) | `tools/{search-helpers,search,search-qdrant,search-expansion}.ts`, novo `search-multiquery.ts`, `daemon-client/service-methods.ts` | `tsc` + teste (`embedText` 1× quando `WQM_MULTIQUERY_N` unset; `applyMultiRRF` puro; guard anti token-concat) + **eval-gate** `search_eval` N=1 vs N=3 |

### Sequenciamento

- **Onda 2a (paralelo, disjunto):** **D ∥ F**. D é o ganho estrutural (maior valor); F é medição quase sem código.
- **Onda 2b (após D mergear):** **E** — rebase sobre o D mergeado (conflita em `tools/search.ts`); rodar o eval-gate antes de adotar. Fica OFF por default de qualquer forma (experimental, `L`, ROI incerto).

### Gates específicos

- **D:** P1.4 deve emitir `structuredContent` validável **mantendo** o fallback `TextContent`; P1.5 é aditivo (não remover `summary`/`maxBytesPerHit`). Sem eval de retrieval (não muda ranking).
- **F:** medição pura — o achado é a **invariância** das métricas de ranking entre `summary` true/false; reportar `bytes_out` (summary = 0, não N/5). Ressalva honesta: estudo externo é QA conversacional, não código.
- **E:** só **adota** (liga por default) se recall@10 subir sem regredir top1 >1pp no `search_eval`; senão fica no código atrás de `WQM_MULTIQUERY_N` (default 1 = off).

### Pré-requisito de dispatch

Mesmo da onda 1: para `/batch` paralelo **real**, lançar Claude de dentro do WSL (`wsl -- claude` na pasta ext4); no host Windows, sequencial via `wsl.exe` git (como foi feito aqui). PRs sempre com `gh pr create --repo alkmimm/...` (trap do #96). Commits/PR bodies via `-F`/`--body-file`.

### Status (onda 2)

- **Onda 2a MERGED + deployed (sequencial via wsl.exe; `make mcp-rebuild`, stack healthy):** F → [#140](https://github.com/alkmimm/workspace-qdrant-mcp/pull/140) (P2.9 harness: byCategory+exact, summary passthrough, recipe); D → [#141](https://github.com/alkmimm/workspace-qdrant-mcp/pull/141) (**P1.4** outputSchema + structuredContent). Ambos `tsc`+vitest verdes; live em `main`.
- **Split do D:** **P1.5** (responseFormat/paginação/byte-budget) saiu para follow-up próprio (D2) — toca o response-path do `search` e adiciona offset no caminho vetorial (correção de paginação com lane scratchpad precisa teste de integração no índice vivo), então merece PR validado à parte.
- **P1.5 A+B → [#142](https://github.com/alkmimm/workspace-qdrant-mcp/pull/142) MERGED + deployed** (D2: `responseFormat` concise/detailed + teto global de bytes `budget_truncated`; `tsc`+56 testes; `make mcp-rebuild`, stack healthy, live em `main`).
- **P1.5 parte C → [#143](https://github.com/alkmimm/workspace-qdrant-mcp/pull/143) MERGED + deployed** (offset/paginação: over-fetch + slice pós-fusão via `paginateRanked`, lane scratchpad só na página 1, `next_offset`; `tsc`+56 testes). **P1.5 completo** (A+B+C). Spot check ao vivo: `next_offset` já aparece na resposta (servidor roda #143 ✓) e `summary` funciona; mas passar `offset` de **entrada** exige o cliente MCP re-ler o ListTools — o schema em cache pré-deploy descarta o param novo (`next_offset:10` provou que o servidor viu offset=0). Reconexão do MCP (usuário) p/ validar o round-trip completo; não é bug de código.
- **Pendente:** só **E** (P2.8 multi-query dense leg, experimental/`L`, eval-gated, OFF por default) — adiado por escolha do usuário.


## 10. Correções pós-revisão (review geral + últimos 20 commits, 2026-06-22/23)

Revisão adversarial (24 agentes, dimensões → find → refute-by-default) sobre a
onda 1+2. Veredito: 12 achados confirmados, 4 refutados, **zero achados de
segurança**. As correções saíram em 4 PRs file-disjuntos (sem conflito entre si
nem com as PRs já mergeadas):

| PR | Escopo | Itens | Validação |
|----|--------|-------|-----------|
| [#144](https://github.com/alkmimm/workspace-qdrant-mcp/pull/144) | `cli` (Rust) | **ts-format window bug** (reportado pelo usuário): `query_aggregates`/`query_latency` filtravam `ts >= datetime('now', ?1)` (espaço, sem `Z`) contra a coluna ISO-8601 `strftime('%Y-%m-%dT%H:%M:%fZ','now')`; como `'T'`(0x54) > `' '`(0x20) lexicamente, qualquer linha do mesmo *dia* passava o `>=` independentemente da hora → over-inclusão em janelas curtas. Fix: `strftime('%Y-%m-%dT%H:%M:%fZ','now',?1)` nos dois arquivos. **n1**: comentário do caveat nearest-rank n≤2 (ROUND half-away → p50 no maior dos 2). | repro Python + teste de regressão inserindo no formato `…Z` real |
| [#145](https://github.com/alkmimm/workspace-qdrant-mcp/pull/145) | `tool-dispatcher.ts`, `store.ts` | **R0 — regressão do #136**: o type-guard `throw` em `dispatchStore` quebrou 6 testes de integração. Trocado por **inferência** (`libraryName`/`forProject`→library, `path`→project, `url`→url; senão erro claro). `required:['type']` removido do inputSchema (o servidor não valida `required`; a inferência+guard é o enforcement real). | `tsc` + 17 testes |
| [#146](https://github.com/alkmimm/workspace-qdrant-mcp/pull/146) | `search*.ts` | **M1 (major, triplo-confirmado TS-1=RC-1=td-02)**: byte-budget (P1.5 B) roda *depois* do `next_offset = offset+limit` → hits de código dropados pelo budget ficavam **inalcançáveis** na próxima página. Fix: `reconcileNextOffset()` puro recomputa `next_offset = offset + (code hits kept)`. **TS-2**: `total` recomputado p/ `kept.length` quando o budget dropa. **m2**: caminho `exact` passou a honrar `offset`/`next_offset` (paridade com o vetorial). | `tsc` + 54 testes (inclui unit de `reconcileNextOffset`) |
| [#147](https://github.com/alkmimm/workspace-qdrant-mcp/pull/147) | `output-schemas.ts`, `grep.ts`, test | **n2**: `success` no `SEARCH_OUTPUT_SCHEMA` (paridade com os irmãos). **m5**: `next_token`(+`projectPath`/`basePath`/`message`) no `LIST_OUTPUT_SCHEMA` (paginação do `list` era indescobrível). **m4**: documentado *por que* grep/list não rodam o byte-budget do search. **m7**: teste de regressão do soft-default de rerank (P1.7) via `applied`. | `tsc` + 17 testes |

### Refutados na revisão (não-bugs — não reinvestigar)
- "Graph inflation", flapping de branch, superioridade de fan-out, hierarchical-RAG (já em §6). Nenhum reaberto.

### Nits adiados (rastreados, não perdidos)
- **n3** — hoist de comentário de rerank em `search-helpers.ts`: pulado p/ não conflitar com #146 (mesmo arquivo) + veredito "uncertain it's even an issue".
- **list input `cursor`/`pageSize`**: expor no inputSchema do `list` exige verificar `buildListOptions`; mudança separada.
- **m6** — teste de gating "scratchpad só na página 1": nit de cobertura.
- **exec_queue.rs:200** (`updated_at < datetime('now','-'||?1||' days')`): mesma classe do ts-bug, mas **benigno** (under-delete de leases velhos, não over-include) → spawn_task `task_631c224b`.

### Pendente de merge + deploy
- Mergear #144, #145, #146, #147 (todas file-disjuntas; ordem livre) e depois `make redeploy`. **m5/n2 só ficam visíveis ao agente após o cliente MCP reconectar** (cache de ListTools), como já observado no #143.


