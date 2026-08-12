# Contextual chunk headers (LLM) — plano de implementação

**Data:** 2026-08-10
**Escopo:** enriquecer o texto de embedding denso com uma frase de contexto gerada
por um LLM local na GPU, no *index time*. Zero custo de latência no read path.
**Não-escopo (fases futuras):** query shaping, compressão/orientação no retorno.

---

## 1. Contexto — o que já existe

Antes de qualquer código novo, o estado real medido em 2026-08-10:

| Peça | Estado | Onde |
|---|---|---|
| Embedder denso GPU | `nomic-ai/CodeRankEmbed` 768d via Infinity | `docker-compose.yml:661` |
| Cross-encoder rerank GPU | `BAAI/bge-reranker-v2-m3`, **live**, w=0.10 | `docker/.env:104`, [rerank.rs](src/rust/daemon/core/src/embedding/rerank.rs) |
| Header contextual determinístico | **já implementado** (path/símbolo/docstring) | [dense_text.rs:28](src/rust/daemon/core/src/strategies/processing/file/chunk_embed/dense_text.rs:28) |
| VRAM | 11.885 / 16.303 MiB usados → **4.0 GB livres** | `nvidia-smi` |
| Schema version (state.db) | `CURRENT_SCHEMA_VERSION = 48` | [schema_version/mod.rs:181](src/rust/daemon/core/src/schema_version/mod.rs:181) |
| Fingerprint de chunking | `CHUNKER_LOGIC_VERSION = 3` | [fingerprint.rs:38](src/rust/daemon/core/src/tree_sitter/chunker/fingerprint.rs:38) |

### O invariante que torna isto barato

O doc-comment de `dense_text.rs` já estabelece e testa a garantia central:

> Only the DENSE embedding sees this text. The stored payload `content`, the
> sparse/BM25 vector, and the chunk content hash all keep using the raw chunk.

Consequências que sustentam o plano inteiro:

1. **O agente nunca vê o texto gerado.** Uma alucinação do LLM degrada recall,
   nunca produz conteúdo falso na resposta. Isso é o que permite usar um modelo
   pequeno sem risco de correção.
2. **O content hash não muda.** Não há churn de hash → não há loop de re-embed,
   que era o risco principal na avaliação inicial.
3. **O ponto de inserção é uma função só**, chamada de um lugar só
   ([chunk_embed/mod.rs:81](src/rust/daemon/core/src/strategies/processing/file/chunk_embed/mod.rs:81)).

---

## 2. Decisões de design (e o porquê)

**D1 — Estender `build_dense_embedding_text`, não substituí-la.**
O header vira quatro partes; a frase do LLM é a última linha antes do corpo cru:

```text
{relative_path} | {parent}::{symbol} ({kind})
{docstring excerpt}
{frase de contexto do LLM}          ← NOVO

{chunk cru — verbatim, inalterado}
```

Se o LLM não responder, o header é **exatamente o de hoje**. Fail-open é o
comportamento default, não um caminho de exceção.

**D2 — Cache persistente keyed por hash, não regeneração.**
O header não é armazenado no payload, então sem cache toda re-indexação
repagaria a geração. Tabela nova em `state.db`, chave:

```
ctx_key = SHA256(chunk_content_hash | file_context_digest | prompt_version | model)[:32]
```

`chunk_content_hash` já é calculado em
[chunk_embed/mod.rs:262](src/rust/daemon/core/src/strategies/processing/file/chunk_embed/mod.rs:262).
Incluir `prompt_version` e `model` na chave é obrigatório: sem eles, mudar o
prompt reutiliza silenciosamente contexto velho — o modo de falha mais caro
possível, porque é invisível.

Efeito colateral bom: chunks idênticos entre branches compartilham entrada de
cache de graça, casando com o dedup de branch que já existe.

**D3 — NÃO bumpar `CHUNKER_LOGIC_VERSION`.**
As fronteiras dos chunks não mudam. Bumpar forçaria um re-chunk de todo o corpus
para obter um efeito que só precisa de re-embed, e conflataria dois conceitos
distintos no mesmo campo. O lever de rollout é o **forced reembed por tenant**
(`/admin/api/projects/reembed`), que já existe e é o mecanismo correto.

