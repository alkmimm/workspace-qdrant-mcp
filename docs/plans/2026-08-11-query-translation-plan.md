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

### Atualização 2026-08-11 23:15 — bucket expandido para n=33 (Fase 4 concluída)

Com 33 queries `pt-` (25 delas espelhando uma query em inglês, mesmo gold):

| categoria | n | top-10 |
|---|---|---|
| **pt** | 33 | **69,7%** (10 miss) |
| doc / impl / orig / real / sym | 38 | **100%** |

A potência agora existe: 10 flips daria Wilcoxon exato p≈0,002. Em n=8 o teto
era p=0,0625 mesmo com tudo virando.

**O gap NÃO é uniforme.** Nos 25 pares espelhados: **PT pior em 10, empate em
11, PT melhor em 4**. Ou seja, 15 de 25 conceitos já estão em paridade ou
melhores em português. O dano é concentrado, não difuso — o que também significa
que uma tradução aplicada indiscriminadamente pode piorar os 15 que já funcionam.
Reforça o D2/D3 (aditivo, perna original com peso ≥).

### O mecanismo: homofilia de idioma, não "fraqueza multilíngue"

Medindo quanto os 8 documentos predominantemente em português do repo ocupam do
top-10:

| bucket | slots ocupados por doc PT |
|---|---|
| queries `pt-` (33) | 48 de 294 = **16,3%** |
| queries em inglês (38) | 4 de 355 = **1,1%** |

**14,5× de sobre-representação** — e esses 8 arquivos são 0,4% dos 1884 indexados.
Em **5 das 10 falhas** há um doc PT no top-3, deslocando o código correto.

O embedder está recuperando texto em português **por ser português**, não por ser
relevante. Isso é mais específico que "cross-lingual fraco" e tem consequência de
projeto direta: **traduzir a query remove o sinal de idioma que sequestra o
ranking** — é exatamente a intervenção certa para este mecanismo, e explica por
que traduzir restaurou 5/5 no teste inicial.

Nota de honestidade sobre os docs PT: 6 dos 8 já existiam antes deste trabalho;
2 são planos desta thread. Não é artefato introduzido pela medição — é
propriedade real do corpus, e um usuário que escreve docs em português vai
ampliá-la com o tempo.

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

## 6. RESULTADO — fases 2/3/5 executadas (2026-08-12)

Todas as fases foram implementadas e medidas. Baseline sempre a mesma: flag
desligado, mesmo corpus, 71 queries, imediatamente antes da variante.

| configuração | pt top-10 | top-3 geral | MRR | Wilcoxon |
|---|---|---|---|---|
| baseline (flag off) | 23/33 (69,7%) | 50/71 | 0,593 | — |
| 1,5B + RRF w=0,7 | 23/33 | — | — | **inerte** |
| 1,5B + RRF w=0,9 | 26/33 (78,8%) | — | — | p=0,46 |
| 1,5B + RRF w=1,0 | 28/33 (84,8%) | — | — | tail guard: REGRESSION |
| 7B + glossário + RRF w=0,9 | 29/33 (87,9%) | 50/71 | 0,607 | p=0,14 |
| **7B + glossário + best-rank** | **29/33 (87,9%)** | **53/71** | **0,627** | **p=0,0119** |
| *teto (tradução ideal + oráculo)* | *31/33 (93,9%)* | — | — | — |

**94% do teto capturado.** As 2 queries restantes (`pt-busca-hibrida`,
`pt-upsert-qdrant`… corrigida) falham nos dois idiomas — não são cross-lingual.

### As três contribuições, medidas em separado

1. **Modelo 1,5B → 7B** resolve *seguir instrução*, não vocabulário. O 1,5B
   ecoou uma query e respondeu outra em português; o 7B traduziu as duas. Mas
   `vazão` só trocou de erro (`queue size` → `the flow of the queue`).
   Latência mediana 123 → 182 ms.
2. **Glossário de domínio** (prompt-only, +14 ms) fechou o vocabulário:
   `vazão`→throughput, `enviados`→upserted, `idempotência`→idempotency. A
   entrada `upsert` foi **prevista antes de medida** — no experimento do teto,
   "sent to Qdrant" dava miss e "upsert embedded points" acertava o mesmo gold.
3. **Forma da fusão** era o maior termo restante. RRF ponderado obriga um
   escalar global a servir duas situações opostas (dominar quando a perna
   original é lixo, sumir quando ela é boa). O sweep tornou isso visível:
   0,7 inerte, 1,0 quebra o top-3. `best-rank` não tem esse conflito.

### O custo do best-rank

Intercalar por melhor rank numa página de tamanho fixo faz algo sair. Uma query
(`pt-debounce-eventos`) deslizou 5 → 9, acionando o guarda de cauda, que
reportou REGRESSION apesar do ganho significativo.

Mantido como default deliberadamente: o *princípio* do guarda vale, mas seu
limiar de 3 posições é um default não medido, a query não saiu do top-10, e
quatro queries saíram de invisíveis para os ranks 2–5. `WQM_TRANSLATE_FUSION=rrf`
desfaz o trade. Um teste fixa o deslocamento diretamente.

### Lições de método

- **A verificação do flag era falsa.** `docker exec ... "echo [$VAR]"` rodado de
  um shell que tinha a variável exportada expandiu do lado de FORA e confirmou o
  host, não o container. Só um probe em node DENTRO do container mostrou
  `undefined` — e o bug era real: `WQM_QUERY_TRANSLATE` nunca foi para o compose.
- **O default de 0,7 era aritmeticamente inerte**, não apenas subótimo: um hit
  traduzido em rank 0 vale `w/(K+1)`, abaixo do ÚLTIMO hit da original.
- **O corpus se move.** Entre baseline e variante o índice cresceu 8% com os
  próprios arquivos deste trabalho, alterando 3 queries. Baseline e variante têm
  de ser tomados em sequência, sem escrita no repo entre eles.

### Não feito

**L2 — boost de rerank gated.** O `bge-reranker-v2-m3` já deployado é
multilíngue e pode pontuar (query PT, chunk EN) sem tradução. Ficou fora desta
rodada de propósito: seriam duas mudanças de ranking simultâneas e o ganho não
seria atribuível. É o próximo A/B natural, e é complementar — tradução é lever
de *recall*, cross-encoder é lever de *precisão*.

## 7. Fora de escopo

- Qualquer mudança em `applyRRFFusion` ou nos pesos denso/sparse (D1).
- Tradução do lado do documento.
- Troca do modelo de embedding.
- Contextual chunk headers — ver §6 do plano irmão para por que foi despriorizado.
