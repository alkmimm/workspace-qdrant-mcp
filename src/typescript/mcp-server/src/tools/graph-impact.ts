/**
 * `graph` actions `impact` and `usages` — both wrap ImpactAnalysis (reverse
 * reachability over the graph), but differ in DEPTH:
 *   impact → transitive blast-radius (`maxHops`, default 3): all that breaks
 *            if you change X, direct AND indirect.
 *   usages → DIRECT references only (one hop): "who references X" (the IDE
 *            find-references).
 * Precision improves once the LSP call-hierarchy pass resolves CALLS edges.
 */

import type { DaemonClient } from '../clients/daemon-client.js';
import type { ImpactAnalysisRequest, ImpactAnalysisResponse } from '../clients/grpc-types.js';
import { maxHopsArg, minConfidenceArg, num, str, type JsonObject } from './graph-args.js';

export function graphNoEdgesHint(kind: 'usages' | 'impact' | 'relations'): string {
  const what =
    kind === 'relations'
      ? 'no outgoing dependency edges'
      : kind === 'usages'
        ? 'no direct references'
        : 'no dependents';
  return (
    `Graph shows ${what} here, but it only has CALL / USES_TYPE / IMPORTS / inheritance edges over ` +
    `callable symbols. Field/property/provider access, dynamic dispatch, string-keyed DI, and ` +
    `generated-code references are NOT edges — this 0 does NOT prove the symbol is unused. Confirm ` +
    `with the grep tool before concluding "no callers/usages".`
  );
}

/**
 * How many times the symbol appears in the text index, or `undefined` when the
 * probe could not run.
 *
 * Called only on the `usages` zero-result path, which by definition has nothing
 * to show — so one extra query buys the difference between "no edge models this"
 * and "nothing references this". `textSearchCount` returns a count without
 * transferring any match bodies.
 *
 * Every failure mode is swallowed on purpose. This is a courtesy on an already
 * empty answer; it must never turn a valid empty result into an error.
 */
async function countTextOccurrences(
  daemonClient: DaemonClient | undefined,
  symbol: string,
  tenantId: string | undefined
): Promise<number | undefined> {
  if (!daemonClient) return undefined;
  try {
    const response = await daemonClient.textSearchCount({
      pattern: symbol,
      regex: false,
      case_sensitive: true,
      context_lines: 0,
      max_results: 1,
      ...(tenantId ? { tenant_id: tenantId } : {}),
    });
    return response?.count;
  } catch {
    return undefined;
  }
}

/**
 * The cuts the daemon made that the node list alone cannot show. Field
 * feedback 2026-10-07: a pinned `filePath` silently dropped every caller that
 * reached the definition only through an ambiguous same-name call, so a tested
 * method's test never appeared and the answer read as complete.
 */
function cutHints(r: ImpactAnalysisResponse, requestedHops: number | undefined): string[] {
  const hints: string[] = [];
  const dropped = r.dropped_below_confidence_floor ?? 0;
  if (dropped > 0) {
    hints.push(
      `${dropped} caller(s) reach this definition only through an ambiguous same-name call ` +
        `(confidence < 0.6: the call could resolve to several definitions named the same). ` +
        `filePath pins one definition, so they were left out; omit filePath to list every ` +
        `candidate caller with its confidence.`
    );
  }
  if (r.node_budget_reached === true) {
    hints.push(
      `The walk stopped at the daemon's node budget, so this blast radius is truncated at the ` +
        `source — lower maxHops or pin filePath for a complete answer.`
    );
  }
  if (requestedHops !== undefined && r.max_hops !== undefined && r.max_hops < requestedHops) {
    hints.push(`maxHops:${requestedHops} was capped at ${r.max_hops}, the deepest walk supported.`);
  }
  return hints;
}

/** `minConfidence` given but nothing removed: say so (#383). */
function noopFilterHint(minConfidence: number): string {
  return (
    `minConfidence:${minConfidence} removed nothing — this result is identical to ` +
    `omitting it. Reported confidences are the best-path edge-weight product and ` +
    `in practice cluster at 1.00 (precise), 0.97 (typed receiver), 0.95, 0.85, with ` +
    `0.7 for a tenant-unique name; a same-name fan-out scores ~1/N. Useful cut points ` +
    `start around 0.85, not 0.5.`
  );
}

