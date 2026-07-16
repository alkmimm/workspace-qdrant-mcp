#!/usr/bin/env python3
"""WAL-safe copy of the daemon's SQLite databases (rehearsal / backup).

Usage:
  migration-rehearsal-copy.py <src-dir> <dest-dir> [--keep N] [--rotate-dir DIR]

Copies memexd.db, search.db, and graph.db from <src-dir> to <dest-dir> using
the SQLite backup API (safe against a live writer mid-WAL — a plain `cp` of
db+wal+shm can produce a torn copy). Missing databases are skipped with a
note; a missing memexd.db means a fresh install, reported as SKIP so the
Makefile gate can treat "nothing to rehearse" as success.

Born from the v48 incident (2026-07-15): a schema migration that passed
clippy and unit gates crash-looped production. Rehearsing the new binary's
migrations against a copy of the live databases catches that class before
`docker compose up` replaces a working daemon.

Disk safety (issue #276) — `make backup-db DB_BACKUP_KEEP=5` once filled the
host disk: 5 x ~7GB snapshots inflated the WSL VHDX until the volume hit 0
bytes, the copy died with a misleading "unexpected EOF" (ENOSPC in disguise),
and it left a PARTIAL directory that looked like a valid backup. Three
guards, in order:

  1. `--keep N` rotates OLD snapshots BEFORE copying, so peak usage is
     KEEP x size rather than (KEEP+1) x size.
  2. A preflight refuses to start when free space cannot hold the copy plus
     a safety margin — a clear error beats a wedged host.
  3. The copy lands in `<dest>.partial/` and is renamed to `<dest>` only
     after every database is copied, so a crash can never leave a directory
     that reads as a complete backup.
"""
import argparse
import os
import shutil
import sqlite3
import sys

DB_FILES = ["memexd.db", "search.db", "graph.db"]

#: Fraction of the copy size to keep free on top of the copy itself. A backup
#: must never be the thing that fills the volume.
SAFETY_MARGIN = 0.10


def human(n: float) -> str:
    """Bytes as a human-readable string (GiB/MiB)."""
    for unit, size in (("GiB", 1 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)):
        if n >= size:
            return f"{n / size:.1f}{unit}"
    return f"{int(n)}B"


def live_size(src_dir: str) -> int:
    """Total bytes of the databases we are about to copy."""
    total = 0
    for name in DB_FILES:
        path = os.path.join(src_dir, name)
        if os.path.exists(path):
            total += os.path.getsize(path)
    return total


def rotate(rotate_dir: str, keep: int, prefix: str = "pre-deploy-") -> None:
    """Delete oldest snapshots until at most `keep - 1` remain.

    Runs BEFORE the copy so the new snapshot lands inside the budget instead
    of transiently exceeding it (`keep - 1` + the one about to be written =
    `keep`). Also sweeps leftover `.partial` directories from crashed runs.
    `keep <= 0` disables rotation.
    """
    if not os.path.isdir(rotate_dir):
        return
    for entry in os.listdir(rotate_dir):
        if entry.endswith(".partial"):
            stale = os.path.join(rotate_dir, entry)
            print(f"rotate: removing stale partial {entry}")
            shutil.rmtree(stale, ignore_errors=True)
    if keep <= 0:
        return
    snaps = sorted(
        (e for e in os.listdir(rotate_dir) if e.startswith(prefix)),
        key=lambda e: os.path.getmtime(os.path.join(rotate_dir, e)),
    )
    while len(snaps) > max(keep - 1, 0):
        oldest = snaps.pop(0)
        print(f"rotate: removing old snapshot {oldest} (keep={keep})")
        shutil.rmtree(os.path.join(rotate_dir, oldest), ignore_errors=True)


def preflight(src_dir: str, dest_parent: str, keep: int) -> int:
    """Refuse to copy when the volume cannot hold it. Returns an exit code."""
    need = live_size(src_dir)
    free = shutil.disk_usage(dest_parent).free
    required = int(need * (1 + SAFETY_MARGIN))
    print(f"preflight: snapshot ~{human(need)}, free {human(free)}, keep={keep}")
    if free < required:
        print(
            f"ERROR: refusing to back up — need {human(required)} free "
            f"(snapshot {human(need)} + {int(SAFETY_MARGIN * 100)}% margin) "
            f"but only {human(free)} is available.\n"
            f"       Lower DB_BACKUP_KEEP (each snapshot is ~{human(need)}), "
            f"free disk space, or point DB_BACKUP_DIR at a bigger volume.\n"
            f"       A backup that fills the disk is worse than no backup: it "
            f"takes the daemon down with it.",
            file=sys.stderr,
        )
        return 1
    # A full KEEP set is what the caller is really asking to store.
    if keep > 0 and free < need * keep:
        print(
            f"NOTE: {keep} snapshots would need ~{human(need * keep)} but only "
            f"{human(free)} is free — older ones were rotated out first, so "
            f"this run fits. Consider a lower DB_BACKUP_KEEP."
        )
    return 0


def copy_databases(src_dir: str, dest_dir: str) -> None:
    """SQLite-backup every present database into `dest_dir`."""
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


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("src_dir")
    parser.add_argument("dest_dir")
    parser.add_argument(
        "--keep",
        type=int,
        default=0,
        help="rotate snapshots in --rotate-dir before copying (0 = no rotation)",
    )
    parser.add_argument(
        "--rotate-dir",
        default=None,
        help="directory holding pre-deploy-* snapshots (default: dest's parent)",
    )
    args = parser.parse_args()

    if not os.path.exists(os.path.join(args.src_dir, "memexd.db")):
        print("SKIP: no memexd.db in source (fresh install — nothing to rehearse)")
        return 0

    rotate_dir = args.rotate_dir or os.path.dirname(os.path.abspath(args.dest_dir))
    # Rotate FIRST: frees the budget the preflight then measures against.
    rotate(rotate_dir, args.keep)

    dest_parent = os.path.dirname(os.path.abspath(args.dest_dir)) or "."
    os.makedirs(dest_parent, exist_ok=True)
    rc = preflight(args.src_dir, dest_parent, args.keep)
    if rc != 0:
        return rc

    # Write to `<dest>.partial`, promote on success: a crashed copy must never
    # be mistaken for a usable backup (the v48-rollout casualty).
    partial = args.dest_dir.rstrip("/") + ".partial"
    shutil.rmtree(partial, ignore_errors=True)
    try:
        copy_databases(args.src_dir, partial)
    except Exception as exc:  # noqa: BLE001 — report and leave nothing behind
        shutil.rmtree(partial, ignore_errors=True)
        print(f"ERROR: copy failed ({exc}) — partial snapshot discarded", file=sys.stderr)
        return 1

    shutil.rmtree(args.dest_dir, ignore_errors=True)
    os.rename(partial, args.dest_dir)
    print("COPY_OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
