#!/usr/bin/env python3
"""WAL-safe copy of the daemon's SQLite databases (rehearsal / backup).

Usage: migration-rehearsal-copy.py <src-dir> <dest-dir>

Copies memexd.db, search.db, and graph.db from <src-dir> to <dest-dir> using
the SQLite backup API (safe against a live writer mid-WAL — a plain `cp`
of db+wal+shm can produce a torn copy). Missing databases are skipped with a
note; a missing memexd.db means a fresh install, reported as SKIP so the
Makefile gate can treat "nothing to rehearse" as success.

Born from the v48 incident (2026-07-15): a schema migration that passed
clippy and unit gates crash-looped production. Rehearsing the new binary's
migrations against a copy of the live databases catches that class before
`docker compose up` replaces a working daemon.
"""
import os
import sqlite3
import sys

DB_FILES = ["memexd.db", "search.db", "graph.db"]


def main() -> int:
    if len(sys.argv) != 3:
        print(f"usage: {sys.argv[0]} <src-dir> <dest-dir>", file=sys.stderr)
        return 2
    src_dir, dest_dir = sys.argv[1], sys.argv[2]

    if not os.path.exists(os.path.join(src_dir, "memexd.db")):
        print("SKIP: no memexd.db in source (fresh install — nothing to rehearse)")
        return 0

    os.makedirs(dest_dir, exist_ok=True)
    for name in DB_FILES:
        src_path = os.path.join(src_dir, name)
        if not os.path.exists(src_path):
            print(f"note: {name} absent in source; skipped")
            continue
        src = sqlite3.connect(f"file:{src_path}?mode=ro", uri=True)
        dst = sqlite3.connect(os.path.join(dest_dir, name))
        try:
            src.backup(dst)
        finally:
            dst.close()
            src.close()
        size = os.path.getsize(os.path.join(dest_dir, name))
        print(f"copied {name} -> {dest_dir} ({size} bytes)")

    print("COPY_OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
