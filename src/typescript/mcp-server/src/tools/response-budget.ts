/**
 * Shared response byte-budget helper (spec 20 token economy).
 *
 * Bounds the TOTAL payload of a tool response: per-item caps alone cannot —
 * N items at the cap still sum to N×cap. Trailing items are dropped once the
 * cumulative size would exceed the budget (the head of the list is assumed
 * to be the ranked/most-relevant prefix), and at least one item is always
 * kept so a budget can never blank a non-empty response. A non-positive
 * budget disables the trim.
 *
 * Shared by search shaping (`search-shaping.ts`) and grep shaping
 * (`grep.ts`) so both tools enforce budgets with identical semantics.
 */
export function applyByteBudget<T>(
  items: readonly T[],
  sizeOf: (item: T) => number,
  budget: number
): { kept: T[]; dropped: number } {
  if (budget <= 0) return { kept: [...items], dropped: 0 };
  let running = 0;
  const kept: T[] = [];
  for (const item of items) {
    const bytes = sizeOf(item);
    if (kept.length > 0 && running + bytes > budget) break;
    running += bytes;
    kept.push(item);
  }
  return { kept, dropped: items.length - kept.length };
}
