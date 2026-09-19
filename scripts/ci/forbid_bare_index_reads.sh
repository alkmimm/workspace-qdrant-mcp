#!/usr/bin/env bash
# forbid_bare_index_reads.sh — every disk read that feeds the FTS5 line index
# must go through `document_processor::redaction::read_for_index`.
#
# Why: the chunker masks credentials on the text it embeds
# (`redaction::redact_for_path` in `process_file_sync_inner`), but the FTS5
# path in `strategies/processing/file/` reads the file from disk on its own.
# On 2026-09-19 the chunker hook shipped alone and `grep` (FTS5) served a
# 64-hex token for a file whose Qdrant payload showed `<redacted>`. A bare
# `fs::read_to_string` / `fs::read` in any `code_lines` writer reopens that
# hole, and nothing at compile time notices — so this does.
#
# Usage: scripts/ci/forbid_bare_index_reads.sh [<repo_root>]
# Exit 0 when every read in the guarded directories is the shared reader;
# 1 with the offending lines otherwise.

set -uo pipefail

ROOT="${1:-.}"
GUARDED=(
	"$ROOT/src/rust/daemon/core/src/strategies/processing/file"
	"$ROOT/src/rust/daemon/core/src/fts_batch_processor"
	"$ROOT/src/rust/daemon/core/src/search_db"
)

echo "=== Forbid bare index reads (FTS5 must use redaction::read_for_index) ==="
status=0
for dir in "${GUARDED[@]}"; do
	[[ -d "$dir" ]] || { echo "[FAIL] guarded directory missing: $dir"; status=1; continue; }
	# Test modules may read fixtures however they like.
	hits=$(grep -rnE 'fs::read_to_string\(|fs::read\(' "$dir" --include='*.rs' \
		| grep -vE '/tests?(_[a-z_]+)?\.rs:|/tests/' || true)
	if [[ -n "$hits" ]]; then
		echo "[FAIL] bare file read feeding the text index — use redaction::read_for_index:"
		echo "$hits" | sed 's/^/    /'
		status=1
	else
		echo "[OK]   $dir"
	fi
done

# The reader itself must still exist where the call sites expect it.
if ! grep -q 'pub async fn read_for_index' "$ROOT/src/rust/daemon/core/src/document_processor/redaction.rs"; then
	echo "[FAIL] redaction::read_for_index is gone — the guard above is meaningless without it"
	status=1
fi

[[ $status -eq 0 ]] && echo "forbid_bare_index_reads: OK"
exit $status
