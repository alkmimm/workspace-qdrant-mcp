/**
 * Cheap, local language gate for search queries.
 *
 * WHY — see docs/plans/2026-08-11-query-translation-plan.md. A Portuguese query
 * against this English code corpus loses to LANGUAGE HOMOPHILY: the 8
 * predominantly-Portuguese docs in the repo take 16.3% of top-10 slots on `pt-`
 * benchmark queries against 1.1% on English ones (14.5x over-represented, from
 * 0.4% of indexed files), and in 5 of 10 failures one sits in the top 3,
 * displacing the correct code. The embedder retrieves Portuguese text BECAUSE
 * it is Portuguese.
 *
 * The fix is to also search a translated copy of the query. Translation costs an
 * SLM round-trip, so this gate decides — with no network and no model — whether
 * that cost is worth paying. English queries, the common path, must not pay it.
 *
 * DESIGN: biased toward answering ENGLISH when unsure.
 *   - a false "non-English" wastes a round-trip and translates already-English
 *     text;
 *   - a false "English" merely preserves today's behaviour.
 * Neither is catastrophic, but the first is a regression in the common path, so
 * evidence is required to leave it: a query must show a positive Romance signal,
 * never merely an absence of English. That is what keeps identifier queries
 * ("applyRRFFusion implementation") and keyword-shaped ones ("reciprocal rank
 * fusion dense sparse") — which carry no function words at all — on the English
 * path by default.
 *
 * Scope is deliberately Portuguese/Spanish, the languages this deployment
 * actually sees. It is a routing heuristic, not language identification.
 */

/**
 * Letters that essentially do not occur in English prose but are common in
 * Portuguese and Spanish. A single one is strong evidence on its own.
 */
const ROMANCE_DIACRITICS = /[áàâãéêíóôõúüçñ]/i;

/**
 * Portuguese/Spanish function words with no English collision.
 *
 * Deliberately excludes forms that are also English words — notably "a" (PT
 * article, EN article) and "e"/"o" in isolation — since a single shared token
 * would fire the gate on an English query.
 */
const ROMANCE_WORDS = new Set([
  'onde', 'como', 'qual', 'quais', 'quando', 'porque', 'por que', 'quem',
  'que', 'não', 'nao', 'são', 'sao', 'está', 'esta', 'estão', 'este', 'essa', 'esse', 'isso',
  'do', 'da', 'dos', 'das', 'no', 'na', 'nos', 'nas', 'ao', 'aos', 'às',
  'pelo', 'pela', 'para', 'com', 'sem', 'sobre', 'entre',
  'um', 'uma', 'uns', 'umas', 'os', 'as',
  'é', 'ser', 'foi', 'tem', 'faz', 'deve', 'pode', 'usa', 'usar',
  'mais', 'menos', 'muito', 'cada', 'todo', 'toda', 'todos', 'todas',
  'arquivo', 'arquivos', 'busca', 'consulta', 'fila', 'código', 'codigo',
]);

/** English function words. Presence pushes back toward the English path. */
const ENGLISH_WORDS = new Set([
  'where', 'how', 'what', 'which', 'when', 'who', 'why', 'whose',
  'the', 'is', 'are', 'was', 'were', 'does', 'do', 'did', 'can', 'should', 'would',
  'in', 'on', 'for', 'with', 'that', 'this', 'these', 'those', 'from', 'into',
  'and', 'or', 'not', 'of', 'to', 'at', 'by', 'as', 'be', 'been',
  'implementation', 'search', 'file', 'files', 'query', 'code',
]);

/**
 * Portuguese morphology with essentially no English counterpart: gerunds
 * (-ando/-endo/-indo), -ção/-ções, -mente, -dade, -agem, -ável/-ível.
 *
 * Enumerating function words alone left real Portuguese queries below the floor
 * — "Onde o daemon gera embeddings densos usando FastEmbed ONNX?" matches only
 * `onde`, since `gera`/`densos`/`usando` are content words. Morphology
 * generalizes where a word list cannot, and it is what keeps the floor at 2
 * instead of dropping it to 1 (which would fire on English keyword queries like
 * "no cache", where `no` collides with the Portuguese article).
 *
 * Requires 2+ leading characters so a short English word cannot BE a suffix
 * ("undo", "end"). The bar stays low because a lone morphological hit is worth
 * only one point and still has to clear the floor of 2 and outnumber the
 * English signal — "commando" on its own never routes a query.
 */
const ROMANCE_MORPHOLOGY = /^.{2,}(ando|endo|indo|ções|ção|mente|dade|agem|ável|ível)$/u;

/** Minimum distinct Romance function words required when no diacritic is present. */
const ROMANCE_WORD_FLOOR = 2;

export interface QueryLanguageVerdict {
  /** True when the query should be routed through translation. */
  isLikelyNonEnglish: boolean;
  /** Distinct Romance function words matched. */
  romanceWords: number;
  /** Distinct English function words matched. */
  englishWords: number;
  /** Whether a Romance diacritic was present. */
  hasDiacritic: boolean;
  /** Short, human-readable justification — surfaced in logs/telemetry. */
  reason: string;
}

/**
 * Tokenize for function-word matching: lowercase, split on anything that is not
 * a letter (Unicode-aware, so accented forms survive) — identifiers like
 * `applyRRFFusion` or `SPARSE_ONLY_WEIGHT` fall apart into non-words, which is
 * exactly right here: they are evidence of neither language.
 */
function functionWordTokens(query: string): string[] {
  return query
    .toLowerCase()
    .split(/[^\p{L}]+/u)
    .filter((token) => token.length > 0);
}

/**
 * Decide whether `query` should be routed through translation.
 *
 * Pure and allocation-light — safe to call on every search.
 */
export function classifyQueryLanguage(query: string): QueryLanguageVerdict {
  const tokens = functionWordTokens(query);
  const distinct = new Set(tokens);

  let romanceWords = 0;
  let englishWords = 0;
  for (const token of distinct) {
    // A token counts once: as a function word, or failing that, on morphology.
    if (ROMANCE_WORDS.has(token) || ROMANCE_MORPHOLOGY.test(token)) romanceWords += 1;
    if (ENGLISH_WORDS.has(token)) englishWords += 1;
  }

  const hasDiacritic = ROMANCE_DIACRITICS.test(query);

  // A diacritic is strong evidence, but not licence to ignore a query that is
  // otherwise plainly English (an English sentence quoting one accented term).
  if (hasDiacritic && romanceWords >= englishWords) {
    return {
      isLikelyNonEnglish: true,
      romanceWords,
      englishWords,
      hasDiacritic,
      reason: `Romance diacritic with ${romanceWords} Romance vs ${englishWords} English function words`,
    };
  }

  // No diacritic: require a real Romance signal that also beats the English one.
  if (romanceWords >= ROMANCE_WORD_FLOOR && romanceWords > englishWords) {
    return {
      isLikelyNonEnglish: true,
      romanceWords,
      englishWords,
      hasDiacritic,
      reason: `${romanceWords} Romance function words vs ${englishWords} English`,
    };
  }

  return {
    isLikelyNonEnglish: false,
    romanceWords,
    englishWords,
    hasDiacritic,
    reason:
      romanceWords === 0 && englishWords === 0
        ? 'no function words in either language — defaulting to English'
        : `English path (${englishWords} English vs ${romanceWords} Romance function words)`,
  };
}
