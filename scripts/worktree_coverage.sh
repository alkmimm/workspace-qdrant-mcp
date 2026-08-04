#!/usr/bin/env bash
# worktree_coverage — auditoria de cobertura de git worktrees no índice workspace-qdrant.
#
# Responde, para um repo, as perguntas que já causaram bugs de busca com worktree:
#   A. COBERTURA  — para cada worktree on-disk (`git worktree list`), a sua branch
#                   aparece em tracked_files (baseline do folder principal)? Esse é
#                   o modelo B1: o worktree NÃO precisa ser um watch_folder próprio;
#                   o daemon taggeia a branch do worktree no baseline compartilhado
#                   (dedup fast-path). Sem a tag, a busca branch-scoped de dentro do
#                   worktree só vê `main`-widened. Registro é informativo, não gate.
#   B. QDRANT     — branch com arquivos no índice mas 0 pontos no Qdrant = silent-0
#                   (a classe de drift #224/#316: SQLite ok, semantic search vazio).
#   C. HIGIENE    — tags stale (branch sumiu do git), branches nunca observadas,
#                   e tenants `local_*` órfãos (a assinatura do re-key #299/F-014).
#
# Read-only: abre o DB do daemon em modo `ro` e só faz COUNT no Qdrant.
# Uso:   scripts/worktree_coverage.sh [/abs/path/do/repo]   (default: CWD)
# Exit:  0 = sem FAIL (pode haver WARN/INFO);  2 = há FAIL;  3 = repo não registrado.
# Requer: rodar no WSL (nativo ext4) com o container wqm-memexd de pé.
#
# NOTA de exit-code: `docker exec -i` com heredoc no stdin ENGOLE o código de saída
# do processo remoto (retorna 0 mesmo com sys.exit != 0). Por isso o python emite um
# marcador `__RC__=N` na última linha e é o bash quem decide o exit real.
set -euo pipefail

REPO="${1:-$(pwd)}"
REPO="$(cd "$REPO" && git rev-parse --show-toplevel 2>/dev/null || echo "$REPO")"
CONTAINER="${WQM_MEMEXD_CONTAINER:-wqm-memexd}"

if ! git -C "$REPO" rev-parse --git-dir >/dev/null 2>&1; then
  echo "ERRO: $REPO não é um repositório git" >&2; exit 3
fi

CUR="$(git -C "$REPO" rev-parse --abbrev-ref HEAD)"
BR_CSV="$(git -C "$REPO" for-each-ref --format='%(refname:short)' refs/heads | paste -sd, -)"
# `git worktree list --porcelain` (path + branch por worktree) → base64 p/ evitar quoting.
WT_B64="$(git -C "$REPO" worktree list --porcelain | base64 | tr -d '\n')"

TMP="$(mktemp)"; trap 'rm -f "$TMP"' EXIT
docker exec -i "$CONTAINER" python3 - "$REPO" "$CUR" "$BR_CSV" "$WT_B64" <<'PYEOF' > "$TMP" 2>&1 || true
import sqlite3, sys, json, base64, urllib.request

REPO, CUR, BR_CSV, WT_B64 = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
GIT_BRANCHES = set(b for b in BR_CSV.split(",") if b)
QDRANT = "http://qdrant:6333"
fails, warns = [], []

def emit_rc(n):
    print("__RC__=%d" % n)

# --- parse `git worktree list --porcelain`: (path, branch) por worktree ---
worktrees, cp, cb = [], None, None
for line in base64.b64decode(WT_B64).decode(errors="replace").splitlines():
    if line.startswith("worktree "):
        cp, cb = line[9:], None
    elif line.startswith("branch "):
        cb = line[7:].replace("refs/heads/", "")
    elif line == "" and cp is not None:
        worktrees.append((cp, cb)); cp = cb = None
if cp is not None:
    worktrees.append((cp, cb))
linked_wts = worktrees[1:]  # [0] é o próprio main working tree

def qd_count(must):
    body = json.dumps({"filter": {"must": must}, "exact": True}).encode()
    req = urllib.request.Request(QDRANT + "/collections/projects/points/count",
                                 data=body, headers={"Content-Type": "application/json"}, method="POST")
    return json.load(urllib.request.urlopen(req, timeout=30))["result"]["count"]

def idx_files_on(wid, branch):
    if not branch:
        return 0
    return st.execute("""SELECT COUNT(*) FROM tracked_files WHERE watch_folder_id=?
        AND EXISTS(SELECT 1 FROM json_each(branches) WHERE value=?)""", (wid, branch)).fetchone()[0]

st = sqlite3.connect("file:/var/lib/memexd/memexd.db?mode=ro", uri=True)
row = st.execute("SELECT watch_id, tenant_id FROM watch_folders WHERE path=? AND collection='projects'", (REPO,)).fetchone()
if not row:
    print("FAIL: nenhum watch_folder registrado para path=%s" % REPO)
    print("      (o repo nunca foi registrado; abra uma sessão MCP nele ou use `wqm project add`)")
    emit_rc(3); sys.exit(0)
main_wid, tenant = row
print("repo      = %s" % REPO)
print("tenant_id = %s   main watch_id = %s   HEAD = %s" % (tenant, main_wid, CUR))

