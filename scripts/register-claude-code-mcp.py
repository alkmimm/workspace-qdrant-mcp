#!/usr/bin/env python3
"""Register the workspace-qdrant MCP server in the *Claude Code* client config.

Why this exists (and why it is separate from ``gen-mcp-config.ps1``):

Claude Code — including the background "chips" / spawned sessions the desktop app
opens — reads its MCP servers from the user config ``~/.claude.json``
(``%USERPROFILE%\\.claude.json`` on Windows). It does **not** read
``claude_desktop_config.json``; that file is consumed only by the Claude Desktop
app's own MCP client. A server registered *solely* in the desktop config is
therefore invisible to Claude Code and to every chip it spawns — which is exactly
the failure this helper prevents (a chip that "cannot use the workspace-qdrant
MCP" because the server was never in the config the chip reads).

Cross-platform: ``Path.home()`` resolves to the right per-user config on both
Windows and Linux/WSL, so the same script backs the Windows ``apply-config-claude-code``
and the Linux ``claude-register`` Makefile targets.

Safe by construction: the whole config is round-tripped through Python's ``json``
(no depth-limited re-serialisation that could truncate the large ~/.claude.json),
the previous file is copied to a timestamped ``.bak-register-*`` backup, and the
new file is written atomically (temp + replace). The bearer token is never
printed. By default the entry references the token via ``${MCP_HTTP_TOKEN}`` so no
secret is written to disk (Claude Code expands it at load time); pass
``--token-mode literal`` to embed the resolved value instead.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path

SERVER_NAME_RE = re.compile(r"^[A-Za-z0-9_-]+$")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Register workspace-qdrant in the Claude Code (~/.claude.json) config.",
    )
    parser.add_argument(
        "--config",
        type=Path,
        default=Path.home() / ".claude.json",
        help="Claude Code user config (default: ~/.claude.json / %%USERPROFILE%%\\.claude.json)",
    )
    parser.add_argument("--server-name", default="workspace-qdrant")
    parser.add_argument("--url", default="http://localhost:6335/mcp")
    parser.add_argument(
        "--transport",
        choices=["http", "mcp-remote"],
        default="http",
        help=(
            "http = Claude Code native Streamable-HTTP (no proxy process, less "
            "reconnect churn, recommended); mcp-remote = stdio bridge via the "
            "mcp-remote proxy (parity with the Claude Desktop app entry)."
        ),
    )
    parser.add_argument("--bearer-token-env-var", default="MCP_HTTP_TOKEN")
    parser.add_argument(
        "--token-mode",
        choices=["ref", "literal"],
        default="ref",
        help=(
            "ref = write '${VAR}' and let Claude Code expand it at load (no secret "
            "on disk; needs VAR present in the session environment). literal = "
            "resolve VAR now and embed the value in the config file."
        ),
    )
    parser.add_argument(
        "--mcp-remote-proxy",
        default="",
        help="Path to mcp-remote/dist/proxy.js (mcp-remote transport). "
        "Default: auto-detect via `npm root -g`.",
    )
    parser.add_argument("--dry-run", action="store_true")
    return parser.parse_args()


def resolve_token(mode: str, var: str) -> str:
    if mode == "ref":
        return "${" + var + "}"
    value = os.environ.get(var, "").strip()
    if not value:
        raise SystemExit(
            f"--token-mode literal but ${var} is not set in this environment. "
            f"Export it, or use --token-mode ref."
        )
    return value


def resolve_proxy(explicit: str) -> str:
    if explicit:
        path = Path(explicit)
        if not path.is_file():
            raise SystemExit(f"--mcp-remote-proxy not found: {path}")
        return str(path).replace("\\", "/")
    root = ""
    try:
        root = subprocess.run(
            ["npm", "root", "-g"], capture_output=True, text=True, check=True
        ).stdout.strip()
    except Exception:  # npm missing / not on PATH
        root = ""
    if root:
        candidate = Path(root) / "mcp-remote" / "dist" / "proxy.js"
        if candidate.is_file():
            return str(candidate).replace("\\", "/")
    raise SystemExit(
        "mcp-remote proxy not found. Install it with `npm install -g mcp-remote`, "
        "or pass --mcp-remote-proxy <abs>/dist/proxy.js. "
        "(Or use --transport http, which needs no proxy.)"
    )


def build_entry(args: argparse.Namespace, header_token: str) -> dict:
    if args.transport == "http":
        return {
            "type": "http",
            "url": args.url,
            "headers": {"Authorization": f"Bearer {header_token}"},
        }
    proxy = resolve_proxy(args.mcp_remote_proxy)
    return {
        "command": "node",
        "args": [proxy, args.url, "--header", f"Authorization: Bearer {header_token}"],
    }


def redact(obj: object) -> str:
    """Serialise for display, masking a literal bearer token (never the ${VAR} ref)."""
    text = json.dumps(obj, ensure_ascii=False)
    return re.sub(r'(Bearer\s+)(?!\$\{)[^"\\\s]{8,}', r"\1<REDACTED>", text)


def main() -> int:
    args = parse_args()
    if not SERVER_NAME_RE.fullmatch(args.server_name):
        raise SystemExit("server name may contain only letters, digits, '-' and '_'")

    header_token = resolve_token(args.token_mode, args.bearer_token_env_var)
    entry = build_entry(args, header_token)

    config_path = args.config.expanduser()
    if config_path.exists():
        try:
            config = json.loads(config_path.read_text(encoding="utf-8"))
        except json.JSONDecodeError as exc:
            raise SystemExit(f"{config_path} is not valid JSON: {exc}")
        if not isinstance(config, dict):
            raise SystemExit(f"{config_path}: top-level JSON is not an object.")
    else:
        config = {}

    servers = config.setdefault("mcpServers", {})
    if not isinstance(servers, dict):
        raise SystemExit(f"{config_path}: 'mcpServers' is not an object.")
    action = "update" if args.server_name in servers else "add"
    servers[args.server_name] = entry

    if args.dry_run:
        print(f"[dry-run] target: {config_path}")
        print(f"[dry-run] {action}: {args.server_name} -> {redact(entry)}")
        return 0

    config_path.parent.mkdir(parents=True, exist_ok=True)
    if config_path.exists():
        stamp = datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S")
        backup = config_path.with_name(f"{config_path.name}.bak-register-{stamp}")
        shutil.copy2(config_path, backup)
        print(f"backup: {backup}")

    with tempfile.NamedTemporaryFile(
        mode="w",
        encoding="utf-8",
        dir=config_path.parent,
        prefix=f".{config_path.name}.",
        suffix=".tmp",
        delete=False,
    ) as handle:
        json.dump(config, handle, ensure_ascii=False, indent=2)
        temp_path = Path(handle.name)
    if config_path.exists():
        shutil.copymode(config_path, temp_path)
    temp_path.replace(config_path)

    verify = json.loads(config_path.read_text(encoding="utf-8"))
    if args.server_name not in verify.get("mcpServers", {}):
        raise SystemExit("post-write verification failed: server not present.")

    print(f"{action}d '{args.server_name}' in {config_path}")
    print(f"  transport={args.transport} token-mode={args.token_mode} entry={redact(entry)}")
    if args.token_mode == "literal":
        print("  note: a literal token is now stored in the config file.")
    else:
        print(f"  note: token is referenced as ${{{args.bearer_token_env_var}}}; "
              f"ensure it is set in the environment Claude Code (and its chips) inherit.")
    print("Restart the Claude app so new sessions / chips pick up the change.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
