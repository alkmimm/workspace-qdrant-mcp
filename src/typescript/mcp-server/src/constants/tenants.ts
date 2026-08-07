/**
 * Tenant identifier constants shared across rules, scratchpad, and search paths.
 *
 * Mirrors the Rust constant `wqm_common::constants::TENANT_GLOBAL`. Use this
 * import wherever the literal `'global'` would be written as the `tenant_id`
 * sentinel for rules/scratchpad entries that apply across all projects.
 */

/**
 * Sentinel `tenant_id` for global-scope rules and scratchpad entries.
 *
 * Typed as a string literal via `as const` so it can be assigned to
 * positions typed `'global' | 'project'` or similar discriminated unions.
 */
export const TENANT_GLOBAL = 'global' as const;

export type TenantGlobal = typeof TENANT_GLOBAL;

/**
 * Dedicated `tenant_id` bucket for agent tool-usage feedback (store type:"feedback").
 *
 * A synthetic tenant (no project maps to it) so feedback about the workspace-qdrant
 * tooling aggregates in ONE place and is automatically isolated from every
 * project-scoped surface: the semantic recall lane and `scratchpad list` are
 * tenant-strict, so a note under this tenant never leaks into code search or a
 * project's scratchpad. `/feedback-review` targets it explicitly. TS-only — the
 * daemon treats it as an opaque tenant_id, exactly like the 'global' sentinel.
 */
export const TENANT_FEEDBACK = 'workspace-qdrant-feedback' as const;