export async function runImpactAction(
  action: 'impact' | 'usages',
  args: JsonObject,
  tenant: string,
  branch: string | undefined,
  daemonClient: DaemonClient
): Promise<Record<string, unknown>> {
  const symbol = str(args, 'symbol');
  if (!symbol) throw new Error(`graph action '${action}' requires \`symbol\``);
  const filePath = str(args, 'filePath');
  // Precision filter: drop nodes below this best-path confidence at the
  // daemon (before top_k + total_impacted). Omitted = all. See tool desc.
  const minConfidence = minConfidenceArg(args);
  // `usages` is one hop by definition; `impact` walks `maxHops` (daemon
  // default 3 when omitted).
  const maxHops = action === 'usages' ? 1 : maxHopsArg(args);
  // Bound the impacted-node list: the daemon caps to top_k (nearest-by-depth
  // first) and still returns the true total_impacted. topK<=0 = all.
  const topK = num(args, 'topK') ?? 50;
  const req: ImpactAnalysisRequest = {
    tenant_id: tenant,
    ...(branch !== undefined ? { branch } : {}),
    symbol_name: symbol,
    top_k: topK,
    ...(filePath ? { file_path: filePath } : {}),
    ...(minConfidence !== undefined ? { min_confidence: minConfidence } : {}),
    ...(maxHops !== undefined ? { max_hops: maxHops } : {}),
  };
  const r = await daemonClient.impactAnalysis(req);
  const hints = cutHints(r, action === 'impact' ? maxHops : undefined);
  if (minConfidence !== undefined && (r.filtered_by_min_confidence ?? 0) === 0) {
    hints.push(noopFilterHint(minConfidence));
  }
  if (action === 'usages') {
    return usagesAnswer(r, symbol, tenant, topK, hints, daemonClient);
  }
  // The daemon caps at top_k AFTER ordering nearest-first, so a page that came
  // back full may have cut nodes that did not fit. Only an unsaturated page
  // proves the list is complete. Presenting a capped page as the whole answer
  // is what let `usages` return a different arbitrary subset on every call and
  // still read as authoritative (issue #367).
  const returnedCount = (r.impacted_nodes ?? []).length;
  const truncated = topK > 0 && returnedCount >= topK;
  if (truncated) {
    hints.unshift(
      `Truncated at topK=${topK}: more nodes exist beyond the cap. Re-run with a ` +
        `larger topK (topK:0 removes the cap) before treating this list as complete.`
    );
  }
  if ((r.total_impacted ?? returnedCount) === 0) {
    hints.push(graphNoEdgesHint('impact'));
  }
  return {
    success: true,
    action,
    tenant_id: tenant,
    symbol,
    ...r,
    truncated,
    ...(hints.length > 0 ? { hint: hints.join(' ') } : {}),
  };
}

async function usagesAnswer(
  r: ImpactAnalysisResponse,
  symbol: string,
  tenant: string,
  topK: number,
  hints: string[],
  daemonClient: DaemonClient
): Promise<Record<string, unknown>> {
  // A daemon that honours `max_hops` walked one hop and echoes 1: every node
  // is a direct reference and `total_impacted` is their true count. An older
  // daemon walked three hops, so the direct references are filtered here from
  // a transitive page whose cap applied across ALL depths.
  const oneHop = r.max_hops === 1;
  const all = r.impacted_nodes ?? [];
  const direct = oneHop ? all : all.filter((n) => n.distance === 1);
  const truncated = oneHop
    ? (r.total_impacted ?? all.length) > all.length
    : topK > 0 && all.length >= topK;
  if (truncated) {
    hints.unshift(
      oneHop
        ? `Truncated at topK=${topK}: ${r.total_impacted} direct references exist and ` +
            `${direct.length} are listed. Re-run with a larger topK (topK:0 removes the cap) ` +
            `before treating this list as complete.`
        : // `usages` needs its OWN wording here. The cap applied to the
          // TRANSITIVE page, and only afterwards did the distance===1 filter
          // run — so a reader sees a count well below topK next to
          // `truncated: true`. Name both numbers so the arithmetic is visible.
          `Truncated: the daemon returned the first ${topK} impacted nodes across ALL ` +
            `depths (nearest first), and ${direct.length} of those are DIRECT references. ` +
            `Further direct references may sit beyond that cap — re-run with a larger ` +
            `topK (topK:0 removes it) before treating this list as complete.`
    );
  }
  // A zero here is the trap this tool is repeatedly reported for. The graph
  // only models CALLS / USES_TYPE / IMPORTS, so an idiom that REFERENCES a
  // symbol without invoking it produces no edge at all: `ref.watch(provider)`
  // passes the symbol as an argument, and Flutter's `find.byType(Widget)`
  // asserts on a type without constructing it. Measured on DOC-V2:
  // `activeContextProvider` existed as a node with ZERO incoming edges against
  // 71 references on disk. Counting the text index turns an ambiguous zero
  // into a specific statement.
  let textOccurrences: number | undefined;
  if (direct.length === 0) {
    textOccurrences = await countTextOccurrences(daemonClient, symbol, tenant);
    hints.push(graphNoEdgesHint('usages'));
    if (textOccurrences !== undefined && textOccurrences > 0) {
      hints.push(
        `The text index holds ${textOccurrences} occurrence(s) of "${symbol}", so this 0 ` +
          `means NOT MODELLED rather than unused. A symbol passed by reference is modelled ` +
          `for Dart lower-camel identifiers (REFERENCES edges), but NOT for other ` +
          `languages, and not for type-shaped names such as find.byType(X) in any ` +
          `language. Existing data also needs a graph rebuild before those edges appear. ` +
          `Use grep for the sites.`
      );
    }
  }
  return {
    success: true,
    action: 'usages',
    tenant_id: tenant,
    symbol,
    ...r,
    impacted_nodes: direct,
    // The count of direct references: exact from a one-hop walk; from a
    // transitive page, what was returned — a FLOOR when the page was
    // truncated, which `truncated` says rather than letting the number pass
    // for a complete answer.
    total_impacted: oneHop ? (r.total_impacted ?? direct.length) : direct.length,
    truncated,
    ...(truncated && !oneHop ? { total_impacted_all_depths: r.total_impacted } : {}),
    ...(textOccurrences !== undefined ? { text_occurrences: textOccurrences } : {}),
    ...(hints.length > 0 ? { hint: hints.join(' ') } : {}),
  };
}
