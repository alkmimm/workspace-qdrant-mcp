#!/usr/bin/env bash
# forbid_canonicalize.sh — CI guard preventing new canonicalize() calls without markers.
#
# Fails CI on any std::fs::canonicalize() or .canonicalize() call that lacks
# a // CATEGORY-B: marker on the same line or within 3 lines above.
#
# Phase A behavior (T5 execution, before T6 lands):
#   --baseline mode captures current violations to a snapshot file.
#   Without --baseline, compares against the snapshot and fails only on NEW violations.
#
# After T6 removes all Category A sites, the snapshot will be empty and the script
# will enforce zero new canonicalize() calls without markers.
#
# Usage:
#   ./scripts/ci/forbid_canonicalize.sh [--baseline] [<project_root>]
#
# Exit codes:
#   0 — no new violations (or baseline mode succeeded)
#   1 — new canonicalize() calls without markers found
#
# See docs/specs/16-path-abstraction.md §3.2.2 for Category B discipline.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="${1:-.}"
BASELINE_MODE=0

# Handle --baseline flag
if [[ "${1:-}" == "--baseline" ]]; then
	BASELINE_MODE=1
	ROOT="${2:-.}"
fi

SNAPSHOT_FILE="$SCRIPT_DIR/forbid_canonicalize_baseline.txt"

echo "=== Forbid Canonicalize Check ==="
echo "Root: $ROOT"
if [[ $BASELINE_MODE -eq 1 ]]; then
	echo "Mode: BASELINE (snapshot current violations)"
else
	echo "Mode: ENFORCE (fail on new violations)"
fi
echo ""

# Find all canonicalize calls and check for CATEGORY-B marker
# Match patterns:
#   std::fs::canonicalize(
#   .canonicalize()

TEMP_VIOLATIONS="$SCRIPT_DIR/.forbid_canonicalize_temp.txt"
>"$TEMP_VIOLATIONS"

