#!/usr/bin/env python3
"""Register the workspace-qdrant HTTP MCP server in Codex on Linux/WSL."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile


SERVER_NAME_RE = re.compile(r"^[A-Za-z0-9_-]+$")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--codex-home", type=Path, required=True)
    parser.add_argument("--codex-bin", default="codex")
    parser.add_argument("--server-name", default="workspace-qdrant")
    parser.add_argument("--url", default="http://localhost:6335/mcp")
    parser.add_argument("--bearer-token-env-var", default="MCP_HTTP_TOKEN")
    return parser.parse_args()


def replace_server_options(
    config_path: Path,
    server_name: str,
    startup_timeout: int,
    tool_timeout: int,
    enabled_tools: list[str],
) -> None:
    header = f"[mcp_servers.{server_name}]"
    lines = config_path.read_text(encoding="utf-8").splitlines()

    try:
        start = lines.index(header)
    except ValueError as exc:
        raise RuntimeError(f"Codex did not create the expected table {header}") from exc

    end = next(
        (index for index in range(start + 1, len(lines)) if lines[index].startswith("[")),
        len(lines),
    )
    managed_keys = {
        "startup_timeout_sec",
        "tool_timeout_sec",
        "required",
        "enabled_tools",
    }
    section = [
        line
        for line in lines[start:end]
        if line.split("=", 1)[0].strip() not in managed_keys
    ]
    tools_toml = ", ".join(json.dumps(tool) for tool in enabled_tools)
    section.extend(
        [
            f"startup_timeout_sec = {startup_timeout}",
            f"tool_timeout_sec = {tool_timeout}",
            "required = true",
            f"enabled_tools = [{tools_toml}]",
        ]
    )

    content = "\n".join(lines[:start] + section + lines[end:]).rstrip() + "\n"
    config_path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(
        mode="w",
        encoding="utf-8",
        dir=config_path.parent,
        prefix=f".{config_path.name}.",
        delete=False,
    ) as handle:
        handle.write(content)
        temp_path = Path(handle.name)
    if config_path.exists():
        shutil.copymode(config_path, temp_path)
    temp_path.replace(config_path)


def main() -> int:
    args = parse_args()
    if not SERVER_NAME_RE.fullmatch(args.server_name):
        raise SystemExit("server name may contain only letters, digits, '-' and '_'")
    if shutil.which(args.codex_bin) is None:
        raise SystemExit(f"Codex CLI not found: {args.codex_bin}")

    public_config_path = (
        args.repo
        / "src/typescript/mcp-server/src/constants/mcp-public-config.json"
    )
    public_config = json.loads(public_config_path.read_text(encoding="utf-8"))
    startup_timeout = int(public_config["codex"]["startup_timeout_sec"])
    tool_timeout = int(public_config["codex"]["tool_timeout_sec"])
    enabled_tools = list(public_config["publicTools"])

    codex_home = args.codex_home.expanduser().resolve()
    codex_home.mkdir(parents=True, exist_ok=True)
    config_path = codex_home / "config.toml"
    original_config = config_path.read_bytes() if config_path.exists() else None
    original_mode = config_path.stat().st_mode if config_path.exists() else None
    env = os.environ.copy()
    env["CODEX_HOME"] = str(codex_home)
    command = [
        args.codex_bin,
        "mcp",
        "add",
        args.server_name,
        "--url",
        args.url,
        "--bearer-token-env-var",
        args.bearer_token_env_var,
    ]
    try:
        subprocess.run(command, check=True, env=env)
        replace_server_options(
            config_path,
            args.server_name,
            startup_timeout,
            tool_timeout,
            enabled_tools,
        )

        result = subprocess.run(
            [args.codex_bin, "mcp", "get", args.server_name, "--json"],
            check=True,
            env=env,
            capture_output=True,
            text=True,
        )
        registered = json.loads(result.stdout)
        transport = registered.get("transport", {})
        expected = {
            "url": args.url,
            "bearer_token_env_var": args.bearer_token_env_var,
        }
        if any(transport.get(key) != value for key, value in expected.items()):
            raise RuntimeError("Codex reported an unexpected MCP transport configuration")
        if registered.get("startup_timeout_sec") != startup_timeout:
            raise RuntimeError("Codex did not load the configured startup timeout")
        if registered.get("tool_timeout_sec") != tool_timeout:
            raise RuntimeError("Codex did not load the configured tool timeout")
        if registered.get("enabled_tools") != enabled_tools:
            raise RuntimeError("Codex did not load the canonical enabled-tools allowlist")
    except Exception:
        if original_config is None:
            config_path.unlink(missing_ok=True)
        else:
            config_path.write_bytes(original_config)
            if original_mode is not None:
                config_path.chmod(original_mode)
        raise

    print(f"Codex MCP '{args.server_name}' registered successfully.")
    print(f"Config: {config_path}")
    print(f"URL: {args.url}")
    if not os.environ.get(args.bearer_token_env_var):
        print(
            f"Warning: export {args.bearer_token_env_var} before starting Codex ",
            "so it can authenticate to the MCP server.",
            file=sys.stderr,
            sep="",
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
