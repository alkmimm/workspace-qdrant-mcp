import { describe, expect, it, vi } from 'vitest';

import type { QueryTranslator } from '../../src/clients/query-translator.js';
import {
  queryLanguageVerdicts,
  queryTranslationOutcomes,
  register,
  translatedLegHits,
} from '../../src/telemetry/metrics.js';
import {
  QUERY_TRANSLATE_ENV,
  resolveTranslatedQuery,
} from '../../src/tools/search-translated-leg.js';

const PT = 'Onde a vazão da fila é medida no daemon?';
const EN = 'Where is the queue throughput measured in the daemon?';
const ON = { [QUERY_TRANSLATE_ENV]: '1' };

function fakeTranslator(result: string | null): QueryTranslator {
  return { translateToEnglish: vi.fn(async () => result) } as unknown as QueryTranslator;
}

/**
 * Sum of every counter series matching one label value.
 *
 * Summing rather than picking the first match: `queryLanguageVerdicts` is split
 * by `actor` as well, so a single verdict spans several series and a `.find()`
 * would silently read whichever one happened to come first — usually a zero.
 */
async function counterValue(
  metric: { get(): Promise<{ values: Array<{ labels: Record<string, unknown>; value: number }> }> },
  label: string,
  value: string
): Promise<number> {
  const data = await metric.get();
  return data.values
    .filter((v) => v.labels[label] === value)
    .reduce((total, v) => total + v.value, 0);
}

/**
 * Deltas rather than `register.resetMetrics()` between tests: a reset would
 * also wipe the pre-initialised zero series that two of these tests exist to
 * verify, and it makes the suite order-dependent. Measuring the change across a
 * call sidesteps both.
 */
async function delta<T>(
  metric: Parameters<typeof counterValue>[0],
  label: string,
  value: string,
  fn: () => Promise<T>
): Promise<number> {
  const before = await counterValue(metric, label, value);
  await fn();
  return (await counterValue(metric, label, value)) - before;
}