while IFS= read -r filepath; do
	[[ -z "$filepath" ]] && continue

	# Process each file to find canonicalize calls
	# Use awk to track line numbers and check for markers
	awk -v file="$filepath" '
    BEGIN {
        line_num = 0
        # Ring of recent lines. prev_lines[0] is the CURRENT line (assigned
        # below before the check), so prev_lines[N] is N lines above it and the
        # ring must hold 4 entries to reach 3 lines above.
        for (i = 0; i < 4; i++) {
            prev_lines[i] = ""
        }
    }
    {
        line_num++
        # Shift previous lines
        for (i = 3; i >= 0; i--) {
            prev_lines[i+1] = prev_lines[i]
        }
        prev_lines[0] = $0

        # Check for canonicalize call patterns
        if ($0 ~ /std::fs::canonicalize\(/ || $0 ~ /\.canonicalize\(\)/) {
            # Check if CATEGORY-B marker is on this line or within 3 lines above.
            # POSIX awk does not support `\s`; use [[:space:]] explicitly.
            has_marker = 0

            # Check current line
            if ($0 ~ /\/\/[[:space:]]*CATEGORY-B:/) {
                has_marker = 1
            }

            # Check up to 3 lines above. The bound is 3, not 2: prev_lines[0]
            # is the current line, so stopping at 2 only reached 2 lines above
            # while the contract above (and the failure message) promise 3 — a
            # 3-line justification was silently rejected as unmarked.
            if (!has_marker) {
                for (i = 0; i <= 3; i++) {
                    if (prev_lines[i] ~ /\/\/[[:space:]]*CATEGORY-B:/) {
                        has_marker = 1
                        break
                    }
                }
            }

            if (!has_marker) {
                print file ":" line_num ": " $0
            }
        }
    }
    ' "$filepath"

done < <(find "$ROOT/src/rust" -name "*.rs" -type f 2>/dev/null) >>"$TEMP_VIOLATIONS"

# Normalize the file column to a REPO-RELATIVE path (src/rust/...), stripping
# whatever ROOT the run happened to use. Without this the baseline is only
# valid for the exact ROOT that produced it: a host run (ROOT=".") writes
# "./src/rust/…" while the image build (ROOT="/repo") produces "/repo/src/rust/…",
# so every grandfathered site read as a brand-new violation and the gate failed
# on an unchanged tree.
sed -E -i 's|^[^:]*(src/rust/)|\1|' "$TEMP_VIOLATIONS"

# Count violations
VIOLATION_COUNT=$(wc -l <"$TEMP_VIOLATIONS" | xargs)

if [[ $BASELINE_MODE -eq 1 ]]; then
	# Snapshot current violations
	cp "$TEMP_VIOLATIONS" "$SNAPSHOT_FILE"
	echo "Baseline snapshot created: $SNAPSHOT_FILE"
	echo "Current violation count: $VIOLATION_COUNT"
	echo ""

	if [[ $VIOLATION_COUNT -eq 0 ]]; then
		echo "✓ No canonicalize() calls without CATEGORY-B markers."
		rm "$TEMP_VIOLATIONS"
		exit 0
	else
		echo "ℹ Baseline captured. These Category A sites will be removed in T6."
		head -20 "$SNAPSHOT_FILE"
		if [[ $VIOLATION_COUNT -gt 20 ]]; then
			echo "... and $(($VIOLATION_COUNT - 20)) more"
		fi
		rm "$TEMP_VIOLATIONS"
		exit 0
	fi
else
	# Enforce: compare against snapshot
	if [[ ! -f "$SNAPSHOT_FILE" ]]; then
		echo "ERROR: Baseline snapshot not found: $SNAPSHOT_FILE" >&2
		echo "Run with --baseline flag first to create snapshot." >&2
		rm "$TEMP_VIOLATIONS"
		exit 1
	fi

	# Find new violations not in baseline.
	#
	# The comparison key is `file + trimmed code`, deliberately NOT the line
	# number. A line-keyed baseline rots on contact: editing ANYTHING above a
	# grandfathered site shifts its line and the site reads as brand new. That
	# happened — a two-line shift in platform_tests.rs (166 -> 168) reported a
	# site already present in the baseline as a new violation, which is exactly
	# the kind of spurious red that gets a gate disabled instead of fixed.
	#
	# Trade-off, stated plainly: two byte-identical canonicalize() lines in the
	# same file now collapse to one key, so adding a duplicate of an already-
	# grandfathered line is not flagged. That is a far narrower blind spot than
	# a check that cries wolf on every unrelated edit.
	BASELINE_COUNT=$(wc -l <"$SNAPSHOT_FILE" | xargs)

	# file:line: code  ->  file\tcode  (drop field 2, the line number; keep the
	# code verbatim including any colons it contains).
	# `file:line: code` -> `file<TAB>code`, with the path normalized to
	# repo-relative so a baseline written on the host matches a run inside the
	# image build. Old baselines carrying a "./" or absolute prefix normalize
	# to the same key.
	key_of() {
		sed -E -e 's|^[^:]*(src/rust/)|\1|' \
		       -e 's/^([^:]+):[0-9]+:[[:space:]]*(.*)$/\1\t\2/' \
		       -e 's/[[:space:]]+$//' "$1" | sort -u
	}

	NEW_KEYS=$(comm -13 <(key_of "$SNAPSHOT_FILE") <(key_of "$TEMP_VIOLATIONS"))
	if [[ -z "$NEW_KEYS" ]]; then
		NEW_VIOLATIONS_COUNT=0
	else
		NEW_VIOLATIONS_COUNT=$(printf '%s\n' "$NEW_KEYS" | wc -l | xargs)
	fi

	if [[ $NEW_VIOLATIONS_COUNT -eq 0 ]]; then
		echo "✓ No new canonicalize() calls without CATEGORY-B markers."
		echo "  (Baseline contains $BASELINE_COUNT grandfathered Category A site(s))"
		rm "$TEMP_VIOLATIONS"
		exit 0
	else
		echo "✗ Found $NEW_VIOLATIONS_COUNT new canonicalize() call(s) without CATEGORY-B marker:" >&2
		# Report the CURRENT file:line for each new key, so the message is
		# clickable. Previously the count came from the file:line projection
		# while the printed list came from the full line — the two could and
		# did disagree.
		while IFS= read -r key; do
			[[ -z "$key" ]] && continue
			file="${key%%$'\t'*}"
			code="${key#*$'\t'}"
			# Keys are repo-relative; re-attach ROOT to actually open the file.
			grep -nF -- "$code" "$ROOT/$file" 2>/dev/null \
				| head -1 \
				| sed -E "s|^([0-9]+):|  $file:\1: |" >&2 \
				|| echo "  $file: $code" >&2
		done <<<"$NEW_KEYS"
		echo "" >&2
		echo "Add a // CATEGORY-B: marker if this site is safe (process-local use only)." >&2
		rm "$TEMP_VIOLATIONS"
		exit 1
	fi
fi
