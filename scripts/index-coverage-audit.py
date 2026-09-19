#!/usr/bin/env python3
"""Index coverage audit: what git tracks vs what the index holds, per project.

The daemon's `indexing.percent` counts the queue of what its walk SAW. A file the
walk never reaches — an ignore rule, an extension outside the allowlist, a size
cap, an ingest that produced nothing — is absent from the index AND from every
percentage, while native grep still sees it. On 2026-09-19 that was 415
hexagonal `adapters/out` sources in one repo (global `out/` rule) that an agent
found with native grep and could not find in the index.

For every enabled watch folder this script compares `git ls-files` on the
checked-out branch with `tracked_files` rows holding that branch, and sorts
each missing file into the gate that dropped it, in the daemon's own order:

  allowlist   extension/filename not in the daemon's COMPILED allowlist
              (src/rust/daemon/core/src/allowed_extensions/extensions.rs is
              the list the daemon runs; every call site uses ::default())
  allowlist-drift
              not in the compiled list, but LISTED under `allowed_extensions`
              in assets/default_configuration.yaml — which the daemon never
              reads. The documentation promises what the daemon does not do
              (2026-09-19: .properties, .conf, .ini, .kts, .plist, .service,
              .jinja2, .patch, .cfg — 226 files across the watched repos).
  size        over max_file_size (or the stricter size_restricted cap)
  global      matched a rule of global.wqmignore   (git check-ignore -v)
  wqmignore   matched by the repo's .wqmignore
  gitignore   tracked by git but matched by the repo's .gitignore (walk skips it)
  ELIGIBLE    none of the above — the walk should have indexed it. This bucket
              is the defect; everything else is a rule someone chose.

Read-only. Needs docker (the daemon's SQLite volume), git and python3 on the
WSL side. Exit 0 when the ELIGIBLE bucket is empty for every project, 1 when
it is not, so it can gate an experiment run (runbook experiment-freeze.md §4).

Usage: scripts/index-coverage-audit.py [--repo <path>]... [--show N] [--json]
"""
import argparse
import fnmatch
import json
import os
import re
import subprocess
import sys
from collections import Counter, defaultdict

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
VOLUME = "workspace-qdrant-mcp_memexd_db"
IMAGE = "workspace-qdrant-mcp-memexd:local"
GLOBAL_IGNORE = os.path.join(REPO, "state", "memexd", "global.wqmignore")
DEFAULT_CONFIG = os.path.join(REPO, "assets", "default_configuration.yaml")
RUST_ALLOWLIST = os.path.join(REPO, "src", "rust", "daemon", "core", "src", "allowed_extensions", "extensions.rs")
BUCKET_ORDER = ("ELIGIBLE", "allowlist-drift", "global", "wqmignore", "gitignore", "allowlist", "size", "missing-on-disk")
DEFECT_FLAGS = {
    "ELIGIBLE": "  <-- DEFECT (walk/ingest dropped an eligible file)",
    "allowlist-drift": "  <-- DEFECT (default_configuration.yaml promises it, the daemon does not read that list)",
}


def rust_list(text, name):
    """String items of `const NAME: &[&str] = &[ ... ];` in extensions.rs, comments stripped."""
    m = re.search(rf"const {re.escape(name)}: &\[&str\] = &\[(.*?)\];", text, re.S)
    if not m:
        sys.exit(f"could not find {name} in extensions.rs")
    body = re.sub(r"//[^\n]*", "", m.group(1))
    return re.findall(r'"([^"]+)"', body)


def sh(cmd, cwd=None, stdin=None):
    return subprocess.run(cmd, cwd=cwd, input=stdin, text=True, capture_output=True, check=False)


def yaml_list(text, key):
    """Items of a two-space-indented `key:` list in default_configuration.yaml (no YAML lib on the WSL side)."""
    out, on = [], False
    for line in text.splitlines():
        if re.match(rf"^  {re.escape(key)}:\s*$", line):
            on = True
            continue
        if on:
            if re.match(r"^  [a-z_]+:", line) or (line.strip() and not line.startswith("    ")):
                break
            m = re.match(r'^\s*-\s*"?([^"#]+?)"?\s*(#.*)?$', line)
            if m:
                out.append(m.group(1).strip())
    return out