**D4 — Denso apenas na fase 1.**
A Contextual Retrieval da Anthropic põe o contexto no embedding *e* no BM25. Aqui
o BM25 fica de fora na primeira fase para preservar o invariante de `dense_text.rs`
e manter o A/B com uma variável só. Fase 5 avalia estender ao esparso — com
medição própria, porque muda o espaço de termos.

**D5 — Contexto de arquivo = outline de símbolos, não o arquivo inteiro.**
A receita canônica manda passar o documento completo no prompt. Para um corpus de
código isso estoura contexto em arquivos grandes e desperdiça prompt. Como o
tree-sitter já produz símbolos por chunk, o prompt recebe:

- path relativo + linguagem
- outline ordenado dos símbolos do arquivo (nome + kind)
- docstring de módulo, se houver
- o chunk alvo

Cap duro de ~2k tokens. Mais barato, mais determinístico e reaproveita dado que já
está em memória.

**D6 — Gerar na ordem do arquivo, com prefixo estável.**
O outline é idêntico para todos os chunks de um arquivo. Processando os chunks de
um arquivo consecutivamente e mantendo o prefixo byte-idêntico, o cache de prefixo
KV do llama.cpp reaproveita o prompt inteiro entre chunks. Isso é a diferença entre
o backfill levar ~1h e levar várias horas — não é micro-otimização.

**D7 — llama.cpp, não vLLM.**
VRAM apertada (4 GB), e o transporte é o mesmo `/v1/...` OpenAI-compatible que
`OpenAiCompatibleProvider` e `RemoteReranker` já falam. GGUF dá controle fino de
VRAM (`--n-gpu-layers`) e degradação graciosa. vLLM tem overhead de reserva que
não cabe aqui.

**D8 — SLM, com o tamanho como parâmetro medido (não escolha fixa).**

O que este plano pede é um **SLM**, não um LLM: escrever uma frase situando um
chunk, dado o outline do arquivo, é extração e compressão — não raciocínio. É a
classe de tarefa onde modelos de 0,5–3B ficam perto da paridade com modelos de
fronteira. O tamanho pequeno não é concessão à VRAM; é a ferramenta certa.

Fixar 3B seria chute. Pontos de medição:

| Modelo | GGUF Q4_K_M | Observação |
|---|---|---|
| `Qwen2.5-Coder-0.5B-Instruct` | ~0,4 GB | piso; testa se a tarefa é trivial mesmo |
| `Qwen2.5-Coder-1.5B-Instruct` | ~1,1 GB | candidato provável |
| `Qwen2.5-Coder-3B-Instruct` | ~2,0 GB | teto dentro do orçamento de VRAM |

Por que o tamanho importa mais do que parece:

- **Escala (R4).** Corpus completo = 318,8k pontos. 1,5B em vez de 3B corta o
  backfill quase pela metade. Se o delta de qualidade for nulo, escolher 3B custou
  horas de GPU por nada.
- **VRAM.** 1,1 GB vs 2,0 GB, dentro de 4 GB livres, é a diferença entre folga e
  aperto — e o aperto é o que força o workaround de mover o embedder para a CPU.
- **Fase 5b.** Query shaping roda em query time; lá o tamanho deixa de ser
  otimização e vira requisito, provavelmente apontando para algo ainda menor.

**Trocar o modelo depois deve ser barato.** Requisitos de implementação:

1. Modelo vem de `WQM_CONTEXT_LLM_MODEL` (env → config), **nunca hardcoded**;
   trocar não pode exigir rebuild de imagem — só `.env` + recreate do serviço.
2. O GGUF é montado por volume/env no serviço `llm-gpu`, não assado na imagem.
3. **A invalidação já é automática:** `model` faz parte do `ctx_key` (D2), então
   trocar o modelo regenera em vez de reusar contexto do modelo anterior. É a
   propriedade que torna o sweep seguro.
4. Corolário de custo: cada ponto do sweep é uma população de cache separada. Um
   sweep de 3 modelos sobre este tenant (33,9k chunks) ≈ 3× geração ≈ ~1,5–2 h.
   **Varrer sobre um subconjunto ou sobre um tenant pequeno**, nunca sobre o
   corpus completo.

---

## 3. Fases

Cada fase é **uma PR** atômica no fork `alkmimm/workspace-qdrant-mcp`
(nunca no upstream `ChrisGVE/...`). Fases 1–3 são deploy-seguras com a feature
desligada; nenhum comportamento muda até a fase 4.

