#!/usr/bin/env python3
"""Report local read_log usage since the two manual checks on 2026-11-26."""

import os
import subprocess
import urllib.request


# Container generation at the 22:55:25 UTC scrape. Counters reset on restart.
BASELINE_STARTED_AT = "2026-11-26T22:33:03.369991299Z"
# Successful calls, raw bytes and returned bytes by requested mode.
BASELINE = {
    "compact": (0, 0, 0),
    "template": (0, 0, 0),
    "interleaved": (0, 0, 0),
    "auto": (1, 885, 282),
    "raw": (1, 361, 361),
}


def report(metrics):
    samples = {}
    for line in metrics.splitlines():
        if not line.startswith(("wqm_mcp_read_log_reads_total{", "wqm_mcp_read_log_bytes_total{")):
            continue
        series, value = line.split()
        name, label_text = series[:-1].split("{")
        # These two families have only enumerated mode and kind labels.
        labels = dict(label.split("=", 1) for label in label_text.split(","))
        mode = labels["mode"].strip('"')
        kind = "calls" if name.endswith("_reads_total") else labels["kind"].strip('"')
        samples[mode, kind] = int(value)

    rows = []
    for mode, (base_calls, base_raw, base_returned) in BASELINE.items():
        calls = samples[mode, "calls"] - base_calls
        raw = samples[mode, "raw"] - base_raw
        returned = samples[mode, "returned"] - base_returned
        if min(calls, raw, returned) < 0:
            raise ValueError(f"{mode} counter below baseline. Check the scrape history or MCP logs.")
        saved = raw - returned
        percent = f"{100 * saved / raw:.1f}%" if raw else "n/a"
        rows.append(f"{mode:<12} {calls:>8} {raw:>12} {returned:>14} {saved:>12} {percent:>9}")

    print(f"{'mode':<12} {'calls':>8} {'raw_bytes':>12} {'returned_bytes':>14} {'saved_bytes':>12} {'savings':>9}")
    print("\n".join(rows))


def main():
    request = urllib.request.Request("http://127.0.0.1:9092/metrics")
    token = os.getenv("MCP_METRICS_TOKEN") or os.getenv("MCP_HTTP_TOKEN")
    if token:
        request.add_header("Authorization", f"Bearer {token}")
    with urllib.request.urlopen(request, timeout=10) as response:
        metrics = response.read().decode()

    # Verify after the scrape so a concurrent restart cannot silently mix generations.
    started_at = subprocess.check_output(
        ["docker", "inspect", "--format", "{{.State.StartedAt}}", "wqm-mcp"], text=True
    ).strip()
    if started_at != BASELINE_STARTED_AT:
        raise SystemExit("The MCP instance has changed. Use the scrape history or MCP logs since the baseline.")
    report(metrics)


if __name__ == "__main__":
    try:
        main()
    except KeyError as error:
        raise SystemExit(f"Missing read_log counter in /metrics: {error}") from None
    except (OSError, subprocess.CalledProcessError, ValueError) as error:
        raise SystemExit(f"Cannot report read_log usage: {error}") from None
