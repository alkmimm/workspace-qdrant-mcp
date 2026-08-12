import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { describe, expect, it } from 'vitest';
import { parse as parseYaml } from 'yaml';

import { classifyQueryLanguage } from '../../src/utils/query-language.js';

describe('classifyQueryLanguage', () => {
  it('routes Portuguese questions through translation', () => {
    const verdict = classifyQueryLanguage('Onde a fila aplica retry e backoff para itens que falharam?');
    expect(verdict.isLikelyNonEnglish).toBe(true);
  });

  it('catches Portuguese carrying no diacritic at all', () => {
    // No accented character anywhere — the function-word floor has to carry it.
    const verdict = classifyQueryLanguage('Onde a fila aplica retry e backoff para itens que falharam?');
    expect(verdict.hasDiacritic).toBe(false);
    expect(verdict.romanceWords).toBeGreaterThanOrEqual(2);
    expect(verdict.isLikelyNonEnglish).toBe(true);
  });

  it('keeps English questions on the English path', () => {
    const verdict = classifyQueryLanguage('Where does the queue apply retry and backoff for failed items?');
    expect(verdict.isLikelyNonEnglish).toBe(false);
  });

  it('defaults identifier lookups to English rather than guessing', () => {
    // Neither language's function words appear; absence of English must never
    // by itself route a query into translation.
    for (const q of ['applyRRFFusion implementation', 'SPARSE_ONLY_WEIGHT sparse-only demotion']) {
      const verdict = classifyQueryLanguage(q);
      expect(verdict.isLikelyNonEnglish).toBe(false);
      expect(verdict.romanceWords).toBe(0);
    }
  });

  it('defaults keyword-shaped queries to English', () => {
    const verdict = classifyQueryLanguage('reciprocal rank fusion dense sparse');
    expect(verdict.isLikelyNonEnglish).toBe(false);
    expect(verdict.reason).toMatch(/defaulting to English/);
  });

  it('does not fire on an English query quoting one accented term', () => {
    // A lone diacritic must not outvote plain English — otherwise a query about
    // a café/naïve identifier would pay a pointless round-trip.
    const verdict = classifyQueryLanguage('Where is the café encoding handled in the parser?');
    expect(verdict.hasDiacritic).toBe(true);
    expect(verdict.isLikelyNonEnglish).toBe(false);
  });

  it('uses Portuguese morphology when function words alone fall short', () => {
    // Only `onde` is a function word here; `usando` (-ando) has to carry it
    // past the floor. Caught by the dataset acceptance test below, not by any
    // hand-picked case.
    const verdict = classifyQueryLanguage('Onde o daemon gera embeddings densos usando FastEmbed ONNX?');
    expect(verdict.isLikelyNonEnglish).toBe(true);
    expect(verdict.romanceWords).toBeGreaterThanOrEqual(2);
  });

  it('does not let short English words trip a Romance suffix', () => {
    // The 4-char prefix guard is what stops "and"/"end"/"undo"-shaped tokens.
    for (const q of ['undo the last write', 'append and end the batch']) {
      expect(classifyQueryLanguage(q).isLikelyNonEnglish).toBe(false);
    }
  });

  it('handles empty and punctuation-only input without firing', () => {
    for (const q of ['', '   ', '???', '---']) {
      expect(classifyQueryLanguage(q).isLikelyNonEnglish).toBe(false);
    }
  });

  it('reports a reason on every verdict', () => {
    for (const q of ['Onde está o chunker?', 'Where is the chunker?', 'chunker']) {
      expect(classifyQueryLanguage(q).reason.length).toBeGreaterThan(0);
    }
  });
});

/**
 * Acceptance gate for the language gate (plan phase 1): it must classify the
 * bundled benchmark exactly — every `pt-` query routed to translation, every
 * other query left on the English path. Reading the real dataset rather than a
 * fixture is deliberate: when the bucket grows, this test is what proves the
 * gate still covers it.
 */
describe('classifyQueryLanguage over the bundled benchmark dataset', () => {
  const here = dirname(fileURLToPath(import.meta.url));
  const datasetPath = join(here, '../../scripts/benchmark-data/semantic-search-quality.yaml');
  const dataset = parseYaml(readFileSync(datasetPath, 'utf8')) as {
    queries: Array<{ id: string; query: string }>;
  };

  it('routes every pt- query through translation', () => {
    const ptQueries = dataset.queries.filter((q) => q.id.startsWith('pt-'));
    expect(ptQueries.length).toBeGreaterThanOrEqual(33);

    const escaped = ptQueries.filter((q) => !classifyQueryLanguage(q.query).isLikelyNonEnglish);
    expect(escaped.map((q) => q.id)).toEqual([]);
  });

  it('leaves every non-pt query on the English path', () => {
    const others = dataset.queries.filter((q) => !q.id.startsWith('pt-'));
    expect(others.length).toBeGreaterThanOrEqual(38);

    const misrouted = others.filter((q) => classifyQueryLanguage(q.query).isLikelyNonEnglish);
    expect(misrouted.map((q) => q.id)).toEqual([]);
  });
});