---

### Fase 0 — Medir e recuperar VRAM (sem código)

**Objetivo:** saber se cabe um terceiro modelo antes de escrever qualquer linha.

1. Baseline de VRAM com o embedder parado, para separar o que é Infinity do que é
   o compositor do Windows:
   ```bash
   docker compose --env-file docker/.env -f docker-compose.yml stop embeddings-gpu && sleep 5 && nvidia-smi --query-gpu=memory.used,memory.free --format=csv
   ```
2. Religar e medir de novo. Delta = custo real do Infinity. Os pesos fp16 dos dois
   modelos somam ~2,3 GB; se o delta for muito maior, é allocator/batch e vale
   capar o batch do Infinity no `command:` do compose.
3. Validar que um runtime CUDA novo enxerga o sm_120 (Blackwell). O compose já
   documenta que TEI não cobre esse card — a mesma armadilha vale aqui:
   ```bash
   docker run --rm --gpus all ghcr.io/ggml-org/llama.cpp:server-cuda --list-devices
   ```
4. Baseline de qualidade de busca **antes** de qualquer mudança, via `search_eval`
   com o dataset bundled do tenant. Guardar recall@10 / MRR — é o denominador de
   toda a fase 4.

**Critério de aceite:** ≥ 3,0 GB livres com o stack completo de pé, sm_120
reconhecido, baseline registrado.

**Se falhar:** existe uma saída elegante — durante o backfill em massa, apontar o
embedding para o sidecar CPU (`embedding.fallback_base_url`, que já existe) e
entregar a GPU inteira ao LLM de contexto. O backfill é offline; a latência do
embedder não importa nessa janela.

---

### Fase 1 — Sidecar + cliente Rust (dark, sem mudança de comportamento)

**Objetivo:** infraestrutura de pé e testada, feature desligada.

1. **Compose** — novo serviço `llm-gpu`, espelhando o bloco `embeddings-gpu`
   (`docker-compose.yml:661`): mesma rede, mesmo padrão de healthcheck, mesmo
   `deploy.resources.reservations.devices`, profile próprio `llm-gpu`.
   Volume nomeado novo para o cache de modelo (não compartilhar `infinity_data`).
   Modelo: **parametrizado por `WQM_CONTEXT_LLM_MODEL`, ver D8** — começar pelo
   menor (0,5B) e subir só se a medição justificar. Nada de GGUF assado na imagem.
   Flags que importam: `--n-gpu-layers 99`, `--parallel` (batching contínuo),
   `--cache-reuse` (o que torna D6 efetivo, com a ressalva do R6), contexto ~4k.
2. **`.env` / `.env.example`** — `WQM_CONTEXT_LLM_BASE_URL`,
   `WQM_CONTEXT_LLM_MODEL`, `WQM_CONTEXT_ENABLED=0`. Adicionar `llm-gpu` ao
   `COMPOSE_PROFILES` **só na fase 4**.
3. **Env passthrough** — declarar as três vars no bloco `environment:` do serviço
   `memexd`, ao lado de `WQM_RERANK_BASE_URL` (`docker-compose.yml:154`).
4. **Cliente Rust** — `src/rust/daemon/core/src/embedding/context_gen.rs`,
   espelhando `rerank.rs` linha a linha: construção não-bloqueante, derivação
   `{base_url}/v1/chat/completions`, timeout limitado, sem header de auth,
   `from_env() -> Option<Self>`. Testes com `wiremock` no mesmo formato dos de
   `rerank.rs` (parse, erro remoto, short-circuit vazio) mais um caso novo:
   **resposta malformada / vazia → `Ok(None)`, nunca `Err` que suba**.
5. **Config** — `src/rust/daemon/core/src/config/context_llm.rs` no molde de
   `config/embedding.rs` (defaults, `validate()`, overrides por env), plugado em
   `config/mod.rs:306` junto do bloco de embedding.

**Critério de aceite:** `make redeploy` sobe sem o profile `llm-gpu` e nada muda.
Com o profile ligado, o healthcheck fica green e o cliente responde num teste
manual. `cargo test --package workspace-qdrant-core` verde.

---

### Fase 2 — Tabela de cache (migração v49)

**Objetivo:** persistência do contexto gerado, isolada e reversível.

