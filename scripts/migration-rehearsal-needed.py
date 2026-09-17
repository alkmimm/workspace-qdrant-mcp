#!/usr/bin/env python3
"""Decide whether a migration rehearsal is worth a 10 GiB copy.

Usage:
  migration-rehearsal-needed.py <live-dir> <probe-log>

`probe-log` is the output of the NEW binary run as
`WQM_DATABASE_PATH=<empty>/memexd.db memexd --foreground --migrate-only` on an
EMPTY directory: it creates fresh databases and logs one line per store with
the version it targets ("Current schema version: 0, target: 49", "Search DB
schema version: 0, target: 10", "Graph schema version: 0, target: 6"). That
probe costs ~4 s and a few hundred KiB. Comparing those targets with the LIVE
databases' recorded versions tells whether the new binary would migrate
anything at all.

Why: the rehearsal (v48 incident) exists to catch a migration that crash-loops
production — but it copied the three live databases (~10 GiB) on EVERY
redeploy, migrations pending or not, and on 2026-09-16 that copy (plus the
snapshot's) filled the WSL2 VM's page cache to its ceiling and took the
Windows host down. Most redeploys ship no schema change; those must cost
nothing here.

Prints exactly one line and exits:
  SKIP: ...    (0)  every live version equals the new binary's target
  NEEDED: ...  (10) at least one store would migrate, or the answer is
                    unknowable (unparseable probe, unreadable live DB) —
                    unknown means REHEARSE, never skip.
"""
import os
import re
import sqlite3
import sys

STORES = [
    # (file, version table, probe-log label)
    ("memexd.db", "schema_version", "Current schema version"),
    ("search.db", "search_schema_version", "Search DB schema version"),
    ("graph.db", "graph_schema_version", "Graph schema version"),
]
NEEDED = 10


def live_version(path: str, table: str):
    """MAX(version) recorded in `table`, or None when unreadable/absent."""
    if not os.path.exists(path):
        return None
    try:
        conn = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
        try:
            row = conn.execute(f"SELECT MAX(version) FROM {table}").fetchone()
        finally:
            conn.close()
    except sqlite3.Error:
        return None
    return row[0] if row and row[0] is not None else None


def target_version(log: str, label: str):
    m = re.search(re.escape(label) + r": (\d+), target: (\d+)", log)
    return int(m.group(2)) if m else None


def main() -> int:
    if len(sys.argv) != 3:
        print("NEEDED: usage: migration-rehearsal-needed.py <live-dir> <probe-log>")
        return NEEDED
    live_dir, probe_log = sys.argv[1], sys.argv[2]
    try:
        log = open(probe_log, encoding="utf-8", errors="replace").read()
    except OSError as exc:
        print(f"NEEDED: probe log unreadable ({exc}) — rehearsing to be safe")
        return NEEDED

    parts, pending, unknown = [], [], []
    for name, table, label in STORES:
        target = target_version(log, label)
        live = live_version(os.path.join(live_dir, name), table)
        parts.append(f"{name.split('.')[0]} {live if live is not None else '?'}/{target if target is not None else '?'}")
        if target is None or live is None:
            unknown.append(name)
        elif live != target:
            pending.append(name)

    summary = ", ".join(parts)
    if unknown:
        print(f"NEEDED: could not compare {', '.join(unknown)} ({summary}) — rehearsing to be safe")
        return NEEDED
    if pending:
        print(f"NEEDED: {', '.join(pending)} would migrate ({summary})")
        return NEEDED
    print(f"SKIP: every store is already at the new binary's target ({summary}) — nothing to rehearse")
    return 0


if __name__ == "__main__":
    sys.exit(main())