describe('query-translation metrics', () => {
  it('records the language verdict even while the feature is DISABLED', async () => {
    // The reason the whole instrumentation exists: the decision number has to
    // be collectable before committing to the VRAM and latency cost, which
    // means it cannot live behind the flag it is meant to inform.
    const langDelta = await delta(queryLanguageVerdicts, 'verdict', 'non_english', () =>
      resolveTranslatedQuery(PT, null, {})
    );
    expect(langDelta).toBe(1);

    const outcomeDelta = await delta(queryTranslationOutcomes, 'outcome', 'disabled', () =>
      resolveTranslatedQuery(PT, null, {})
    );
    expect(outcomeDelta).toBe(1);
  });

  it('classifies English queries while disabled too', async () => {
    const english = await delta(queryLanguageVerdicts, 'verdict', 'english', () =>
      resolveTranslatedQuery(EN, null, {})
    );
    const nonEnglish = await delta(queryLanguageVerdicts, 'verdict', 'non_english', () =>
      resolveTranslatedQuery(EN, null, {})
    );

    expect(english).toBe(1);
    expect(nonEnglish).toBe(0);
  });

  it('separates a declined gate from a failed translation', async () => {
    // already_english is the free path; translation_failed cost a round-trip
    // and returned nothing. Collapsing them would hide a broken sidecar behind
    // a healthy-looking "no second leg" count.
    const declined = await delta(queryTranslationOutcomes, 'outcome', 'already_english', () =>
      resolveTranslatedQuery(EN, fakeTranslator(EN), ON)
    );
    const failed = await delta(queryTranslationOutcomes, 'outcome', 'translation_failed', () =>
      resolveTranslatedQuery(PT, fakeTranslator(null), ON)
    );
    const succeeded = await delta(queryTranslationOutcomes, 'outcome', 'translated', () =>
      resolveTranslatedQuery(PT, fakeTranslator(null), ON)
    );

    expect(declined).toBe(1);
    expect(failed).toBe(1);
    expect(succeeded).toBe(0);
  });

  it('records a successful translation', async () => {
    let decision: Awaited<ReturnType<typeof resolveTranslatedQuery>> | undefined;
    const succeeded = await delta(queryTranslationOutcomes, 'outcome', 'translated', async () => {
      decision = await resolveTranslatedQuery(PT, fakeTranslator(EN), ON);
    });

    expect(decision?.query).toBe(EN);
    expect(succeeded).toBe(1);
  });

  it('exposes every outcome series from startup, at zero', async () => {
    // A dashboard should read "0", not "No data", before the first search.
    const data = await queryTranslationOutcomes.get();
    const outcomes = data.values.map((v) => v.labels['outcome']).sort();

    expect(outcomes).toEqual([
      'already_english',
      'disabled',
      'no_translator',
      'translated',
      'translation_failed',
    ]);
  });

  it('attributes the verdict to the actor that searched', async () => {
    // The label exists to separate heavy development traffic from real use, so
    // a share computed over everything is not dominated by the agent's own work.
    const asUser = await delta(queryLanguageVerdicts, 'actor', 'user', () =>
      resolveTranslatedQuery(PT, null, {}, 'user')
    );
    const asClaude = await delta(queryLanguageVerdicts, 'actor', 'claude', () =>
      resolveTranslatedQuery(PT, null, {}, 'claude')
    );
    // Anything unrecognised collapses instead of opening cardinality.
    const asOther = await delta(queryLanguageVerdicts, 'actor', 'other', () =>
      resolveTranslatedQuery(PT, null, {}, 'some-new-caller')
    );

    expect([asUser, asClaude, asOther]).toEqual([1, 1, 1]);
  });

  it('routes harness traffic to its own series, not the human one', async () => {
    // The defect this guards: the eval harness is FORCED to report
    // telemetryActor 'user' by the search_events CHECK, so trusting the actor
    // alone puts 33 Portuguese benchmark queries into the exact series the
    // dashboard headlines as real usage — inventing the signal it is read for.
    const asBenchmark = await delta(queryLanguageVerdicts, 'actor', 'benchmark', () =>
      resolveTranslatedQuery(PT, null, {}, 'user', true)
    );
    const asUser = await delta(queryLanguageVerdicts, 'actor', 'user', () =>
      resolveTranslatedQuery(PT, null, {}, 'user', true)
    );

    expect(asBenchmark).toBe(1);
    expect(asUser).toBe(0);
  });

  it('exposes both leg-contribution series from startup', async () => {
    const data = await translatedLegHits.get();
    expect(data.values.map((v) => v.labels['agreement']).sort()).toEqual([
      'both_legs',
      'translated_only',
    ]);
  });

  it('keeps outcomes a partition — one per search, never double-counted', async () => {
    // `leg_skipped` is a SEPARATE counter rather than a sixth outcome for this
    // reason: a search that translated and then could not run its leg would be
    // counted twice here, and the outcome distribution would stop summing to
    // the number of searches.
    const before = await register.getMetricsAsJSON();
    const outcomes = before.find((m) => m.name === 'wqm_mcp_query_translation_total');
    const outcomeNames = (outcomes?.values ?? []).map((v) => v.labels['outcome']).sort();

    expect(outcomeNames).toEqual([
      'already_english',
      'disabled',
      'no_translator',
      'translated',
      'translation_failed',
    ]);
    expect(outcomeNames).not.toContain('leg_skipped');
  });

  it('uses metric names the dashboard queries', async () => {
    // Guards the dashboard JSON, which references these by string and would
    // silently render "No data" if a name drifted.
    const names = (await register.getMetricsAsJSON()).map((m) => m.name);

    expect(names).toContain('wqm_mcp_query_language_total');
    expect(names).toContain('wqm_mcp_query_translation_total');
    expect(names).toContain('wqm_mcp_translated_leg_hits_total');
    expect(names).toContain('wqm_mcp_translated_leg_skipped_total');
    expect(names).toContain('wqm_mcp_query_translation_duration_seconds');
  });
});
