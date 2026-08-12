# Query translation (PT→EN) — plano de implementação

**Data:** 2026-08-11
**Origem:** achado registrado em `2026-08-10-contextual-chunk-headers-plan.md` §6.
**Escopo:** traduzir a query não-inglesa antes de embeddar e fundir os dois
rankings. Query-time, lado TypeScript. Daemon intocado.

---

## 1. Evidência

Baseline de 2026-08-11 (46 queries, corpus estável, ruído zero entre execuções):

| categoria | n | fora do top-10 |
|---|---|---|
| **pt** | 8 | **5** |
| impl / orig / sym / doc / real | 38 | **0** |

As mesmas 5 queries traduzidas para inglês: **5/5 acertam** (ranks 8, 1, 6, 8, 9).
O hybrid não resgatava nenhuma, então não é lacuna lexical — é cross-lingual.

Projeção para o conjunto completo: semantic top-10 **89,1% → 100%**.

Toda a massa de falha do benchmark está num bucket, e a tradução o resolve
inteiro. Nenhuma outra alavanca de qualidade de busca tem esse perfil hoje.

## 2. Alternativas descartadas

**Voltar para um embedder multilíngue.** É a causa direta do problema —
CodeRankEmbed é code-specialized — mas a troca que o introduziu levou recall@10
de 58,7% para 81,5%. Reverter recuperaria PT e devolveria ~23 pontos de recall
em código. O bucket PT vale 5 queries; o resto vale 38. **Trade péssimo.**

**Instruir agentes a escrever em inglês.** Já está nas instruções do servidor MCP,
e é por isso que o problema não aparece mais alto na telemetria. Mas não cobre o
humano digitando em português — que é o modo de uso real deste usuário — nem
agentes em conversa PT que ignorem a instrução. Instrução não é mecanismo.

**Índice dual (segundo vetor multilíngue).** Resolveria, ao custo de um segundo
espaço vetorial sobre 318k pontos: reembed completo, mais VRAM, mais storage,
e uma decisão de fusão nova. Desproporcional para 5 queries.

**Tradução no lado do documento.** Traduzir chunks para PT inverte o problema e
multiplica o corpus. Não.

## 3. Desenho

**Onde:** servidor MCP (TypeScript), antes do dispatch da busca. O daemon aplica
`query_prefix` e embedda (`embedding_service.rs:229`) sem saber de nada disso.

**Fluxo:**

1. Query chega.
2. **Gate barato primeiro** — heurística local (proporção não-ASCII + stopwords
   PT/ES) decide se *parece* não-inglês. Só então há chamada ao SLM. O caminho
   comum (query em inglês) não paga hop de rede nenhum.
3. SLM traduz para inglês. Prompt curto, saída de ~15 tokens.
4. Busca roda **duas vezes** — original e traduzida — com opções idênticas, em
   paralelo.
5. Os dois rankings finais são fundidos por RRF simétrico.
6. Resposta marca qual perna contribuiu (observabilidade).

**D1 — Fundir os rankings FINAIS, não as pernas internas.**
`applyRRFFusion` (search-qdrant.ts:178) funde denso × keyword com pesos
calibrados (`KEYWORD_WEIGHT=0.25`, `SPARSE_ONLY_WEIGHT`) — a nota do código
registra que errar isso derrubou hybrid recall de 58,7% para 38%. Multi-query é
um eixo **ortogonal**. Rodar a busca completa (com a fusão interna dela) duas
vezes e fundir os dois resultados finais deixa aquela calibragem intocada. Não
mexer em `applyRRFFusion`.

**D2 — Aditivo, nunca substitutivo.** A perna original permanece sempre. Conteúdo
legitimamente em português (scratchpad, docs, comentários) continua alcançável
pela query original. Substituir trocaria um bucket quebrado por outro.

**D3 — Peso da perna original ≥ traduzida.** Garante o piso: uma tradução ruim
degrada para "não pior que hoje" em vez de deslocar bons resultados. O peso é o
parâmetro a varrer na medição.

**D4 — Nunca token-concat.** Duas buscas e fusão, jamais concatenar as duas
queries num único texto de embedding.

**D5 — Fail-open.** SLM indisponível, timeout ou resposta vazia → só a query
original, exatamente o comportamento atual. Busca nunca bloqueia por tradução.

**D6 — Sem reindex.** Mudança puramente query-side. Diferente do plano de
contextual headers, que exigia reembed de 33,5k chunks só para ser testado, este
é A/B-ável imediatamente. É a maior vantagem prática desta alavanca.

## 4. Fases

**Fase 1 — Gate de idioma + medição do gate (sem SLM).**
Heurística pura, com testes. Medir sobre as 46 queries do dataset: precisa
classificar as 8 `pt-` como não-inglês e as 38 restantes como inglês. Falso
positivo custa latência; falso negativo mantém a falha atual. Ship sozinha —
sem consumidor ainda, sem mudança de comportamento.

**Fase 2 — Sidecar SLM + cliente de tradução.**
Reaproveita integralmente o desenho já especificado no plano irmão (§ Fase 1 e
D7/D8 de `2026-08-10-contextual-chunk-headers-plan.md`): serviço `llm-gpu` com
llama.cpp, `/v1/chat/completions`, modelo parametrizado por env, começando pelo
menor. Aqui o D8 aperta: é query-time, então o orçamento de latência manda —
começar em 0,5B e só subir se a tradução medir pior.

**Fase 3 — Segunda perna + fusão (gated, default OFF).**
Busca dupla em paralelo, RRF simétrico dos rankings finais, flag
`WQM_QUERY_TRANSLATE=0` por padrão. Com o flag desligado, resultados devem ser
byte-idênticos aos de hoje.

**Fase 4 — Expandir o bucket `pt-` do dataset.**
**Bloqueador para decidir, não opcional.** Com 8 queries PT (5 falhando), o
Wilcoxon pareado dá p=0,0625 mesmo com 5/5 virando e zero regressão — o piso
travado em `compare.test.ts`. Alvo: ~30 queries `pt-`, cobrindo as mesmas áreas
das categorias em inglês. Sem isso a Fase 5 mede algo que não consegue concluir.

**Fase 5 — Medir e decidir.**
`search_eval` antes/depois → `scripts/compare-search-eval-reports.ts`. Critério
já definido (ΔMRR pareado + Wilcoxon + guarda de cauda). Varrer o peso da perna
traduzida (D3). **A guarda de cauda é o gate que importa aqui:** o risco real não
é deixar de ganhar nas PT, é regredir alguma query em inglês por ruído da segunda
perna.

## 5. Riscos

| Risco | Severidade | Mitigação |
|---|---|---|
| Tradução perde termo de domínio | Média | Perna original sempre presente (D2) + peso ≥ (D3) |
| Segunda perna regride query inglesa | **Alta** | Gate de idioma não dispara em inglês; guarda de cauda é o gate de decisão |
| Latência em query PT | Média | Gate local antes do SLM; buscas em paralelo; modelo menor (0,5B) |
| Falso negativo do gate | Baixa | Mantém a falha de hoje, não introduz nova |
| Decidir com n insuficiente | **Alta** | Fase 4 é bloqueador explícito |

## 6. Fora de escopo

- Qualquer mudança em `applyRRFFusion` ou nos pesos denso/sparse (D1).
- Tradução do lado do documento.
- Troca do modelo de embedding.
- Contextual chunk headers — ver §6 do plano irmão para por que foi despriorizado.
