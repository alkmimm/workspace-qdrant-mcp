#!/usr/bin/env python3
"""Aggregate agent-economy harness runs into per-condition tables.

Reads the runs.jsonl produced by run_harness.py and prints:
  1. a per-condition summary (success rate, cost, tokens, turns, duration,
     MCP calls, server bytes) — the TCC's primary comparison table;
  2. a task × condition success/cost matrix to spot which task categories a
     condition helps or hurts.

Usage:
  python3 analyze.py results/runs.jsonl [--csv out.csv]
"""

import argparse
import csv
import json
import sys
from collections import defaultdict
from pathlib import Path
from statistics import mean, median


def fmt(value, pattern="{:.4f}"):
    return pattern.format(value) if value is not None else "—"


def load(path: Path) -> list:
    runs = []
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line:
            continue
        runs.append(json.loads(line))
    return runs


def summarize(runs: list) -> dict:
    by_condition = defaultdict(list)
    for r in runs:
        if "error" in r:
            continue
        by_condition[r["condition"]].append(r)

    summary = {}
    for cond, rows in sorted(by_condition.items()):
        def vals(key):
            out = [r[key] for r in rows if r.get(key) is not None]
            return out

        server_calls = [r.get("server", {}).get("total_calls", 0) for r in rows]
        server_out = [r.get("server", {}).get("bytes_out", 0) for r in rows]
        server_in = [r.get("server", {}).get("bytes_in", 0) for r in rows]
        summary[cond] = {
            "runs": len(rows),
            "success_rate": mean(1 if r.get("success") else 0 for r in rows),
            "mean_cost_usd": mean(vals("cost_usd")) if vals("cost_usd") else None,
            "median_cost_usd": median(vals("cost_usd")) if vals("cost_usd") else None,
            "mean_input_tokens": mean(vals("input_tokens")) if vals("input_tokens") else None,
            "mean_output_tokens": mean(vals("output_tokens")) if vals("output_tokens") else None,
            "mean_cache_read_tokens": mean(vals("cache_read_tokens")) if vals("cache_read_tokens") else None,
            "mean_turns": mean(vals("num_turns")) if vals("num_turns") else None,
            "mean_duration_ms": mean(vals("duration_ms")) if vals("duration_ms") else None,
            "mean_mcp_calls": mean(server_calls) if server_calls else 0,
            "mean_server_bytes_out": mean(server_out) if server_out else 0,
            "mean_server_bytes_in": mean(server_in) if server_in else 0,
        }
        # Cost per completed task — the headline metric: total spend divided
        # by the number of SUCCESSFUL runs (a cheap condition that fails is
        # not cheap).
        successes = sum(1 for r in rows if r.get("success"))
        total_cost = sum(v for v in vals("cost_usd"))
        summary[cond]["cost_per_completed_usd"] = total_cost / successes if successes else None
    return summary


def print_summary(summary: dict) -> None:
    metrics = [
        ("runs", "{:d}"),
        ("success_rate", "{:.1%}"),
        ("cost_per_completed_usd", "{:.4f}"),
        ("mean_cost_usd", "{:.4f}"),
        ("mean_input_tokens", "{:,.0f}"),
        ("mean_cache_read_tokens", "{:,.0f}"),
        ("mean_output_tokens", "{:,.0f}"),
        ("mean_turns", "{:.1f}"),
        ("mean_duration_ms", "{:,.0f}"),
        ("mean_mcp_calls", "{:.1f}"),
        ("mean_server_bytes_out", "{:,.0f}"),
        ("mean_server_bytes_in", "{:,.0f}"),
    ]
    conditions = list(summary.keys())
    header = ["metric"] + conditions
    print("\n== Per-condition summary ==")
    print(" | ".join(header))
    print(" | ".join("---" for _ in header))
    for key, pattern in metrics:
        row = [key] + [fmt(summary[c].get(key), pattern) for c in conditions]
        print(" | ".join(row))


def print_matrix(runs: list) -> None:
    cell = {}
    tasks, conditions = [], []
    for r in runs:
        if "error" in r:
            continue
        t, c = r["task"], r["condition"]
        if t not in tasks:
            tasks.append(t)
        if c not in conditions:
            conditions.append(c)
        prev = cell.get((t, c), {"n": 0, "ok": 0, "cost": 0.0})
        prev["n"] += 1
        prev["ok"] += 1 if r.get("success") else 0
        prev["cost"] += r.get("cost_usd") or 0.0
        cell[(t, c)] = prev
    print("\n== Task × condition (success n/N, mean cost) ==")
    print(" | ".join(["task"] + conditions))
    print(" | ".join("---" for _ in range(len(conditions) + 1)))
    for t in tasks:
        row = [t]
        for c in conditions:
            v = cell.get((t, c))
            row.append(f"{v['ok']}/{v['n']} ${v['cost'] / v['n']:.3f}" if v else "—")
        print(" | ".join(row))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("runs", help="path to runs.jsonl")
    parser.add_argument("--csv", default=None, help="also write the per-run flat table as CSV")
    args = parser.parse_args()

    runs = load(Path(args.runs))
    if not runs:
        sys.exit("no runs found")
    errored = [r for r in runs if "error" in r]
    if errored:
        print(f"note: {len(errored)} run(s) errored and are excluded", file=sys.stderr)

    print_summary(summarize(runs))
    print_matrix(runs)

    if args.csv:
        keys = [
            "task", "category", "condition", "rep", "success", "cost_usd",
            "input_tokens", "output_tokens", "cache_read_tokens",
            "cache_creation_tokens", "num_turns", "duration_ms", "wall_ms",
        ]
        with open(args.csv, "w", newline="") as f:
            writer = csv.writer(f)
            writer.writerow(keys + ["mcp_calls", "server_bytes_in", "server_bytes_out"])
            for r in runs:
                if "error" in r:
                    continue
                server = r.get("server", {})
                writer.writerow(
                    [r.get(k) for k in keys]
                    + [server.get("total_calls", 0), server.get("bytes_in", 0), server.get("bytes_out", 0)]
                )
        print(f"\nCSV written to {args.csv}")


if __name__ == "__main__":
    main()