# watch_folders do tenant (main + worktrees registrados)
sib = st.execute("""SELECT COALESCE(is_worktree,0), COALESCE(is_active,0), COALESCE(is_archived,0),
    COALESCE(enabled,1), path FROM watch_folders WHERE tenant_id=? AND collection='projects'
    ORDER BY COALESCE(is_worktree,0)""", (tenant,)).fetchall()
registered_wt_paths = set(p for wt, _, _, _, p in sib if wt)
print("\n== watch_folders do tenant ==")
for wt, act, arch, en, path in sib:
    print("  %-8s act=%d arch=%d en=%d %s" % ("WORKTREE" if wt else "main", act, arch, en, path))

# ---------- A. COBERTURA por worktree on-disk ----------
print("\n== A. COBERTURA (worktrees on-disk × índice) ==")
print("  worktrees ligados on-disk: %d | registrados no daemon: %d" % (len(linked_wts), len(registered_wt_paths)))
if not linked_wts:
    print("  (nenhum worktree ligado on-disk — nada a cobrir)")
for path, branch in linked_wts:
    registered = path in registered_wt_paths
    src = "registrado" if registered else "baseline do main"
    if branch is None:
        verdict = "WARN detached-HEAD (sem branch p/ taggear)"
        warns.append("worktree em detached HEAD: %s" % path)
    else:
        # Cobertura B1 = a branch do worktree está taggeada no baseline do main.
        # Registro NÃO é requisito: o daemon cobre via tag sob o folder principal.
        n = idx_files_on(main_wid, branch)
        if n > 0:
            verdict = "ok  coberto: %d arquivos sob a branch (%s)" % (n, src)
        else:
            verdict = "WARN branch sem tag no baseline (rode um scan/reindex do tenant)"
            warns.append("worktree branch '%s' sem tag no índice: %s" % (branch, path))
    print("  %-55s branch=%-40s -> %s" % (path, branch, verdict))

# ---------- B. QDRANT cross-check (silent-0 / drift) ----------
print("\n== B. QDRANT cross-check (silent-0 guard) ==")
try:
    tot = qd_count([{"key": "tenant_id", "match": {"value": tenant}}])
    print("  pontos do tenant: %d" % tot)
    if tot == 0:
        fails.append("tenant tem 0 pontos no Qdrant mas está registrado — possível re-key #299 (rode reembed force=true)")
    check = sorted({CUR} | {b for _, b in linked_wts if b})
    for b in check:
        fi = idx_files_on(main_wid, b)
        pts = qd_count([{"key": "tenant_id", "match": {"value": tenant}}, {"key": "branch", "match": {"value": b}}])
        drift = fi > 0 and pts == 0
        if drift:
            fails.append("DRIFT: branch '%s' tem %d arquivos no índice mas 0 pontos no Qdrant (silent-0)" % (b, fi))
        print("  %-4s branch=%-40s idx_files=%-5d qdrant_points=%d" % ("FAIL" if drift else "ok", b, fi, pts))
except Exception as e:
    warns.append("Qdrant inacessível (%s) — cross-check B pulado" % e)
    print("  WARN Qdrant inacessível: %s" % e)

# ---------- C. HIGIENE ----------
print("\n== C. HIGIENE ==")
idx_branches = set()
for (b,) in st.execute("SELECT DISTINCT branches FROM tracked_files WHERE watch_folder_id=?", (main_wid,)):
    try:
        idx_branches.update(json.loads(b))
    except Exception:
        pass
stale = sorted(idx_branches - GIT_BRANCHES)
unindexed = sorted(GIT_BRANCHES - idx_branches)
print("  branch labels: index=%d  git=%d" % (len(idx_branches), len(GIT_BRANCHES)))
if stale:
    warns.append("tags stale (branch sumiu do git, %d): %s" % (len(stale), stale[:8]))
    print("  WARN stale index tags (candidatas a branch_prune): %s" % stale[:8])
if unindexed:
    print("  INFO %d branch(es) do git sem tag no índice (nunca observadas em checkout/worktree):" % len(unindexed))
    print("       %s%s" % (unindexed[:6], " ..." if len(unindexed) > 6 else ""))
orphans = st.execute("SELECT tenant_id, COALESCE(is_archived,0) FROM watch_folders WHERE tenant_id LIKE 'local\\_%' ESCAPE '\\'").fetchall()
active_orphans = [o[0] for o in orphans if o[1] == 0]
if active_orphans:
    fails.append("tenant(s) local_* ativo(s) — assinatura de re-key #299: %s" % active_orphans)
print("  tenants local_* (global): %d total, %d ativos" % (len(orphans), len(active_orphans)))

# ---------- RESUMO ----------
print("\n== RESUMO ==")
for f in fails:
    print("  FAIL  " + f)
for w in warns:
    print("  WARN  " + w)
if not fails and not warns:
    print("  PASS  cobertura de worktree consistente")
elif not fails:
    print("  PASS (com WARN)  nenhuma falha crítica")
emit_rc(2 if fails else 0)
PYEOF

cat "$TMP"
RC="$(grep -oE '^__RC__=[0-9]+' "$TMP" | tail -1 | cut -d= -f2 || true)"
exit "${RC:-0}"
