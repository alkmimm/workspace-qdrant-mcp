#!/usr/bin/env python3
"""Agent token-economy experiment harness.

Runs each task from tasks.json under each condition by driving a headless
`claude -p` session, then joins two measurement planes per run:

  - client side (ground truth): the CLI result JSON — real billed tokens
    (input / output / cache read / cache creation), cost USD, turns,
    duration;
  - server side: the daemon's `search_events` rows in the run's time
    window — MCP calls by op, bytes_in/bytes_out (spec 20), shape modes,
    followups/escalations (parent-linked, schema v44+).

Runs are strictly SEQUENTIAL so the time window attributes server events
unambiguously (single-user deployment). Session ids recorded on both
planes allow a stricter join when needed (schema v44+ populates
search_events.session_id with the MCP HTTP session).

Conditions (what varies is ONLY the tool surface / instructions):
  native      no MCP server; native Read/Grep/Glob only
  mcp         workspace-qdrant MCP + native read tools
  mcp-packed  same as mcp, plus a system-prompt instruction to request
              responseFormat:"packed" on search calls

Usage:
  python3 run_harness.py                        # all tasks × all conditions
  python3 run_harness.py --conditions mcp       # one condition
  python3 run_harness.py --only loc-shaping     # one task
  python3 run_harness.py --repeat 3             # N repetitions per cell
  python3 run_harness.py --dry-run              # print commands, run nothing

Requires: `claude` CLI on PATH (authenticated), MCP_HTTP_TOKEN in the
environment (for the mcp* conditions), docker access to the wqm-memexd
container (server-side metrics; skipped with a warning if unavailable).
"""

import argparse
import json
import os
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parent.parent
DEFAULT_TASKS = HERE / "tasks.json"
DEFAULT_OUT = HERE / "results" / "runs.jsonl"

NATIVE_TOOLS = "Read,Grep,Glob"
MCP_TOOLS = (
    "Read,Grep,Glob,"
    "mcp__workspace-qdrant__search,mcp__workspace-qdrant__grep,"
    "mcp__workspace-qdrant__retrieve,mcp__workspace-qdrant__list,"
    "mcp__workspace-qdrant__graph"
)
PACKED_INSTRUCTION = (
    "When you call the workspace-qdrant `search` tool, always pass "
    "responseFormat:\"packed\" and read the returned packed_bundle.text "
    "instead of individual result bodies."
)
MCP_URL = os.environ.get("WQM_MCP_URL", "http://localhost:6335/mcp")
MEMEXD_CONTAINER = os.environ.get("WQM_MEMEXD_CONTAINER", "wqm-memexd")
MEMEXD_DB = "/var/lib/memexd/memexd.db"

CONDITIONS = ("native", "mcp", "mcp-packed")


