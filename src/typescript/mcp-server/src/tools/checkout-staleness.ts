/**
 * Stale-checkout warning for project-scoped read responses.
 *
 * When a project's main folder has its trunk checked out, that disk is what
 * the daemon indexes for the trunk — and every branch view fills unchanged
 * files in from it. The daemon never moves a user's working tree, so a main
 * checkout nobody pulls keeps the trunk's index at an old commit. On
 * 2026-11-12 DOC-V2's main checkout sat 412 commits behind `origin/main`
 * (emnify 274) while the agents worked on branches cut from origin; search and
 * grep answered from code months old, and nothing said so.
 *
 * The daemon reports the lag (git only, no fetch) with the project status;
 * this turns it into an in-band warning the agent can pass on.
 */

import type { GetProjectStatusResponse } from '../clients/grpc-types.js';
import type { IndexingProgress } from './search-types.js';

/** Default lag, in commits, from which the warning is attached. A handful of
 *  commits is the normal gap between pulls; 20 is a stale checkout. */
export const DEFAULT_CHECKOUT_BEHIND_WARN_COMMITS = 20;

/** The main checkout's trunk against `origin/<trunk>`. */
export interface CheckoutLag {
  trunk: string;
  behind: number;
  ahead: number;
}

/** Attached as `checkout_stale` when the main checkout trails origin. */
export interface CheckoutStaleWarning extends CheckoutLag {
  message: string;
}

/**
 * Threshold from `WQM_CHECKOUT_BEHIND_WARN_COMMITS`: a positive number of
 * commits, `0` to switch the warning off. Unset or unparsable keeps the
 * default, so a typo can never silence it.
 */
export function checkoutBehindWarnCommits(
  env: Record<string, string | undefined> = process.env
): number {
  const raw = env['WQM_CHECKOUT_BEHIND_WARN_COMMITS']?.trim();
  if (raw === undefined || raw === '') return DEFAULT_CHECKOUT_BEHIND_WARN_COMMITS;
  const n = Number(raw);
  if (!Number.isFinite(n) || n < 0) return DEFAULT_CHECKOUT_BEHIND_WARN_COMMITS;
  return n === 0 ? Number.POSITIVE_INFINITY : n;
}

/** The lag carried by a (normalized) project status; absent when the daemon
 *  did not report one (main folder on another branch, no origin, older daemon). */
export function checkoutLagOf(status: GetProjectStatusResponse): CheckoutLag | undefined {
  const { checkout_trunk: trunk, checkout_behind_commits: behind } = status;
  if (typeof trunk !== 'string' || trunk === '' || typeof behind !== 'number') return undefined;
  return { trunk, behind, ahead: status.checkout_ahead_commits ?? 0 };
}

/** The warning for `progress`, or `undefined` when the checkout is current enough. */
export function checkoutStaleWarning(
  progress: IndexingProgress | null | undefined,
  threshold: number = checkoutBehindWarnCommits()
): CheckoutStaleWarning | undefined {
  const lag = progress?.checkout_lag;
  if (lag === undefined || lag.behind < threshold) return undefined;
  const diverged =
    lag.ahead > 0
      ? ` It also has ${lag.ahead} local commit(s) origin lacks, so it needs a merge or rebase, not a fast-forward.`
      : '';
  return {
    ...lag,
    message:
      `This project's main checkout of '${lag.trunk}' is ${lag.behind} commit(s) behind ` +
      `origin/${lag.trunk}, and the index follows that checkout: results on '${lag.trunk}', ` +
      `and files a branch did not change, reflect the old code.${diverged} Tell the user: ` +
      `pull the main checkout (\`git pull --ff-only\` on '${lag.trunk}').`,
  };
}