def yaml_scalar(text, key, default):
    m = re.search(rf"^\s*{re.escape(key)}:\s*([^\s#]+)", text, re.M)
    return m.group(1) if m else default


def size_mb(s):
    m = re.match(r"^(\d+(?:\.\d+)?)\s*(MB|KB|GB)?$", s, re.I)
    if not m:
        return float(s)
    n, u = float(m.group(1)), (m.group(2) or "MB").upper()
    return n * {"KB": 1 / 1024, "MB": 1, "GB": 1024}[u]


def tracked_from_db():
    code = (
        "import sqlite3, json\n"
        "c = sqlite3.connect('file:/live/memexd.db?mode=ro', uri=True)\n"
        "wf = c.execute('select watch_id, path, tenant_id from watch_folders where enabled=1').fetchall()\n"
        "rows = c.execute('select watch_folder_id, relative_path, branches from tracked_files').fetchall()\n"
        "print(json.dumps({'folders': wf, 'rows': rows}))\n"
    )
    r = sh(["docker", "run", "--rm", "-v", f"{VOLUME}:/live:ro", "--entrypoint", "python3", IMAGE, "-c", code])
    if r.returncode != 0:
        sys.exit(f"cannot read the daemon database: {r.stderr.strip()}")
    return json.loads(r.stdout)


def check_ignore(path, files, excludes_file):
    """{relative_path: 'source:line pattern'} for files an ignore layer drops.

    One layer at a time: `excludes_file` as core.excludesFile with the repo's own
    ignore files switched off (`--no-index` still reads .gitignore, so hits from
    another source are discarded), or `None` for the repo's .gitignore layer.
    A negated pattern (`!…`) as the last match means re-included — not ignored.
    """
    if not files:
        return {}
    cmd = ["git"]
    if excludes_file is not None:
        cmd += ["-c", "core.excludesFile=" + excludes_file]
    cmd += ["check-ignore", "-v", "-z", "--stdin", "--no-index"]
    r = sh(cmd, cwd=path, stdin="\0".join(files) + "\0")
    parts = r.stdout.split("\0")
    out = {}
    for i in range(0, len(parts) - 3, 4):  # -z -v: source, line, pattern, path
        src, ln, pat, p = parts[i], parts[i + 1], parts[i + 2], parts[i + 3]
        if not p or pat.startswith("!"):
            continue
        base = os.path.basename(src)
        if excludes_file is None and base != ".gitignore":
            continue
        if excludes_file is not None and base != os.path.basename(excludes_file):
            continue
        out[p] = f"{base}:{ln} {pat}"
    return out


