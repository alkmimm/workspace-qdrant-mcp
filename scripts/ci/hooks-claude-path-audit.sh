#!/usr/bin/env bash
# hooks-claude-path-audit.sh — no hardcoded `.claude` paths in the hooks module.
#
# The hooks installer must resolve the Claude config directory through
# `get_claude_config_dir()` so CLAUDE_CONFIG_DIR (Claude Code Enterprise and
# custom installs, where the directory is NOT `~/.claude`) is honored. A
# hardcoded `.claude` literal silently installs hooks into a directory the
# user's Claude never reads — it "succeeds" and does nothing.
#
# `settings.rs` is exempt: it is the module that defines the resolution itself.
# Comment lines are exempt: prose may legitimately mention the path.
#
# Extracted verbatim from the `hooks-claude-path-audit` job in
# .github/workflows/ci.yml. That workflow does NOT run on this fork (GitHub
# Actions are disabled — the container build is the CI), so the check lived
# nowhere; it now runs in docker/Dockerfile.memexd, which builds the CLI that
# owns this module.
#
# Usage: bash scripts/ci/hooks-claude-path-audit.sh [<project_root>]

set -euo pipefail

ROOT="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
HOOKS_DIR="$ROOT/src/rust/cli/src/commands/hooks"

echo "=== Hooks .claude Path Audit ==="
echo "Scanning: $HOOKS_DIR"
echo ""

if [ ! -d "$HOOKS_DIR" ]; then
	echo "  ERROR: hooks module not found at $HOOKS_DIR" >&2
	echo "  (moved or renamed? update this check rather than deleting it)" >&2
	exit 1
fi

hits=$(grep -rn '\.claude' "$HOOKS_DIR" --include='*.rs' \
	| grep -v 'CLAUDE_CONFIG_DIR' \
	| grep -Ev "^[^:]+:[0-9]+:[[:space:]]*(//|///|\*)" \
	| grep -v 'settings.rs' \
	|| true)

if [ -n "$hits" ]; then
	echo "  ERROR: hardcoded .claude path(s) outside settings.rs:" >&2
	echo "$hits" >&2
	echo "" >&2
	echo "  Use get_claude_config_dir() so CLAUDE_CONFIG_DIR installs are honored." >&2
	echo "=== Hooks path audit FAILED ===" >&2
	exit 1
fi

echo "  OK: no hardcoded .claude paths outside settings.rs"
echo "=== Hooks path audit passed ==="
