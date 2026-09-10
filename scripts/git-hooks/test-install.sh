#!/bin/sh
# Behavioural tests for the hooks installer.
#
# These assert what the generated hook DOES, not what it says. The bug they
# guard shipped for months behind a green tree precisely because nothing
# executed the thing: the sync script was committed non-executable, the hook
# exec'd it directly, and when the executable bit did not survive the trip to a
# developer's machine the failure was invisible — exec failed, `>/dev/null 2>&1`
# ate the message, `|| true` ate the status, `exit 0` reported success, and
# branch sync silently never happened.
#
# Hermetic: a throwaway git repo under a temp dir, no network, no daemon.
set -eu

SELF_DIR="$(cd "$(dirname "$0")" && pwd)"
INSTALL="$SELF_DIR/install.sh"
FAILURES=0

fail() {
    printf 'FAIL: %s\n' "$1" >&2
    FAILURES=$((FAILURES + 1))
}

ok() {
    printf 'ok: %s\n' "$1"
}

# A scratch repo plus a stand-in sync script that records that it ran.
setup() {
    WORK="$(mktemp -d)"
    REPO="$WORK/repo"
    mkdir -p "$REPO"
    git init -q "$REPO"
    git -C "$REPO" config user.email test@example.com
    git -C "$REPO" config user.name test
    MARKER="$WORK/ran.txt"
    LOG="$WORK/logs/hooks.jsonl"
    SYNC="$WORK/fake-sync.sh"
}

teardown() {
    [ -n "${WORK:-}" ] && rm -rf "$WORK"
}

write_sync_script() {
    # $1 = "ok" (records the marker) or "boom" (writes to stderr and fails)
    if [ "$1" = "boom" ]; then
        cat > "$SYNC" <<'INNER'
#!/bin/sh
echo "wqm-sync exploded" >&2
exit 3
INNER
    else
        cat > "$SYNC" <<INNER
#!/bin/sh
printf 'ran %s\n' "\$1" >> "$MARKER"
INNER
    fi
    # THE POINT: not executable. This is the state the bug depended on.
    chmod 644 "$SYNC"
}

install_hooks() {
    sh "$INSTALL" --repo "$REPO" --wqm-script "$SYNC" --log "$LOG" \
        --mcp-url http://localhost:6335/mcp --token unused >/dev/null 2>&1
}

# ── A non-executable sync script must still run ──────────────────────────────
setup
write_sync_script ok
install_hooks
HOOK="$REPO/.git/hooks/post-commit"

if [ ! -f "$HOOK" ]; then
    fail "installer produced no post-commit hook"
else
    # Strip the bit AFTER install. The installer chmods +x on its way through,
    # so without this the scenario is never exercised and the check passes on
    # the very code it is meant to catch — verified: it did.
    #
    # This models the real failure: a host where chmod is refused, or a checkout
    # onto a filesystem that cannot carry the bit at all.
    chmod 644 "$SYNC"
    sh "$HOOK" >/dev/null 2>&1 || true
    if [ -s "$MARKER" ]; then
        ok "hook invokes a NON-EXECUTABLE sync script"
    else
        fail "hook did not run the sync script when it lacked the executable bit"
    fi
fi
teardown

# ── A failing sync script must leave a trace ─────────────────────────────────
setup
write_sync_script boom
install_hooks
HOOK="$REPO/.git/hooks/post-commit"

if [ ! -f "$HOOK" ]; then
    fail "installer produced no post-commit hook (failure case)"
else
    # The hook must still exit 0 — a broken sync may never block a git command.
    if sh "$HOOK" >/dev/null 2>&1; then
        ok "hook exits 0 even when the sync script fails"
    else
        fail "hook propagated failure; a hook must never block a git operation"
    fi
    if [ -f "$LOG" ] && grep -q "wqm-sync exploded" "$LOG"; then
        ok "the failure reached the log instead of /dev/null"
    else
        fail "sync failure left NO trace — this is the silent no-op being fixed"
    fi
fi
teardown

# ── The scripts ship executable, so chmod is a convenience not a mechanism ───
#
# Checked on the FILESYSTEM, not via `git ls-files`: this also runs inside the
# container build, where scripts/ is a plain COPY and there is no git index to
# consult. A mode check that silently finds nothing to look at is worse than no
# check, so it asserts the bit that actually ships.
for _script in wqm-sync-branch.sh install.sh; do
    if [ -x "$SELF_DIR/$_script" ]; then
        ok "$_script ships executable"
    else
        fail "$_script is not executable; the hooks still work (they go through \`sh\`) but every developer who installs them gets a permanently dirty working tree"
    fi
done

# When a git index IS available, pin the committed mode too — the filesystem bit
# is downstream of it, and 644 in the index is what produced the drift.
if git -C "$SELF_DIR" rev-parse --git-dir >/dev/null 2>&1; then
    for _script in wqm-sync-branch.sh install.sh; do
        _mode="$(git -C "$SELF_DIR" ls-files -s -- "$_script" | awk '{print $1}')"
        if [ "$_mode" = "100755" ]; then
            ok "$_script is committed 100755"
        else
            fail "$_script is committed $_mode, so the working tree drifts on every install"
        fi
    done
fi

if [ "$FAILURES" -ne 0 ]; then
    printf '\n%s check(s) failed\n' "$FAILURES" >&2
    exit 1
fi
printf '\nall hook-installer checks passed\n'
