#!/usr/bin/env python3
"""Analyze the memory sampler CSV: per-container trend + leak projection.

Reads the CSV produced by sampler.sh (default $HOME/.wqm-mem-watch/samples.csv,
override with WQM_MEM_WATCH_DIR or a path argument) and reports, for every
sampled container, the current/min/max charged memory plus a linear-regression
slope on ANONYMOUS memory — the part a real leak grows. Page cache (the bulk of
`memory.current`) is reclaimable and deliberately ignored for the verdict.

Usage:
    python3 analyze.py [path/to/samples.csv]
"""
import csv
import os
import sys

DEFAULT = os.path.join(
    os.environ.get("WQM_MEM_WATCH_DIR", os.path.expanduser("~/.wqm-mem-watch")),
    "samples.csv",
)


def linreg(points):
    """Least-squares slope (y-units per second) over (t_seconds, y) points."""
    n = len(points)
    if n < 2:
        return None
    sx = sum(x for x, _ in points)
    sy = sum(y for _, y in points)
    sxx = sum(x * x for x, _ in points)
    sxy = sum(x * y for x, y in points)
    denom = n * sxx - sx * sx
    if denom == 0:
        return None
    return (n * sxy - sx * sy) / denom


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else DEFAULT
    if not os.path.exists(path):
        print(f"no samples yet ({path})")
        return 0

    with open(path, newline="") as fh:
        rows = list(csv.DictReader(fh))
    if not rows:
        print("no samples yet (empty file)")
        return 0

    t0 = int(rows[0]["epoch"])
    span_h = (int(rows[-1]["epoch"]) - t0) / 3600
    print(f"samples: {len(rows)}   span: {span_h:.2f} h   "
          f"({rows[0]['iso']} -> {rows[-1]['iso']})")
    print(f"source : {path}\n")

    # Containers are the `<name>_cur_gib` columns, in file order.
    names = [c[:-len("_cur_gib")] for c in rows[0] if c.endswith("_cur_gib")]

    def series(col):
        return [(int(r["epoch"]) - t0, float(r[col]))
                for r in rows if r.get(col) not in (None, "")]

    for name in names:
        cur = series(f"{name}_cur_gib")
        anon = series(f"{name}_anon_gib")
        if not cur:
            continue
        cur_vals = [y for _, y in cur]
        print(f"=== {name} ===")
        print(f"  current : {cur[-1][1]:.3f} GiB charged"
              + (f"   anon: {anon[-1][1]:.3f} GiB" if anon else "   anon: n/a"))
        print(f"  charged : min {min(cur_vals):.3f} / max {max(cur_vals):.3f} GiB"
              f"  (Δ {max(cur_vals) - min(cur_vals):.3f})")

        leak_series = anon if anon else cur  # prefer anon; fall back to charged
        slope = linreg(leak_series)
        if slope is None or span_h < 0.25:
            print(f"  trend   : need ~15+ min of samples (have {span_h * 60:.0f} min)\n")
            continue
        per_day = slope * 86400
        basis = "anon" if anon else "charged (anon n/a)"
        verdict = ("LEAKING" if per_day > 0.5 else
                   "rising" if per_day > 0.1 else
                   "stable" if abs(per_day) <= 0.1 else "shrinking")
        print(f"  {basis} slope: {per_day:+.3f} GiB/day  ->  {verdict}")
        if per_day > 0.1:
            now = leak_series[-1][1]
            hours = (30 - now) / (per_day / 24)
            if hours > 0:
                print(f"  projection: ~{hours:.0f} h to reach 30 GiB at this rate")
        print()

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
