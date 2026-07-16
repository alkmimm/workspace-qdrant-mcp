#!/usr/bin/env bash
# tracked-but-unindexed — guardrail de cobertura do índice workspace-qdrant.
#
# Compara `git ls-files` (verdade do repo) com `tracked_files` (índice do daemon)
# e reporta:
#   1. TRACKED-BUT-UNINDEXED — arquivos que o git rastreia e o índice NÃO tem.
#      Fatal para auditorias: zero-hits nesses paths é cegueira, não ausência.
#      (Causa típica: allowlist de .gitignore aninhado apodrecido — a walk do
#      daemon é index-blind, "tracked" não vence "ignored"; caso proto/ 2026-07-16.)
#   2. INDEXED-NOT-ON-HEAD — indexados sem a tag da branch atual (invisíveis a
#      reads branch-scoped; informativo, geralmente transiente).
#
# Uso:   tracked_but_unindexed.sh /abs/path/do/repo [prefixo/]
# Exit:  0 = cobertura ok; 2 = há tracked-but-unindexed (falhe a auditoria).
# Requer: rodar no WSL com o container wqm-memexd de pé.
set -euo pipefail

REPO="${1:?uso: $0 /abs/path/do/repo [prefixo/]}"
PREFIX="${2:-}"
HEAD_BRANCH="$(git -C "$REPO" rev-parse --abbrev-ref HEAD)"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# Verdade do git (arquivos rastreados, opcionalmente sob um prefixo).
git -C "$REPO" ls-files -- "${PREFIX:-.}" | LC_ALL=C sort > "$TMP/tracked"

# Verdade do índice (paths + presença da tag da branch atual), via daemon DB.
docker exec -i wqm-memexd python3 - "$REPO" "$PREFIX" "$HEAD_BRANCH" <<'PYEOF' > "$TMP/index.json"
import sqlite3, sys, json
repo, prefix, head = sys.argv[1], sys.argv[2], sys.argv[3]
st = sqlite3.connect("file:/var/lib/memexd/memexd.db?mode=ro", uri=True)
wf = st.execute("SELECT watch_id FROM watch_folders WHERE path=? AND enabled=1", (repo,)).fetchone()
if not wf:
    print(json.dumps({"error": f"nenhum watch_folder habilitado com path={repo}"})); sys.exit(0)
like = (prefix + "%") if prefix else "%"
rows = st.execute("""SELECT DISTINCT relative_path,
    EXISTS(SELECT 1 FROM json_each(branches) WHERE value=?) AS on_head
    FROM tracked_files WHERE watch_folder_id=? AND relative_path LIKE ?""",
    (head, wf[0], like)).fetchall()
print(json.dumps({"indexed": [r[0] for r in rows],
                  "not_on_head": [r[0] for r in rows if not r[1]]}))
PYEOF

python3 - "$TMP" <<'PYEOF'
import json, sys, os
tmp = sys.argv[1]
data = json.load(open(f"{tmp}/index.json"))
if "error" in data:
    print(f"ERRO: {data['error']}"); sys.exit(3)
tracked = set(open(f"{tmp}/tracked").read().splitlines())
indexed = set(data["indexed"])
missing = sorted(tracked - indexed)
not_on_head = sorted(set(data["not_on_head"]) & tracked)
print(f"tracked(git)={len(tracked)}  indexed={len(indexed)}")
print(f"TRACKED-BUT-UNINDEXED: {len(missing)}")
for m in missing[:50]:
    print(f"   MISSING  {m}")
if len(missing) > 50:
    print(f"   ... +{len(missing)-50}")
print(f"indexed-not-on-HEAD (informativo): {len(not_on_head)}")
for m in not_on_head[:10]:
    print(f"   no-head  {m}")
open(f"{tmp}/exitcode", "w").write("2" if missing else "0")
PYEOF

exit "$(cat "$TMP/exitcode")"
