#!/usr/bin/env bash
# check-version-consistency.sh
#
# Validates version consistency across configuration files and CI workflows.
# Run from the repository root.
#
# Checks:
#   1. ORT_VERSION is identical across all workflow files that use it
#   2. default_configuration.yaml tree_sitter_version matches Cargo.lock
#   3. Cargo.lock contains tree-sitter (sanity check for build.rs)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
EXIT_CODE=0

echo "=== Version Consistency Check ==="
echo ""

# ── Check 1: ONNX Runtime version consistency ──────────────────────────
#
# The REFERENCE is the version the real build uses, not a workflow constant.
# This originally compared `ORT_VERSION:` across .github/workflows/ only —
# which on this fork validates nothing, because GitHub Actions are disabled and
# the container build is the CI. Worse, it PASSED when it found no declarations
# at all, so it would have reported "OK" from inside an image where
# .github/ is not even in the build context.
#
# Reference resolution, in order:
#   1. $ONNX_VERSION — passed by docker/Dockerfile.memexd from its own ARG, so
#      the check validates the version the build is ACTUALLY compiling against.
#   2. `ARG ONNX_VERSION=` in docker/Dockerfile.memexd — for host runs.
# No reference => hard error. A version check with no reference is not a pass.

echo "--- ONNX Runtime version consistency ---"

DOCKERFILE="$REPO_ROOT/docker/Dockerfile.memexd"
ORT_REFERENCE=""
ORT_SOURCE=""

if [ -n "${ONNX_VERSION:-}" ]; then
    ORT_REFERENCE="$ONNX_VERSION"
    ORT_SOURCE="\$ONNX_VERSION (passed by the image build)"
elif [ -f "$DOCKERFILE" ]; then
    ORT_REFERENCE=$(grep -oE '^ARG ONNX_VERSION=[0-9]+\.[0-9]+\.[0-9]+' "$DOCKERFILE" \
        | head -1 | cut -d= -f2 || true)
    ORT_SOURCE="docker/Dockerfile.memexd (ARG ONNX_VERSION)"
fi

if [ -z "$ORT_REFERENCE" ]; then
    echo "  ERROR: could not determine the ONNX Runtime version the build uses." >&2
    echo "  Expected \$ONNX_VERSION in the environment, or 'ARG ONNX_VERSION=x.y.z'" >&2
    echo "  in $DOCKERFILE." >&2
    EXIT_CODE=1
else
    echo "  reference: $ORT_REFERENCE  (from $ORT_SOURCE)"

    # Cross-check any other declaration in the repo. The workflows are inert on
    # this fork, but a stale constant there is still a trap for anyone reading
    # them as documentation — flag the drift, anchored on the real build.
    MISMATCHED=0
    while IFS= read -r line; do
        [ -z "$line" ] && continue
        file=$(echo "$line" | cut -d: -f1)
        version=$(echo "$line" | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1 || true)
        [ -z "$version" ] && continue
        if [ "$version" != "$ORT_REFERENCE" ]; then
            echo "  MISMATCH: $file declares $version, build uses $ORT_REFERENCE" >&2
            MISMATCHED=1
        else
            echo "  OK: $file agrees ($version)"
        fi
    done < <(grep -rn 'ORT_VERSION:' "$REPO_ROOT/.github/workflows/" 2>/dev/null || true)

    if [ "$MISMATCHED" -eq 1 ]; then
        echo "" >&2
        echo "  ERROR: ONNX Runtime version drift. Align every declaration with" >&2
        echo "  the build's ARG ONNX_VERSION, or delete the stale declaration." >&2
        EXIT_CODE=1
    fi
fi
echo ""

# ── Check 2: tree_sitter_version in YAML config matches Cargo.lock ────

echo "--- Tree-sitter version (YAML config vs Cargo.lock) ---"

LOCK_FILE="$REPO_ROOT/src/rust/Cargo.lock"
YAML_FILE="$REPO_ROOT/assets/default_configuration.yaml"

if [ ! -f "$LOCK_FILE" ]; then
    echo "  WARNING: Cargo.lock not found at $LOCK_FILE"
else
    # Extract tree-sitter version from Cargo.lock
    # Format: name = "tree-sitter" followed by version = "X.Y.Z"
    TS_LOCK_VERSION=$(awk '/^name = "tree-sitter"$/{found=1; next} found && /^version =/{gsub(/"/, "", $3); print $3; exit}' "$LOCK_FILE")
    TS_LOCK_MAJOR_MINOR=$(echo "$TS_LOCK_VERSION" | cut -d. -f1,2)

    echo "  Cargo.lock tree-sitter version: $TS_LOCK_VERSION (major.minor: $TS_LOCK_MAJOR_MINOR)"

    if [ -f "$YAML_FILE" ]; then
        # Extract tree_sitter_version from YAML config (skip comment lines)
        TS_YAML_VERSION=$(grep -v '^ *#' "$YAML_FILE" | grep 'tree_sitter_version:' | head -1 | sed 's/.*: *"\([^"]*\)".*/\1/')
        echo "  default_configuration.yaml: $TS_YAML_VERSION"

        if [ "$TS_LOCK_MAJOR_MINOR" != "$TS_YAML_VERSION" ]; then
            echo ""
            echo "  ERROR: tree_sitter_version mismatch!"
            echo "  Cargo.lock major.minor: $TS_LOCK_MAJOR_MINOR"
            echo "  YAML config:            $TS_YAML_VERSION"
            echo "  Update assets/default_configuration.yaml to match."
            EXIT_CODE=1
        else
            echo "  OK: YAML config matches Cargo.lock"
        fi
    else
        echo "  WARNING: default_configuration.yaml not found at $YAML_FILE"
    fi
fi
echo ""

# ── Check 3: Cargo.lock sanity check for build.rs ─────────────────────

echo "--- Cargo.lock sanity check ---"

if [ -f "$LOCK_FILE" ]; then
    if grep -q '^name = "tree-sitter"$' "$LOCK_FILE"; then
        echo "  OK: tree-sitter package found in Cargo.lock"
    else
        echo "  ERROR: tree-sitter package NOT found in Cargo.lock"
        echo "  build.rs will emit TREE_SITTER_VERSION=unknown"
        EXIT_CODE=1
    fi
else
    echo "  WARNING: Cargo.lock not found"
fi
echo ""

# ── Summary ────────────────────────────────────────────────────────────

if [ $EXIT_CODE -eq 0 ]; then
    echo "=== All version consistency checks passed ==="
else
    echo "=== Version consistency checks FAILED ==="
fi

exit $EXIT_CODE
