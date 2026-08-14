import { describe, expect, it, vi } from 'vitest';

import {
  QueryTranslator,
  createQueryTranslatorFromEnv,
  sanitizeTranslation,
  TRANSLATE_BASE_URL_ENV,
  TRANSLATE_MODEL_ENV,
  type FetchLike,
} from '../../src/clients/query-translator.js';

const PT = 'Onde a fila aplica retry e backoff para itens que falharam?';
const EN = 'Where does the queue apply retry and backoff for failed items?';

/** Fake fetch returning one chat-completion body. */
function respondWith(content: unknown, ok = true, status = 200): FetchLike {
  return vi.fn(async () =>
    ({
      ok,
      status,
      json: async () => ({ choices: [{ message: { content } }] }),
    }) as unknown as Response
  );
}

function translator(fetchImpl: FetchLike, timeoutMs?: number): QueryTranslator {
  return new QueryTranslator({
    baseUrl: 'http://wqm-llm-gpu:8080',
    model: 'test-model',
    fetchImpl,
    ...(timeoutMs !== undefined ? { timeoutMs } : {}),
  });
}

describe('QueryTranslator', () => {
  it('derives the OpenAI-compatible endpoint and strips a trailing slash', () => {
    expect(translator(respondWith(EN)).endpointUrl).toBe(
      'http://wqm-llm-gpu:8080/v1/chat/completions'
    );
    expect(
      new QueryTranslator({ baseUrl: 'http://host:8080/', model: 'm' }).endpointUrl
    ).toBe('http://host:8080/v1/chat/completions');
  });

  it('returns the translated query', async () => {
    await expect(translator(respondWith(EN)).translateToEnglish(PT)).resolves.toBe(EN);
  });

  it('requests greedy decoding so an A/B compares a fixed target', async () => {
    const fetchImpl = respondWith(EN);
    await translator(fetchImpl).translateToEnglish(PT);

    const body = JSON.parse((vi.mocked(fetchImpl).mock.calls[0]![1] as RequestInit).body as string);
    expect(body.temperature).toBe(0);
    expect(body.model).toBe('test-model');
    expect(body.messages.at(-1)).toEqual({ role: 'user', content: PT });
  });

  it('sends no Authorization header — the sidecar is unauthenticated', async () => {
    const fetchImpl = respondWith(EN);
    await translator(fetchImpl).translateToEnglish(PT);

    const headers = (vi.mocked(fetchImpl).mock.calls[0]![1] as RequestInit).headers as Record<string, string>;
    expect(Object.keys(headers).map((k) => k.toLowerCase())).not.toContain('authorization');
  });

  describe('fail-open', () => {
    it('returns null on an HTTP error', async () => {
      await expect(translator(respondWith(EN, false, 503)).translateToEnglish(PT)).resolves.toBeNull();
    });

    it('returns null when fetch rejects', async () => {
      const fetchImpl: FetchLike = vi.fn(async () => {
        throw new Error('ECONNREFUSED');
      });
      await expect(translator(fetchImpl).translateToEnglish(PT)).resolves.toBeNull();
    });

    it('returns null when the body has no string content', async () => {
      await expect(translator(respondWith(undefined)).translateToEnglish(PT)).resolves.toBeNull();
      await expect(translator(respondWith(42)).translateToEnglish(PT)).resolves.toBeNull();
    });

    it('returns null when the request times out', async () => {
      // Never resolves on its own; only the abort signal ends it.
      const fetchImpl: FetchLike = vi.fn(
        (_url, init) =>
          new Promise((_resolve, reject) => {
            init.signal?.addEventListener('abort', () => reject(new Error('aborted')));
          })
      );
      await expect(translator(fetchImpl, 10).translateToEnglish(PT)).resolves.toBeNull();
    });

    it('does not call out at all for an empty query', async () => {
      const fetchImpl = respondWith(EN);
      await expect(translator(fetchImpl).translateToEnglish('   ')).resolves.toBeNull();
      expect(fetchImpl).not.toHaveBeenCalled();
    });
  });
});

describe('sanitizeTranslation', () => {
  it('keeps a clean translation as-is', () => {
    expect(sanitizeTranslation(EN, PT)).toBe(EN);
  });

  it('strips the affixes small instruct models add', () => {
    expect(sanitizeTranslation(`"${EN}"`, PT)).toBe(EN);
    expect(sanitizeTranslation(`Translation: ${EN}`, PT)).toBe(EN);
    expect(sanitizeTranslation(`English: "${EN}"`, PT)).toBe(EN);
  });

  it('takes the first line, dropping a trailing rationale', () => {
    expect(sanitizeTranslation(`${EN}\n\nThis keeps the technical terms.`, PT)).toBe(EN);
  });

  it('rejects an echo of the original — searching it twice buys nothing', () => {
    expect(sanitizeTranslation(PT, PT)).toBeNull();
    expect(sanitizeTranslation(PT.toUpperCase(), PT)).toBeNull();
  });

  it('rejects a reply that is still in the source language', () => {
    // Verbatim from the live 1.5B sidecar (2026-08-12) for pt-upsert-qdrant:
    // it ANSWERED the question in Portuguese instead of translating it. Not an
    // echo, and only ~1.8x the input, so every other guard lets it through.
    const answered =
      'Os pontos com embeddings são enviados para o Qdrant através de uma API ou SDK específico para o Qdrant.';
    expect(sanitizeTranslation(answered, 'Onde os pontos com embeddings são enviados para o Qdrant?')).toBeNull();
  });

  it('accepts a genuine English translation of the same query', () => {
    // Guards the check above from being so eager it rejects real translations.
    const good = 'Where are points with embeddings sent to Qdrant?';
    expect(sanitizeTranslation(good, 'Onde os pontos com embeddings são enviados para o Qdrant?')).toBe(good);
  });

  it('rejects a reply that ran away instead of translating', () => {
    expect(sanitizeTranslation('x'.repeat(500), PT)).toBeNull();
    // Disproportionate to the input even while under the absolute cap.
    expect(sanitizeTranslation('a '.repeat(60), 'curto')).toBeNull();
  });

  it('rejects empty and whitespace-only replies', () => {
    expect(sanitizeTranslation('', PT)).toBeNull();
    expect(sanitizeTranslation('   \n  ', PT)).toBeNull();
    expect(sanitizeTranslation('""', PT)).toBeNull();
  });
});

describe('createQueryTranslatorFromEnv', () => {
  it('returns null when the base URL is unset or blank — translation off by default', () => {
    expect(createQueryTranslatorFromEnv({})).toBeNull();
    expect(createQueryTranslatorFromEnv({ [TRANSLATE_BASE_URL_ENV]: '   ' })).toBeNull();
  });

  it('returns null when a base URL is set but the model is not', () => {
    // Half-configured must stay OFF rather than guess a model id.
    expect(
      createQueryTranslatorFromEnv({ [TRANSLATE_BASE_URL_ENV]: 'http://wqm-llm-gpu:8080' })
    ).toBeNull();
  });

  it('builds a translator when both are set', () => {
    const built = createQueryTranslatorFromEnv({
      [TRANSLATE_BASE_URL_ENV]: 'http://wqm-llm-gpu:8080',
      [TRANSLATE_MODEL_ENV]: 'Qwen/Qwen2.5-1.5B-Instruct-GGUF',
    });
    expect(built?.endpointUrl).toBe('http://wqm-llm-gpu:8080/v1/chat/completions');
  });
});
