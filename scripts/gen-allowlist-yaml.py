#!/usr/bin/env python3
"""Regenerate `watching.allowed_extensions` / `watching.allowed_filenames` in
assets/default_configuration.yaml from the daemon's compiled allowlist.

The daemon never reads those YAML lists — every ingest path uses
`AllowedExtensions::default()`, whose source of truth is
src/rust/daemon/core/src/allowed_extensions/extensions.rs. Until 2026-09-19 the
YAML promised 355 extensions the daemon rejected (.conf, .properties, .cc,
.kts, .jl, man-page `.1`/`.5`, …), so the documentation and `make
coverage-audit` disagreed with the index. The YAML is now a MIRROR: edit the
Rust lists, run this script, and the validate-stage test
`default_configuration_yaml_mirrors_the_compiled_allowlist` fails the build
when the two drift apart.

Usage: scripts/gen-allowlist-yaml.py [--check]   (--check exits 1 on drift, writes nothing)
"""
import argparse
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
RUST = os.path.join(REPO, "src", "rust", "daemon", "core", "src", "allowed_extensions", "extensions.rs")
YAML = os.path.join(REPO, "assets", "default_configuration.yaml")

HEADER_EXT = """  # allowed_extensions: File extensions eligible for ingestion into `projects`
  # Purpose: Primary gate — only files with these extensions (or a name in
  #   allowed_filenames) are processed; everything else is never queued and
  #   never tracked.
  #
  # GENERATED MIRROR — do not edit here. The daemon reads the compiled list in
  # src/rust/daemon/core/src/allowed_extensions/extensions.rs (every ingest
  # path uses AllowedExtensions::default()); this block is regenerated from it
  # by scripts/gen-allowlist-yaml.py, and the validate-stage test
  # `default_configuration_yaml_mirrors_the_compiled_allowlist` fails the
  # build when the two differ. Library-only document formats (.pdf, .epub,
  # .docx, …) are a separate list in the same Rust file. Deliberately absent:
  # .conf .properties .cfg .ini and .env* — measured 2026-09-19, roughly one
  # in six of those files carries credentials (see the Rust file's note).
  # Format: Array of extension strings (with leading dot), sorted
"""

HEADER_NAMES = """  # allowed_filenames: Extension-less files recognized by exact name
  # Purpose: Recognize common extension-less files (Makefile, Dockerfile, etc.)
  # GENERATED MIRROR of PROJECT_FILENAME_LIST in extensions.rs — see
  # allowed_extensions above. Matched case-insensitively against the whole
  # file name; the glob variants (Dockerfile.*, Makefile.*, …) live in
  # PROJECT_FILENAME_GLOB_LIST and are not listed here.
  # Format: Array of exact filename strings, sorted
"""


def rust_list(text, name):
    m = re.search(rf"const {re.escape(name)}: &\[&str\] = &\[(.*?)\];", text, re.S)
    if not m:
        sys.exit(f"could not find {name} in extensions.rs")
    return re.findall(r'"([^"]+)"', re.sub(r"//[^\n]*", "", m.group(1)))


def block(header, key, items):
    lines = [header.rstrip("\n"), f"  {key}:"]
    lines += [f'    - "{i}"' for i in items]
    return "\n".join(lines) + "\n\n"


def comment_block_start(text, pos):
    """Start offset of the contiguous `  #` comment lines directly above the line at `pos`."""
    while True:
        prev = text.rfind("\n", 0, pos - 1)
        if text[prev + 1:pos].startswith("  #"):
            pos = prev + 1
        else:
            return pos


def replace_block(text, key, new_block):
    """Replace `  key:`, its comment header and its list — up to (not including)
    the next key's own comment header, so neighbouring documentation survives."""
    m = re.search(rf"^  {re.escape(key)}:[^\n]*\n", text, re.M)
    if not m:
        sys.exit(f"{key}: not found in the YAML")
    start = comment_block_start(text, m.start())
    after = re.search(r"^  [a-z_]+:", text[m.end():], re.M)
    if after:
        end = comment_block_start(text, m.end() + after.start())
        end = text.rfind("\n", 0, end - 1) + 1 if text[end - 2:end] == "\n\n" else end  # keep one blank line ours
    else:
        end = len(text)
    return text[:start] + new_block + text[end:]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--check", action="store_true")
    a = ap.parse_args()
    rust = open(RUST, encoding="utf-8").read()
    exts = sorted({e.lower() for e in rust_list(rust, "PROJECT_EXTENSION_LIST")})
    names = sorted(set(rust_list(rust, "PROJECT_FILENAME_LIST")), key=str.lower)
    current = open(YAML, encoding="utf-8").read()
    updated = replace_block(current, "allowed_extensions", block(HEADER_EXT, "allowed_extensions", exts))
    updated = replace_block(updated, "allowed_filenames", block(HEADER_NAMES, "allowed_filenames", names))
    if updated == current:
        print(f"default_configuration.yaml already mirrors extensions.rs ({len(exts)} extensions, {len(names)} filenames)")
        return 0
    if a.check:
        print("DRIFT: default_configuration.yaml does not mirror extensions.rs — run scripts/gen-allowlist-yaml.py")
        return 1
    with open(YAML, "w", encoding="utf-8", newline="\n") as f:
        f.write(updated)
    print(f"regenerated: {len(exts)} extensions, {len(names)} filenames")
    return 0


if __name__ == "__main__":
    sys.exit(main())
