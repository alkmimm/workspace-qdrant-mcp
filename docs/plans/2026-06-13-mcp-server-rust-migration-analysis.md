# Analise de migracao do MCP server para Rust

Data: 2026-06-13

## Contexto

O fork atual ainda usa o MCP server em TypeScript em `src/typescript/mcp-server/`,
com `@modelcontextprotocol/sdk`, Node.js e uma arvore grande de testes.

No `upstream/main`, o MCP server ja foi refeito em Rust em
`src/rust/daemon/mcp-server/`, com crate proprio, bench de overhead e uma suite
de testes dedicada.

## O que foi verificado

- O server atual do fork continua sendo TypeScript.
- O upstream ja tem a implementacao Rust do MCP server.
- O upstream tambem adicionou bench e testes para a versao Rust.
- O stack do projeto ja e majoritariamente Rust fora do MCP server.

## Ganhos provaveis da migracao

### 1. Manutencao e coesao

- Remove uma fronteira de linguagem no caminho critico do MCP.
- Unifica o stack com o daemon e o CLI, que ja sao Rust.
- Reduz dependencia de Node, `tsx`, npm lockfile e tooling JS.
- Facilita diagnostico e suporte, porque ha menos runtime para ajustar.

### 2. Deploy e operacao

- Melhor chance de startup mais rapido.
- Menor consumo de memoria em idle.
- Empacotamento mais simples, com um binario Rust em vez de um runtime Node.
- Menos risco de divergencias de ambiente entre dev, CI e producao.

### 3. Confiabilidade do ecossistema

- Menos pontos de falha em ESM/CJS, native addons e install de dependencias JS.
- Melhor encaixe com o restante do projeto, que ja usa Rust para o daemon,
  CLI, shared crates e observabilidade.

## O que nao tende a melhorar muito

- A latencia do caminho pesado de busca e ingestao provavelmente nao muda de
  forma dramativa, porque o custo principal continua no daemon, SQLite e Qdrant.
- O ganho real e mais forte em overhead de servidor, operacao e manutencao do
  que em throughput do backend.

## Sobre a facilidade da migracao

Sim, a migracao tende a ser bem mais facil do que reescrever tudo do zero, por
alguns motivos:

- o servico atual ja e estavel;
- existe uma suite grande de testes no server TypeScript;
- o upstream ja oferece uma versao Rust funcional como referencia;
- o comportamento esperado pode ser comparado por paridade, nao por
  reconstrucao de requisitos do zero.

Mas nao e automatica. O trabalho ainda precisa preservar:

- transporte MCP;
- semantica de sessoes;
- envelopes de erro e saida;
- auth e HTTP;
- metricas;
- modo degraded/fallback;
- compatibilidade dos tools e dos formatos JSON.

## Leitura pratica

- Se a prioridade e manutencao de medio/longo prazo, a migracao para Rust faz
  sentido.
- Se a prioridade fosse apenas performance de busca, o ganho seria menor e
  menos garantido.
- Como existe uma base estavel e bem testada, a migracao fica mais segura, mas
  continua sendo uma troca de implementacao com risco de regressao sem uma
  cobertura de paridade boa.

## Arquivos de referencia

- `src/typescript/mcp-server/src/index.ts`
- `src/typescript/mcp-server/src/server.ts`
- `src/typescript/mcp-server/package.json`
- `src/typescript/mcp-server/tests/`
- `src/rust/daemon/mcp-server/src/main.rs`
- `src/rust/daemon/mcp-server/src/lib.rs`
- `src/rust/daemon/mcp-server/Cargo.toml`
- `src/rust/daemon/mcp-server/benches/README.md`
- `src/rust/daemon/mcp-server/tests/`

## Recomendacao

O melhor caminho parece ser migrar, mas de forma guiada por paridade:

1. mapear tool por tool entre TS e Rust;
2. manter os testes de contrato e criar equivalentes Rust;
3. validar cold start, memoria e latencia em um comparativo real;
4. so entao trocar o server padrao.