def audit_folder(path, branch_rows, compiled, promised, restricted, max_mb, restricted_mb, show):
    branch = sh(["git", "branch", "--show-current"], cwd=path).stdout.strip() or "main"
    tracked_git = [p for p in sh(["git", "ls-files", "-z"], cwd=path).stdout.split("\0") if p]
    indexed = branch_rows.get(branch, set())
    missing = [p for p in tracked_git if p not in indexed]

    buckets, examples = Counter(), defaultdict(list)

    def put(k, p, why=""):
        buckets[k] += 1
        if len(examples[k]) < show:
            examples[k].append(p + (f"  <- {why}" if why else ""))

    survivors = []
    for p in missing:
        base = os.path.basename(p)
        ext = os.path.splitext(base)[1].lower()
        lower = base.lower()
        accepted = (ext in compiled["exts"] or lower in compiled["names"]
                    or any(fnmatch.fnmatchcase(lower, g) for g in compiled["globs"]))
        if not accepted:
            if ext in promised["exts"] or base in promised["names"]:
                put("allowlist-drift", p, f"{ext or base} only in default_configuration.yaml")
            else:
                put("allowlist", p, ext or "(no extension)")
            continue
        try:
            mb = os.path.getsize(os.path.join(path, p)) / 1048576
        except OSError:
            put("missing-on-disk", p)
            continue
        cap = restricted_mb if ext in restricted else max_mb
        if mb > cap:
            put("size", p, f"{mb:.1f} MB > {cap:g} MB")
            continue
        survivors.append(p)

    verdict = {}
    layers = [("global", GLOBAL_IGNORE), ("wqmignore", os.path.join(path, ".wqmignore")), ("gitignore", None)]
    for label, excludes in layers:
        if excludes is not None and not os.path.isfile(excludes):
            continue
        remaining = [p for p in survivors if p not in verdict]
        for p, why in check_ignore(path, remaining, excludes).items():
            verdict[p] = (label, why)
    for p in survivors:
        if p in verdict:
            put(verdict[p][0], p, verdict[p][1])
        else:
            put("ELIGIBLE", p)

    return {
        "branch": branch,
        "git_tracked": len(tracked_git),
        "indexed_on_branch": len(indexed),
        "missing": len(missing),
        "buckets": dict(buckets),
        "examples": dict(examples),
    }


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--repo", action="append", help="limit to these watch folder paths")
    ap.add_argument("--show", type=int, default=8, help="examples per bucket")
    ap.add_argument("--json", action="store_true")
    a = ap.parse_args()

    cfg = open(DEFAULT_CONFIG, encoding="utf-8").read()
    exts = {e.lower() for e in yaml_list(cfg, "allowed_extensions")}
    names = set(yaml_list(cfg, "allowed_filenames"))
    restricted = {e.lower() for e in yaml_list(cfg, "size_restricted_extensions")}
    max_mb = size_mb(yaml_scalar(cfg, "max_file_size", "50MB"))
    restricted_mb = float(yaml_scalar(cfg, "size_restricted_max_mb", "5"))
    if not exts:
        sys.exit("could not parse allowed_extensions from default_configuration.yaml")
    rust = open(RUST_ALLOWLIST, encoding="utf-8").read()
    compiled = {
        "exts": {e.lower() for e in rust_list(rust, "PROJECT_EXTENSION_LIST")},
        "names": {n.lower() for n in rust_list(rust, "PROJECT_FILENAME_LIST")},
        "globs": [g.lower() for g in rust_list(rust, "PROJECT_FILENAME_GLOB_LIST")],
    }
    promised = {"exts": exts, "names": names}

    db = tracked_from_db()
    by_folder = defaultdict(lambda: defaultdict(set))  # watch_id -> branch -> {relative_path}
    for wid, rel, branches in db["rows"]:
        try:
            bs = json.loads(branches or "[]")
        except ValueError:
            bs = []
        for b in bs or [""]:
            by_folder[wid][b].add(rel)

    report, defect = {}, False
    for wid, path, tenant in sorted(db["folders"], key=lambda f: f[1]):
        if a.repo and path not in a.repo:
            continue
        if not os.path.exists(os.path.join(path, ".git")):
            continue
        r = audit_folder(path, by_folder.get(wid, {}), compiled, promised, restricted, max_mb, restricted_mb, a.show)
        r["tenant"] = tenant
        report[path] = r
        if any(r["buckets"].get(k) for k in DEFECT_FLAGS):
            defect = True

    if a.json:
        print(json.dumps(report, indent=2, ensure_ascii=False))
    else:
        for path, r in report.items():
            print(f"\n{os.path.basename(path)}  [{r['branch']}]  git={r['git_tracked']}  "
                  f"indexed={r['indexed_on_branch']}  missing={r['missing']}")
            for k in BUCKET_ORDER:
                if r["buckets"].get(k):
                    print(f"  {k:<16}{r['buckets'][k]:>6}{DEFECT_FLAGS.get(k, '')}")
                    for e in r["examples"][k]:
                        print(f"      {e}")
        totals = Counter()
        for r in report.values():
            totals.update({k: v for k, v in r["buckets"].items() if k in DEFECT_FLAGS})
        print("\nresult:", ("DEFECT — " + ", ".join(f"{k}={v}" for k, v in totals.items())) if defect
              else "no eligible or promised file is missing from the index")
    sys.exit(1 if defect else 0)


if __name__ == "__main__":
    main()