def iso_z(dt: datetime) -> str:
    """Match the daemon's stored ISO-Z format (%Y-%m-%dT%H:%M:%fZ) so lexical
    SQL comparison on the TEXT ts column is correct (never datetime('now'))."""
    return dt.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.") + f"{dt.microsecond // 1000:03d}Z"


def write_mcp_config(enabled: bool) -> str:
    """Materialize a --strict-mcp-config file: either exactly our HTTP server
    or an empty registry (native condition)."""
    if enabled:
        token = os.environ.get("MCP_HTTP_TOKEN")
        if not token:
            sys.exit("MCP_HTTP_TOKEN is not set — required for the mcp* conditions.")
        cfg = {
            "mcpServers": {
                "workspace-qdrant": {
                    "type": "http",
                    "url": MCP_URL,
                    "headers": {"Authorization": f"Bearer {token}"},
                }
            }
        }
    else:
        cfg = {"mcpServers": {}}
    fd, path = tempfile.mkstemp(prefix="harness-mcp-", suffix=".json")
    with os.fdopen(fd, "w") as f:
        json.dump(cfg, f)
    return path


def build_command(prompt: str, condition: str, mcp_config_path: str, args) -> list:
    cmd = [
        "claude",
        "-p",
        prompt,
        "--output-format",
        "json",
        "--max-turns",
        str(args.max_turns),
        "--strict-mcp-config",
        "--mcp-config",
        mcp_config_path,
        "--allowedTools",
        NATIVE_TOOLS if condition == "native" else MCP_TOOLS,
    ]
    if condition == "mcp-packed":
        cmd += ["--append-system-prompt", PACKED_INSTRUCTION]
    if args.model:
        cmd += ["--model", args.model]
    return cmd


def server_metrics(t_start: str, t_end: str) -> dict:
    """Aggregate search_events in [t_start, t_end] via python3 inside the
    memexd container (no sqlite3 binary there; DB lives in the volume)."""
    query = f"""
import json, sqlite3
conn = sqlite3.connect("file:{MEMEXD_DB}?mode=ro", uri=True)
c = conn.cursor()
rows = c.execute(
    "SELECT op, COUNT(*), COALESCE(SUM(bytes_in),0), COALESCE(SUM(bytes_out),0), "
    "COALESCE(SUM(hits_truncated),0) FROM search_events "
    "WHERE tool='mcp_qdrant' AND ts >= ? AND ts <= ? GROUP BY op",
    ({t_start!r}, {t_end!r}),
).fetchall()
sessions = [r[0] for r in c.execute(
    "SELECT DISTINCT session_id FROM search_events "
    "WHERE tool='mcp_qdrant' AND ts >= ? AND ts <= ? AND session_id IS NOT NULL",
    ({t_start!r}, {t_end!r}),
)]
print(json.dumps({{
    "by_op": {{r[0]: {{"calls": r[1], "bytes_in": r[2], "bytes_out": r[3], "hits_truncated": r[4]}} for r in rows}},
    "total_calls": sum(r[1] for r in rows),
    "bytes_in": sum(r[2] for r in rows),
    "bytes_out": sum(r[3] for r in rows),
    "sessions": sessions,
}}))
"""
    try:
        out = subprocess.run(
            ["docker", "exec", MEMEXD_CONTAINER, "python3", "-c", query],
            capture_output=True,
            text=True,
            timeout=30,
        )
        if out.returncode != 0:
            return {"error": out.stderr.strip()[:200]}
        return json.loads(out.stdout)
    except Exception as exc:  # noqa: BLE001 — metrics are best-effort
        return {"error": str(exc)[:200]}


def check_success(result_text: str, expected: list) -> bool:
    return any(marker in result_text for marker in expected)


def run_one(task: dict, condition: str, mcp_config_path: str, args) -> dict:
    cmd = build_command(task["prompt"], condition, mcp_config_path, args)
    t0 = datetime.now(timezone.utc)
    proc = subprocess.run(
        cmd,
        capture_output=True,
        text=True,
        cwd=str(REPO_ROOT),
        timeout=args.timeout,
    )
    t1 = datetime.now(timezone.utc)
    record = {
        "task": task["id"],
        "category": task.get("category"),
        "condition": condition,
        "t_start": iso_z(t0),
        "t_end": iso_z(t1),
        "wall_ms": int((t1 - t0).total_seconds() * 1000),
        "exit_code": proc.returncode,
    }
    try:
        result = json.loads(proc.stdout)
    except json.JSONDecodeError:
        record["error"] = f"unparseable CLI output: {proc.stdout[:200]!r} / {proc.stderr[:200]!r}"
        return record

    text = result.get("result") or ""
    usage = result.get("usage") or {}
    record.update(
        {
            "success": check_success(text, task["expected_paths"]),
            "is_error": bool(result.get("is_error")),
            "num_turns": result.get("num_turns"),
            "duration_ms": result.get("duration_ms"),
            "duration_api_ms": result.get("duration_api_ms"),
            "cost_usd": result.get("total_cost_usd"),
            "input_tokens": usage.get("input_tokens"),
            "output_tokens": usage.get("output_tokens"),
            "cache_read_tokens": usage.get("cache_read_input_tokens"),
            "cache_creation_tokens": usage.get("cache_creation_input_tokens"),
            "client_session_id": result.get("session_id"),
            "answer_excerpt": text[:400],
        }
    )
    # ±1s slack: the daemon stamps ts with its own clock at event INSERT
    # (same machine, but be robust to sub-second skew). Runs are sequential
    # with a 3s gap, so windows never overlap.
    from datetime import timedelta

    record["server"] = server_metrics(
        iso_z(t0 - timedelta(seconds=1)), iso_z(t1 + timedelta(seconds=1))
    )
    return record


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--tasks", default=str(DEFAULT_TASKS))
    parser.add_argument("--out", default=str(DEFAULT_OUT))
    parser.add_argument("--conditions", default=",".join(CONDITIONS))
    parser.add_argument("--only", default=None, help="run a single task id")
    parser.add_argument("--repeat", type=int, default=1)
    parser.add_argument("--max-turns", type=int, default=20)
    parser.add_argument("--timeout", type=int, default=600, help="per-run subprocess timeout (s)")
    parser.add_argument("--model", default=None, help="claude --model override")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()

    tasks = json.loads(Path(args.tasks).read_text())["tasks"]
    if args.only:
        tasks = [t for t in tasks if t["id"] == args.only]
        if not tasks:
            sys.exit(f"no task with id {args.only!r}")
    conditions = [c.strip() for c in args.conditions.split(",") if c.strip()]
    for c in conditions:
        if c not in CONDITIONS:
            sys.exit(f"unknown condition {c!r} (valid: {', '.join(CONDITIONS)})")

    out_path = Path(args.out)
    out_path.parent.mkdir(parents=True, exist_ok=True)

    configs = {c: write_mcp_config(enabled=c != "native") for c in conditions}
    try:
        total = len(tasks) * len(conditions) * args.repeat
        n = 0
        for rep in range(args.repeat):
            for task in tasks:
                for condition in conditions:
                    n += 1
                    label = f"[{n}/{total}] {task['id']} × {condition} (rep {rep + 1})"
                    if args.dry_run:
                        cmd = build_command(task["prompt"], condition, configs[condition], args)
                        print(f"{label}\n  {' '.join(cmd[:8])} ...")
                        continue
                    print(f"{label} ...", flush=True)
                    record = run_one(task, condition, configs[condition], args)
                    record["rep"] = rep + 1
                    with out_path.open("a") as f:
                        f.write(json.dumps(record) + "\n")
                    status = "OK " if record.get("success") else "MISS" if "error" not in record else "ERR"
                    print(
                        f"    {status} cost=${record.get('cost_usd') or 0:.4f} "
                        f"turns={record.get('num_turns')} "
                        f"mcp_calls={record.get('server', {}).get('total_calls', 0)} "
                        f"wall={record['wall_ms']}ms"
                    )
                    time.sleep(3)  # keep attribution windows disjoint (±1s slack each side)
    finally:
        for path in configs.values():
            try:
                os.unlink(path)
            except OSError:
                pass
    if not args.dry_run:
        print(f"\nresults appended to {out_path}\nanalyze:  python3 {HERE / 'analyze.py'} {out_path}")


if __name__ == "__main__":
    main()
