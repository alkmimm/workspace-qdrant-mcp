/**
 * Rule fetching and formatting for Claude Agent SDK integration.
 */

import { loadConfig } from './config.js';
import { SqliteStateManager } from './clients/sqlite-state-manager.js';
import { DaemonClient } from './clients/daemon-client.js';
import { ProjectDetector } from './utils/project-detector.js';
import { RulesTool, type Rule } from './tools/rules.js';
import { TENANT_GLOBAL } from './constants/tenants.js';
import { DEFAULT_CONFIG } from './types/generated-defaults.js';

export type { Rule };

/** Build a RulesTool instance from config. */
function buildRulesTool(config: ReturnType<typeof loadConfig>): RulesTool {
  // See server-factory.ts for the rationale on the 30s floor (LSP startup).
  const daemonTimeoutMs = Number(process.env['WQM_DAEMON_TIMEOUT_MS'] ?? '30000');
  const daemonClient = new DaemonClient({
    port: config.daemon.grpcPort,
    timeoutMs:
      Number.isFinite(daemonTimeoutMs) && daemonTimeoutMs > 0 ? daemonTimeoutMs : 30000,
  });
  const stateManager = new SqliteStateManager({
    dbPath: config.database.path.replace('~', process.env['HOME'] ?? ''),
  });
  stateManager.setDaemonClient(daemonClient);
  stateManager.initialize();
  const projectDetector = new ProjectDetector();
  // No `qdrantTimeout` here on purpose: the client factory resolves it
  // (WQM_QDRANT_TIMEOUT_MS → DEFAULT_QDRANT_TIMEOUT_MS), so this path cannot
  // pin a timeout the rest of the server does not share.
  const rulesToolConfig = {
    qdrantUrl: config.qdrant?.url ?? DEFAULT_CONFIG.qdrant.url,
  } as { qdrantUrl: string; qdrantApiKey?: string; qdrantTimeout?: number };
  if (config.qdrant?.apiKey) rulesToolConfig.qdrantApiKey = config.qdrant.apiKey;
  return new RulesTool(rulesToolConfig, daemonClient, stateManager, projectDetector);
}

/** Comparator: sort rules by priority desc then creation date desc. */
function ruleComparator(a: Rule, b: Rule): number {
  const priorityDiff = (b.priority ?? 0) - (a.priority ?? 0);
  if (priorityDiff !== 0) return priorityDiff;
  const aDate = a.createdAt ? new Date(a.createdAt).getTime() : 0;
  const bDate = b.createdAt ? new Date(b.createdAt).getTime() : 0;
  return bDate - aDate;
}

/**
 * Fetch rules from Qdrant via RulesTool.
 * Fetches both global and project-specific rules (if project detected).
 * Rules are sorted by priority (highest first) then by creation date (newest first).
 */
export async function fetchRules(
  projectId: string | null,
  config: ReturnType<typeof loadConfig>
): Promise<Rule[]> {
  const rules: Rule[] = [];
  try {
    const rulesTool = buildRulesTool(config);

    // The two scopes are independent — fetch them concurrently. Each list is a
    // scroll behind a 2500ms cold-start deadline, so sequential fetches doubled
    // the worst-case session-start stall; ruleComparator restores ordering.
    const [globalResponse, projectResponse] = await Promise.all([
      rulesTool.execute({ action: 'list', scope: TENANT_GLOBAL }),
      projectId
        ? rulesTool.execute({ action: 'list', scope: 'project', projectId })
        : Promise.resolve(undefined),
    ]);

    for (const [label, response] of [
      ['global', globalResponse],
      [`project ${projectId ?? ''}`, projectResponse],
    ] as const) {
      if (!response?.success || !response.rules) continue;
      rules.push(...response.rules);
      console.log(`[Agent] Fetched ${response.rules.length} ${label} rule(s)`);
      // The fetch cap (RULES_LIST_FETCH_LIMIT) is a cliff, not a guarantee.
      // If the listing indicates a tail we did not receive, say so — a silent
      // partial injection is a convention the agent never learns, and this
      // path has no user-visible drop accounting of its own.
      if (response.next_cursor || (response.total ?? 0) > response.rules.length) {
        console.warn(
          `[Agent] WARNING: ${label} rules listing is TRUNCATED ` +
            `(injected ${response.rules.length} of ${response.total ?? '?'}); ` +
            `the remainder will not be in the system prompt`
        );
      }
    }

    rules.sort(ruleComparator);
    console.log(`[Agent] Total rules fetched: ${rules.length}`);
    return rules;
  } catch (error) {
    console.error('[Agent] Error fetching rules:', error);
    return rules;
  }
}

/** Format rules for system prompt injection. */
export function formatRulesForPrompt(rules: Rule[]): string {
  if (rules.length === 0) return '';

  const lines: string[] = [
    '# Behavioral Rules',
    '',
    'The following behavioral rules have been configured and should be followed:',
    '',
  ];

  const formatSection = (title: string, sectionRules: Rule[]) => {
    if (sectionRules.length === 0) return;
    lines.push(`## ${title}`, '');
    sectionRules.forEach((rule, index) => {
      const heading = rule.title ? `**${rule.title}**` : `Rule ${index + 1}`;
      const priority = rule.priority !== undefined ? ` [Priority: ${rule.priority}]` : '';
      lines.push(`### ${heading}${priority}`, '', rule.content ?? '', '');
    });
  };

  formatSection(
    'Global Rules',
    rules.filter((r) => r.scope === TENANT_GLOBAL)
  );
  formatSection(
    'Project-Specific Rules',
    rules.filter((r) => r.scope === 'project')
  );

  return lines.join('\n');
}
