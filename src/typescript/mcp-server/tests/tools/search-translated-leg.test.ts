import { describe, expect, it, vi } from 'vitest';

import type { QueryTranslator } from '../../src/clients/query-translator.js';
import {
  QUERY_TRANSLATE_ENV,
  resolveTranslatedQuery,
  translatedLegEnabled,
} from '../../src/tools/search-translated-leg.js';

const PT = 'Onde a fila aplica retry e backoff para itens que falharam?';
const EN = 'Where does the queue apply retry and backoff for failed items?';

const ON = { [QUERY_TRANSLATE_ENV]: '1' };

function fakeTranslator(result: string | null): QueryTranslator {
  return { translateToEnglish: vi.fn(async () => result) } as unknown as QueryTranslator;
}

describe('translatedLegEnabled', () => {
  it('is off unless the flag is exactly "1"', () => {
    expect(translatedLegEnabled({})).toBe(false);
    expect(translatedLegEnabled({ [QUERY_TRANSLATE_ENV]: '' })).toBe(false);
    expect(translatedLegEnabled({ [QUERY_TRANSLATE_ENV]: '0' })).toBe(false);
    expect(translatedLegEnabled({ [QUERY_TRANSLATE_ENV]: 'true' })).toBe(false);
    expect(translatedLegEnabled({ [QUERY_TRANSLATE_ENV]: '1' })).toBe(true);
  });
});

describe('resolveTranslatedQuery', () => {
  it('is disabled by default — no translator call at all', async () => {
    const translator = fakeTranslator(EN);
    const decision = await resolveTranslatedQuery(PT, translator, {});

    expect(decision).toEqual({ query: null, reason: 'disabled' });
    expect(translator.translateToEnglish).not.toHaveBeenCalled();
  });

  it('returns no leg when translation is unconfigured', async () => {
    expect(await resolveTranslatedQuery(PT, null, ON)).toEqual({
      query: null,
      reason: 'no-translator',
    });
  });

  it('skips the network hop for an English query', async () => {
    // The whole point of the local gate: the common path pays nothing.
    const translator = fakeTranslator(EN);
    const decision = await resolveTranslatedQuery(EN, translator, ON);

    expect(decision).toEqual({ query: null, reason: 'already-english' });
    expect(translator.translateToEnglish).not.toHaveBeenCalled();
  });

  it('skips the network hop for an identifier query', async () => {
    const translator = fakeTranslator(EN);
    const decision = await resolveTranslatedQuery('applyRRFFusion implementation', translator, ON);

    expect(decision.query).toBeNull();
    expect(translator.translateToEnglish).not.toHaveBeenCalled();
  });

  it('translates a Portuguese query', async () => {
    const translator = fakeTranslator(EN);
    const decision = await resolveTranslatedQuery(PT, translator, ON);

    expect(decision).toEqual({ query: EN, reason: 'translated' });
    expect(translator.translateToEnglish).toHaveBeenCalledWith(PT);
  });

  it('falls back to a single leg when the translator returns nothing', async () => {
    // Covers every client-side rejection — echo, still-Portuguese, timeout,
    // HTTP error — since all of them surface as null.
    const decision = await resolveTranslatedQuery(PT, fakeTranslator(null), ON);

    expect(decision).toEqual({ query: null, reason: 'translation-failed' });
  });
});
