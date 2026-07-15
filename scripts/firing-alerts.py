#!/usr/bin/env python3
"""Print firing Prometheus alerts, one per line (for `make stack-status`).

Usage: firing-alerts.py <prometheus-base-url>

The stack ships alert RULES (docker/prometheus/alerts.yml) but no
Alertmanager, so firing alerts were only visible in the Prometheus UI nobody
watches — the 2026-07-15 crash-loop fired DaemonDown silently for ~35 min.
`make stack-status` (which `redeploy` also runs) now surfaces them where the
operator actually looks. Exit 0 always: unreachable Prometheus degrades to a
note, never fails the caller.
"""
import json
import sys
import urllib.request


def main() -> int:
    base = sys.argv[1] if len(sys.argv) > 1 else "http://localhost:9090"
    try:
        with urllib.request.urlopen(f"{base}/api/v1/alerts", timeout=3) as resp:
            data = json.load(resp)
    except Exception as exc:  # noqa: BLE001 — any failure is a soft note
        print(f"  (prometheus unreachable: {exc})")
        return 0

    firing = [a for a in data.get("data", {}).get("alerts", []) if a.get("state") == "firing"]
    if not firing:
        print("  none")
        return 0
    for alert in firing:
        name = alert.get("labels", {}).get("alertname", "?")
        severity = alert.get("labels", {}).get("severity", "?")
        summary = alert.get("annotations", {}).get("summary", "")
        print(f"  [FIRING/{severity}] {name}: {summary}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