1. `src/rust/daemon/core/src/schema_version/v49.rs`, seguindo a forma dos v01–v48:
   ```sql
   CREATE TABLE IF NOT EXISTS chunk_context (
       ctx_key        TEXT PRIMARY KEY,
       context_text   TEXT NOT NULL,
       model          TEXT NOT NULL,
       prompt_version INTEGER NOT NULL,
       created_at     TEXT NOT NULL
   );
   ```
2. Bumpar `CURRENT_SCHEMA_VERSION` para 49 em
   [schema_version/mod.rs:181](src/rust/daemon/core/src/schema_version/mod.rs:181)
   e registrar a migração no dispatcher de `migration.rs`.
3. Acesso: `get_many(&[ctx_key]) -> HashMap` e `put_many(...)`, **dentro de
   transação** (invariante do projeto: nenhuma query solta). Escritas puras →
   transação normal; não precisa de `begin_immediate` (esse padrão é para
   read-then-write, conforme PR #330/#331).
4. Poda: `DELETE` por `prompt_version` antigo, exposto como subcomando de
   manutenção. Sem isso o cache cresce monotonicamente a cada iteração de prompt.

**Ensaiar a migração contra uma CÓPIA do `state.db` vivo antes do deploy** — o
incidente da PR #271 (rename reescrevendo views) veio exatamente de pular esse passo.

**Critério de aceite:** migração aplica e é idempotente numa cópia do banco de
produção; teste de round-trip do cache passa.

---

### Fase 3 — Ligar no pipeline (gated, default OFF)

**Objetivo:** a mudança de comportamento, atrás de um flag desligado.

1. `build_dense_embedding_text` ganha um parâmetro
   `llm_context: Option<&str>`, inserido como última linha do header. **Os cinco
   testes existentes de `dense_text.rs` devem passar sem alteração** ao receber
   `None` — é a prova mecânica do fail-open. Adicionar testes novos: contexto
   presente, contexto vazio-após-trim, contexto multi-linha colapsado para uma
   linha, e o `raw_content_is_preserved_verbatim_after_header` reafirmado com
   contexto presente.
2. Em `embed_chunks` ([chunk_embed/mod.rs:78](src/rust/daemon/core/src/strategies/processing/file/chunk_embed/mod.rs:78)),
   antes do `map` que monta `chunk_texts`:
   - calcular `ctx_key` de cada chunk;
   - `get_many` no cache → hits;
   - misses → uma chamada em lote ao gerador, na ordem do arquivo (D6);
   - `put_many` dos novos;
   - qualquer erro/timeout → todos os contextos viram `None` e o fluxo segue.
3. Guardas de carga, não-negociáveis porque o memexd é a autoridade de escrita:
   - semáforo próprio (não reusar `ctx.embedding_semaphore` — recursos distintos
     saturam em ritmos distintos);
   - **pular geração quando o item da fila não é `is_active`** e a profundidade da
     fila está acima de um limiar: ingest ao vivo nunca espera pelo LLM;
   - timeout por lote curto (~5 s), com o lote inteiro caindo para `None`.
4. Payload: gravar `ctx_ver` em `build_chunk_payload` quando houver contexto.
   Sem isso não há como auditar cobertura nem separar corpus misto na fase 4.
5. Flag: `WQM_CONTEXT_ENABLED`, default `0`.

**Critério de aceite:** com o flag em `0`, os vetores produzidos são
byte-idênticos aos de hoje (verificar reindexando um arquivo e comparando o vetor
denso). Com `1`, o header ganha a linha e o `ctx_ver` aparece no payload.

---

### Fase 4 — Rollout medido em um tenant

**Objetivo:** provar o ganho antes de pagar o custo no corpus inteiro.

1. Adicionar `llm-gpu` ao `COMPOSE_PROFILES`, `WQM_CONTEXT_ENABLED=1`, `make redeploy`.
2. `make verify-deploy MARKER='<string literal ≥20 chars do código novo>'`.
   O MARKER precisa ser um **literal de string**, não um nome de função — o build
   release remove nomes de símbolo (lição recorrente: PRs #253/#123).
3. Pausar o watch do projeto ativo antes do backfill — o dequeue prioriza
   `is_active` e o reembed passa fome atrás dele:
   ```bash
   wqm project watch pause
   ```
4. Forced reembed em **um** tenant só, via `/admin/api/projects/reembed`.
   Acompanhar com `make reindex-status`.
5. A/B com `search_eval` no dataset bundled do mesmo tenant, contra o baseline da
   fase 0. Cuidado: o guard de dataset estrangeiro (PR #219) recusa dataset de
   outro tenant — use o mesmo tenant dos dois lados.
5b. **Sweep de tamanho do SLM (D8)** sobre um subconjunto ou tenant pequeno —
   0,5B → 1,5B → 3B, mesma métrica, mesmo dataset. Só um `.env` + recreate entre
   pontos; o `ctx_key` invalida o cache sozinho a cada troca. Escolher o **menor
   modelo dentro do ruído do maior**, não o de maior score absoluto.
6. Auditar cobertura: fração de pontos com `ctx_ver` presente. Corpus misto
   invalida a comparação; ou está ~100%, ou o número não vale.

**Métricas de decisão — SUPERSEDED, ver §5 R3.**

~~recall@10 +3 pontos~~ é **inmensurável** com n=46 (efeito mínimo detectável
≈ +11 pp). O critério vigente é o delta de MRR pareado por query + Wilcoxon
signed-rank + guarda de regressão de cauda, definido em §5 R3. `recall@10` fica
como métrica de relato, sem poder de decisão.

O kill é barato de propósito: o flag desliga tudo, a tabela `chunk_context` fica
órfã e inerte, e nenhum byte devolvido ao agente jamais dependeu da feature.

---

### Fase 5 — Extensões (só depois de a fase 4 fechar positiva)

- **5a — contexto no BM25/esparso.** É onde a receita original tira o maior ganho
  incremental. Muda o espaço de termos → exige A/B próprio e provavelmente
  reconstrução do léxico.
- **5b — query shaping (#2 da conversa).** Roda no mesmo sidecar: o LLM converte a
  query do agente em 2–3 queries + filtros (`fileType`, `pathGlob`, `excludeTests`)
  e o resultado é fundido por RRF. **Nunca token-concat** (lição do survey de RAG).
  Ataca a causa dominante de resultado vazio, que a telemetria já identificou como
  *scoping*, não ranking.
- **5c — campo `orientation` no retorno (#3).** Só aditivo e extrativo. Nunca
  reescrita: destruiria a garantia de que todo byte devolvido é verbatim do disco,
  que é a base do budget lossless + cursor.

---

## 4. Riscos

| Risco | Severidade | Mitigação |
|---|---|---|
| VRAM insuficiente para o 3º modelo | Alta | Fase 0 mede antes; fallback = embedder na CPU durante o backfill |
| Contenção de GPU degrada latência de search | Média | Failover CPU já existe; backfill só com watch pausado |
| Geração alucinada | **Baixa** | Texto nunca chega ao agente; pior caso é miss de recall |
| Prompt muda e reusa cache velho | Alta se acontecer | `prompt_version` **dentro** da chave de cache (D2) |
| Backfill trava a fila de ingest | Alta | Semáforo próprio + skip quando fila quente + timeout curto (Fase 3.3) |
| Migração v49 quebra views | Média | Ensaio contra cópia do banco vivo (lição PR #271) |

## 5. Revisão crítica (2026-08-10, pós-redação)

Revisão do plano acima contra o código e contra medições parciais. **A ordem das
fases muda**: não começar pela Fase 1.

> Medições citadas abaixo foram colhidas com outra sessão executando limpeza de
> órfãos. São **indicativas, não baseline** — qualquer número medido durante
> mutação do corpus não serve de denominador para a Fase 4.

### R1 — O número da receita original não se aplica aqui (grave)

O `-49%` é medido contra chunking **ingênuo, sem contexto nenhum**. O baseline
deste projeto já tem header contextual determinístico *e* rerank cross-encoder — e
o próprio paper mostra que os efeitos se sobrepõem em vez de somar (`-67%` com
rerank vs `-49%` sem). O ganho marginal esperado aqui é uma fração do publicado.

**Consequência:** o critério "+3 pontos de recall@10" da Fase 4 foi inventado, não
derivado. Ele provavelmente mataria uma feature que funciona. O threshold precisa
sair do baseline medido e do ruído do dataset de eval.

### R2 — A chave de cache invalida o arquivo inteiro a cada edição (corrige D2)

Se `file_context_digest` for digest do **conteúdo** do arquivo, editar uma função
regenera o contexto de todos os chunks daquele arquivo. Num repo em
desenvolvimento ativo isso é regeneração permanente e o cache nunca aquece.

**Correção do D2:** o digest deve ser do **outline** (nomes + kinds dos símbolos),
que só muda quando assinatura muda — não quando um corpo muda.

### R3 — A Fase 4 não tem harness de decisão verificado (BLOQUEADOR)

O plano diz "A/B com `search_eval` no dataset bundled do tenant" sem que eu tenha
confirmado que esse dataset existe e cobre este tenant.

**Resolvido em 2026-08-11 — mas o bloqueador migrou, não sumiu.**

#### O que existe (a busca inicial errou)

`src/typescript/mcp-server/scripts/benchmark-data/semantic-search-quality.yaml`
— 13 KB, **46 queries**, home tenant = workspace-qdrant-mcp. A busca anterior
procurou `*.json` e diretórios com "eval" no nome; o arquivo é YAML sob
`scripts/benchmark-data/`. Erro de busca, não ausência de dataset.

Categorias por prefixo de id: `impl` 14, `pt` 8, `sym` 6, `doc` 4, `real` 2, mais
~11 do set original de 2026-05. O harness (`benchmarks/semantic-search.ts`) já
emite **`mrr` por query** (`1/firstRelevantRank`, linha 456) e `rank` por hit
(linha 362). A instrumentação contínua já existe.

#### O bloqueador real: potência estatística

Com n=46, cada query vale **2,17 pontos** de recall@10. Num teste **pareado**
(McNemar, mesmas queries antes/depois), só os pares discordantes contam:

| Queries virando (sem regressão) | p exato (unilateral) |
|---|---|
| 3 | 0,125 |
| 4 | 0,063 |
| 5 | 0,031 ✔ |

Ou seja: **o efeito mínimo detectável é ~+11 pp de recall@10** (5 queries
virando, zero regressões). O R1 estabelece que o ganho esperado aqui é de um
dígito baixo. **O instrumento não enxerga o efeito que a feature deveria produzir.**

Isso é pior do que não ter dataset: como ele existe e roda fácil, convida a rodar,
tirar um número e acreditar nele.

#### Correção do critério de decisão (substitui a tabela da Fase 4)

Trocar a métrica de decisão de binária para **contínua e pareada**:

- **Métrica:** delta de MRR **por query** entre as duas execuções (o dado já é
  emitido). Um gold subindo de rank 7 para rank 3 conta — em hit@10 seria invisível.
- **Teste:** Wilcoxon signed-rank sobre os deltas pareados. Extrai muito mais sinal
  das mesmas 46 queries do que a contagem de flips.
- **Guarda de regressão, independente do agregado:** nenhuma query pode perder mais
  de N posições. Ganho médio positivo com cauda ruim é regressão disfarçada.
- **recall@10 vira métrica de relato, não gate** — mantida para comparabilidade com
  os baselines históricos, mas sem poder de decisão.

Mesmo pareado, 46 queries é pouco. Expandir o dataset (alvo: 150+, mantendo a
proporção de categorias) continua sendo o investimento de maior retorno para
qualquer decisão de qualidade de busca — não só esta feature.

**Ordem: a correção do critério vem antes da Fase 1.** Construir o sidecar antes
de ter como decidir é construir no escuro.

#### Estado 2026-08-11: critério IMPLEMENTADO

- `src/typescript/mcp-server/src/benchmarks/compare.ts` — comparação pareada
  (`compareBenchmarkReports`), Wilcoxon signed-rank exato e o veredito
  (`assessComparison`) codificando a regra acima (tail guard veta primeiro;
  depois Wilcoxon decide pela direção; resto = inconclusivo).
- `compare-io.ts` — adapta a RESPOSTA salva do `search_eval` (deriva `mrr` de
  `firstRelevantRank`; normaliza `recallAt10` de 0–100 para 0–1) + formatter.
- `scripts/compare-search-eval-reports.ts` — CLI: dois JSONs (full report ou
  resposta da tool, auto-detectado), `--mode/--max-rank-regression/--alpha`.
- 30 testes novos; estatística validada contra distribuições calculadas à mão.

Duas correções sobre o texto acima, agora travadas em teste:

1. A tabela usa McNemar **unilateral**; o teste implementado é **two-sided** —
   o piso all-positive é **6 pares** (p=2/64), não 5 (p=2/32=0,0625).
2. Empates em |Δ| **não** forçam aproximação normal: o p exato é computado
   condicionado às magnitudes observadas (ranks médios ×2 → DP inteiro).
   Relevante porque deltas de MRR são razões de inteiros pequenos e empatam
   com frequência — o caso comum, não o degenerado.

#### Baseline 2026-08-11 22:30 BRT (passo 2 — FEITO)

Corpus estável no momento da medição: tenant `367157a01d98` com **33.608 pontos**,
1884 arquivos, `indexing` 100% (0 pending / 0 in_progress / 0 failed); duas leituras
consecutivas idênticas. Coleção inteira em 329.523 pontos (outros tenants haviam
crescido ~10,7k após um redeploy, mas já assentados). Rerank ativo, `weight=0.10`.

| modo | top1 | top3 | top10 | recall@10 | MRR |
|---|---|---|---|---|---|
| semantic | 54,3% | 78,3% | 89,1% | **84,4%** | **0,67** |
| hybrid | — | — | — | 77,9% | — |

**Teste nulo (A vs A', duas execuções idênticas): ruído ZERO.** 46/46 queries com
ΔMRR exatamente 0, em semantic E hybrid. O harness é determinístico.

Consequência para ler resultados futuros — vale distinguir com cuidado:

- **O ruído zero NÃO baixa o limiar de significância.** Continuam sendo ~6 queries
  se movendo para p<0,05 two-sided.
- **Mas torna a ESTIMATIVA limpa.** Sem variância entre execuções, qualquer Δ≠0 é
  atribuível à mudança, nunca a jitter. Então um efeito pequeno demais para ser
  *significativo* ainda é **visível e confiável como magnitude** — o que permite
  decidir com julgamento (custo/benefício) mesmo quando o teste fica inconclusivo.

Por isso o veredito distingue os dois casos: n=0 reporta "null result, not an
underpowered one" em vez do aviso genérico de amostra pequena. São diagnósticos
diferentes e apontam para ações diferentes.

Baselines salvos em `tmp/20260811-22{30,31}_eval-run-{A,B}.json` (gitignored —
regravar com `POST /admin/api/tools/invoke {"tool":"search_eval"}` quando precisar).

### R4 — Escala subestimada

Contagens indicativas: **33.917** chunks no tenant `367157a01d98`
(workspace-qdrant-mcp), mas **318.844** pontos na coleção `projects` inteira.

Um tenant é ~35 min de geração — tranquilo. O corpus completo é ~19M tokens de
saída, várias horas de GPU. A Fase 5 escondia isso numa linha ("expandir para os
demais tenants"); precisa ser fase própria com janela planejada.

### R5 — Existe um controle determinístico mais barato que não foi considerado

Medição indicativa: **82,1%** dos chunks deste tenant não têm docstring
(27.846/33.917); 21,0% não têm `symbol_name`. *(Marginais — a interseção não foi
medida.)*

Ou seja: para a grande maioria dos chunks o header existente é só
`path | symbol (kind)`. Há headroom real — mas há também uma alavanca
**determinística** não explorada antes de subir um LLM: enriquecer o header com os
imports do arquivo e/ou o breadcrumb de módulo, dado que o tree-sitter já extrai
essa informação (o payload já carrega `chunk_calls`).

Custo: zero GPU, zero latência, zero cache, zero migração. Se isso capturar metade
do ganho, o LLM passa a ter que justificar só o resto.

### R6 — D6 apresenta como resolvido algo que não está

Com `--parallel` (batching contínuo), chunks do mesmo arquivo caem em slots
diferentes e o reuso de prefixo KV degrada. Ou serializa por arquivo (perde
throughput) ou aceita reuso parcial. O D6 afirmava o ganho sem essa ressalva.

### Ordem revisada

| # | Passo | Motivo |
|---|---|---|
| 1 | Resolver R3 — dataset de eval válido para o tenant | Sem isso não há como decidir nada |
| 2 | Baseline limpo, com o corpus estável | R7: não medir durante limpeza de órfãos |
| 3 | Testar R5 — enriquecimento determinístico do header | Controle barato; pode tornar o LLM desnecessário |
| 4 | Só então avaliar o LLM contra esse **novo** baseline | O ganho a justificar é o incremento sobre R5, não sobre hoje |

As Fases 1–5 originais seguem válidas em estrutura (gating, fail-open, kill barato)
— mas só entram em execução depois do passo 4 acima.

## 6. ACHADO 2026-08-11 — este plano ataca o gargalo errado

O passo 3 (controle determinístico) foi executado como investigação e produziu um
resultado que **invalida a premissa do plano inteiro**. Registrado aqui em vez de
silenciosamente abandonado.

### O controle determinístico não tinha o que enriquecer

Perfil do metadata dos 33,5k chunks do tenant:

| campo | cobertura |
|---|---|
| `symbol_name` | 78,6% |
| `signature` | 43,1% |
| `calls` | 32,3% |
| `docstring` | 17,8% |
| fragmentos | 22,6% |

`signature` parecia o candidato — 43% de cobertura e ausente do header. Mas
`splitting.rs:154` propaga a signature para todos os fragmentos, e só **1.425 dos
7.573** fragmentos têm uma; nos ~13k chunks não-fragmentados a signature **já é a
primeira linha do conteúdo**. Adicioná-la ao header seria majoritariamente
redundante. `calls` foi descartado por risco de diluição (injeta vocabulário de
outros símbolos no vetor deste chunk). Imports não estão no caminho do chunker —
são extraídos em `graph/extractor/import_parsers.rs`, e puxá-los descaracterizaria
"controle barato".

### Onde as falhas realmente estão

Analisando o baseline por query, as 5 falhas do semantic são **todas `pt-`**:

| categoria | n | fora do top-10 | rank 4–10 |
|---|---|---|---|
| pt | 8 | **5** | 0 |
| impl | 14 | 0 | 4 |
| orig | 12 | 0 | 1 |
| sym | 6 | 0 | 0 |
| doc | 4 | 0 | 0 |
| real | 2 | 0 | 0 |

O hybrid não resgata nenhuma delas (`hybrid=None` também) — **não é lacuna
lexical**. Todas as categorias em inglês já estão em 100% de top-10.

### Experimento decisivo: tradução

As mesmas 5 queries, traduzidas para inglês, rodadas como `cases` ad-hoc:

| query | PT (baseline) | EN (traduzida) |
|---|---|---|
| pt-fila-retry | miss | **rank 8** |
| pt-upsert-qdrant | miss | **rank 1** |
| pt-idempotencia | miss | **rank 6** |
| pt-busca-hibrida | miss | **rank 8** |
| pt-arquivos-ignorados | miss | **rank 9** |

**5/5.** O gargalo é **cross-lingual**, não contextual. Projetando para o conjunto
completo, o semantic top-10 iria de 89,1% (41/46) para 100% (46/46).

### Consequência

**Header contextual — determinístico OU via SLM — não resolve isto.** Ambos só
adicionam mais tokens em inglês do lado do documento; não constroem a ponte
PT→EN do lado da query. E as categorias que um header melhor poderia ajudar já
estão saturadas: sobram 5 queries de profundidade de rank (4–10), contra 5 falhas
totais que a tradução resolve inteiramente.

**Recomendação: trocar a Fase 5b (query shaping) pela Fase 1.** A alavanca é
traduzir/expandir a query antes de embeddar — o mesmo sidecar SLM, aplicado do
outro lado do pipeline. Cuidados de projeto:

1. **Aditivo, não substitutivo:** buscar com a query original E a traduzida,
   fundindo por RRF. Substituir quebraria conteúdo legitimamente em português
   (scratchpad, docs). Nunca token-concat.
2. Só disparar quando a query não for inglês — detecção barata, e evita custo e
   risco no caminho comum.
3. É query-time: o orçamento de latência é apertado (p50 atual ~90ms), o que
   reforça o D8 na direção do menor modelo.

**Nota sobre o instrumento:** com 5 queries virando, todas positivas e sem
regressão, o Wilcoxon dá p=0,0625 — INCONCLUSIVE pelo teste, exatamente o piso
travado em teste. Mas o ruído é zero e a direção é unânime, então a magnitude é
confiável e a decisão é de julgamento, não de significância. É o cenário previsto
em §5 R3 — e o argumento mais forte até agora para expandir o dataset.

## 7. Fora de escopo

- Bump de `CHUNKER_LOGIC_VERSION` (ver D3).
- Qualquer mudança no payload devolvido pelas read surfaces além do `ctx_ver`.
- Substituir o header determinístico existente — ele permanece como base e como
  comportamento de fallback.
