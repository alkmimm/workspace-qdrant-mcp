//! By-name resolution of stub edges (tree-sitter's name-only references).

use std::collections::HashMap;

use sqlx::Row;
use tracing::debug;
use wqm_common::timestamps::now_utc;

use super::writes::INSERT_EDGE_SQL;
use super::SqliteGraphStore;
use crate::graph::{compute_edge_id, EdgeType, GenerationBranches, GraphDbResult};

/// One real definition a stub name can resolve to. A node unchanged across
/// versions of its file has one row per version, so it is listed once with
/// every generation that defines it.
struct Candidate {
    node_id: String,
    file_path: String,
    symbol_type: String,
    generations: Vec<String>,
}

impl SqliteGraphStore {
    pub(super) async fn resolve_stubs(
        &self,
        tenant_id: &str,
        membership: &GenerationBranches,
    ) -> GraphDbResult<u64> {
        // Dangling edges come in two orientations, both keyed on a file-less
        // stub node:
        //   - target-stub: CALLS / IMPORTS / USES_TYPE point at a name-only
        //     callee / module / type whose defining file is unknown.
        //   - source-stub: CONTAINS is authored from a file-less *parent
        //     container* stub — the class/struct node is created file-anchored
        //     from its OWN chunk, so the CONTAINS edge otherwise never lands on
        //     it (this is why `relations(class, filePath)` listed no members).
        // Both are repointed by name to the real project node; the file-less
        // stub is dropped once it has no edges left.
        // Both dangling queries drive from the small file-less-node set, then
        // probe edges by node id, forced with CROSS JOIN (loop order: nodes
        // outer) so the join starts from the file-less nodes instead of
        // full-scanning every edge of the tenant. `file_path = ''` lets the
        // plain composite index idx_nodes_file(tenant_id, file_path) drive the
        // scan with no `INDEXED BY` hint (a hint on a *partial* index failed the
        // whole query with "no query solution" on the daemon's SQLite).
        let target_dangling = sqlx::query(
            "SELECT e.edge_id, e.generation, e.source_node_id, e.edge_type, e.source_file,
                    e.weight, e.metadata_json, t.symbol_name AS peer_name
             FROM graph_nodes t
             CROSS JOIN graph_edges e ON e.target_node_id = t.node_id
             WHERE t.tenant_id = ?1 AND e.tenant_id = ?1 AND t.file_path = ''",
        )
        .bind(tenant_id)
        .fetch_all(&self.pool)
        .await?;

        let source_dangling = sqlx::query(
            "SELECT e.edge_id, e.generation, e.target_node_id, e.edge_type, e.source_file,
                    e.weight, e.metadata_json, s.symbol_name AS peer_name
             FROM graph_nodes s
             CROSS JOIN graph_edges e ON e.source_node_id = s.node_id
             WHERE s.tenant_id = ?1 AND e.tenant_id = ?1 AND s.file_path = ''",
        )
        .bind(tenant_id)
        .fetch_all(&self.pool)
        .await?;

        if target_dangling.is_empty() && source_dangling.is_empty() {
            return Ok(0);
        }

        // Real candidate nodes (resolved file_path, not file-typed), indexed by
        // symbol_name, one entry per node_id carrying every defining generation.
        let real_rows = sqlx::query(
            "SELECT node_id, generation, symbol_name, file_path, symbol_type, language
             FROM graph_nodes
             WHERE tenant_id = ?1 AND file_path <> '' AND symbol_type <> 'file'",
        )
        .bind(tenant_id)
        .fetch_all(&self.pool)
        .await?;

        let mut by_name: HashMap<String, Vec<Candidate>> = HashMap::new();
        // node_id -> language, stamped at extraction by the dynamic language
        // registry. Scopes call/type resolution to the caller's own language: a
        // TypeScript `.filter()` must not repoint onto a Rust `filter` (R3).
        // Only known, non-empty languages are recorded, so an unclassified node
        // never causes an over-drop.
        let mut node_lang: HashMap<String, String> = HashMap::new();
        for r in &real_rows {
            let name: String = r.get("symbol_name");
            let nid: String = r.get("node_id");
            let generation: String = r.get("generation");
            if let Some(l) = r.get::<Option<String>, _>("language") {
                if !l.is_empty() {
                    node_lang.insert(nid.clone(), l);
                }
            }
            let entries = by_name.entry(name).or_default();
            match entries.iter_mut().find(|c| c.node_id == nid) {
                Some(c) => c.generations.push(generation),
                None => entries.push(Candidate {
                    node_id: nid,
                    file_path: r.get("file_path"),
                    symbol_type: r.get("symbol_type"),
                    generations: vec![generation],
                }),
            }
        }

        // R2 scope map: member node_id -> its enclosing container node_id, from
        // CONTAINS edges. Lets the resolver prefer a same-named callee defined in
        // the CALLER's own class over a tenant-wide collision.
        let contained_by: HashMap<String, String> = sqlx::query(
            "SELECT source_node_id AS class_id, target_node_id AS member_id
             FROM graph_edges
             WHERE tenant_id = ?1 AND edge_type = 'CONTAINS'",
        )
        .bind(tenant_id)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|r| {
            (
                r.get::<String, _>("member_id"),
                r.get::<String, _>("class_id"),
            )
        })
        .collect();

        // R4 import map: (source_file, imported_symbol) -> module locators, read
        // from IMPORTS edge metadata ({"module":"…"} stamped at extraction). A
        // call to an imported name anchors to the definition file the caller
        // actually imported — the precise cross-package tier above proximity.
        let import_rows = sqlx::query(
            "SELECT DISTINCT e.source_file AS sf, n.symbol_name AS sym, e.metadata_json AS mj
             FROM graph_edges e JOIN graph_nodes n ON e.target_node_id = n.node_id
             WHERE e.tenant_id = ?1 AND e.edge_type = 'IMPORTS'
               AND e.metadata_json LIKE '%\"module\"%'",
        )
        .bind(tenant_id)
        .fetch_all(&self.pool)
        .await?;
        let mut imports_by_symbol: HashMap<(String, String), Vec<String>> = HashMap::new();
        for r in &import_rows {
            let mj: String = r.get("mj");
            if let Some(module) = Self::extract_module_field(&mj) {
                let modules = imports_by_symbol
                    .entry((r.get("sf"), r.get("sym")))
                    .or_default();
                if !modules.contains(&module) {
                    modules.push(module);
                }
            }
        }

        // Fan-out ceiling: beyond this many equally-plausible candidates the 1/N
        // keep-all is noise, not recall, so the call is left UNRESOLVED.
        // Env-tunable via WQM_GRAPH_FANOUT_CEILING; 0 disables it.
        let fanout_ceiling: usize = std::env::var("WQM_GRAPH_FANOUT_CEILING")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(16);

        // Resolve a stub name to real definition node(s), each with a CONFIDENCE
        // weight (docs/plans/2026-06-24-code-graph-resolution-roadmap.md, R1/R2):
        //   own-file definition        -> [(node, 1.0)]       precise
        //   caller's class (R2 scope)  -> [(node, 0.95)]      scoped
        //   unique                     -> [(node, 0.7)]       likely
        //   import-anchored (R4)       -> [(node, 0.90)]
        //   same-package (R2.5 prox.)  -> [(node, 0.85)]
        //   ambiguous (2..=ceiling)    -> [(c, 1/N) for each] KEEP ALL
        //   hyper-ambiguous (N>ceiling)-> []                  UNRESOLVED
        //   external / no match        -> []                  leave it a stub
        // The pool holds only definitions that share a branch with the edge's
        // own file version (`caller_gen`): a name defined once per branch is
        // unique on each branch, not ambiguous across all of them, and a
        // definition only another branch has can never be this edge's target.
        // `container_only` restricts the pool to container kinds.
        let pick_all = |name: &str,
                        own_file: &str,
                        caller_gen: &str,
                        caller_class: Option<&str>,
                        caller_lang: Option<&str>,
                        container_only: bool|
         -> Vec<(String, f64)> {
            let Some(candidates) = by_name.get(name) else {
                return Vec::new();
            };
            let pool: Vec<(&str, &str)> = candidates
                .iter()
                .filter(|c| !container_only || Self::is_container_node_type(&c.symbol_type))
                .filter(|c| {
                    c.generations
                        .iter()
                        .any(|g| membership.co_visible(caller_gen, g))
                })
                // Language scope: drop a candidate only when BOTH languages are
                // known AND differ (a cross-language false positive).
                .filter(|c| match (caller_lang, node_lang.get(c.node_id.as_str())) {
                    (Some(cl), Some(tl)) => cl == tl.as_str(),
                    _ => true,
                })
                .map(|c| (c.node_id.as_str(), c.file_path.as_str()))
                .collect();
            if pool.is_empty() {
                return Vec::new();
            }
            // Prefer a definition in the edge's own file (precise).
            if let Some((nid, _)) = pool.iter().find(|(_, fp)| *fp == own_file) {
                return vec![((*nid).to_string(), 1.0)];
            }
            // R2: prefer a candidate in the CALLER's own enclosing class.
            if let Some(cc) = caller_class {
                if let Some((nid, _)) = pool
                    .iter()
                    .find(|(nid, _)| contained_by.get(*nid).map(String::as_str) == Some(cc))
                {
                    return vec![((*nid).to_string(), 0.95)];
                }
            }
            // A unique name.
            if pool.len() == 1 {
                return vec![(pool[0].0.to_string(), 0.7)];
            }
            // R4 — import-anchored precedence: the caller's file imports THIS
            // name from a module that anchors EXACTLY ONE candidate's file.
            // CALLS/USES_TYPE only; a unique anchor only, never a guess.
            if !container_only {
                if let Some(modules) =
                    imports_by_symbol.get(&(own_file.to_string(), name.to_string()))
                {
                    let anchored: Vec<&str> = pool
                        .iter()
                        .filter(|(_, fp)| modules.iter().any(|m| Self::import_anchors_file(m, fp)))
                        .map(|(nid, _)| *nid)
                        .collect();
                    if anchored.len() == 1 {
                        return vec![(anchored[0].to_string(), 0.9)];
                    }
                }
            }
            // R2.5 — proximity precedence: EXACTLY ONE candidate in the deepest
            // directory prefix shared with the caller's file. CALLS/USES_TYPE
            // only (a CONTAINS parent is never guessed), unique bucket only.
            if !container_only {
                let max_depth = pool
                    .iter()
                    .map(|(_, fp)| Self::shared_dir_depth(own_file, fp))
                    .max()
                    .unwrap_or(0);
                if max_depth >= 1 {
                    let bucket: Vec<&str> = pool
                        .iter()
                        .filter(|(_, fp)| Self::shared_dir_depth(own_file, fp) == max_depth)
                        .map(|(nid, _)| *nid)
                        .collect();
                    if bucket.len() == 1 {
                        return vec![(bucket[0].to_string(), 0.85)];
                    }
                }
            }
            if fanout_ceiling > 0 && pool.len() > fanout_ceiling {
                return Vec::new();
            }
            // Ambiguous: keep EVERY candidate at confidence 1/N — below the
            // centrality gate (0.6), visible to impact/usages.
            let conf = 1.0 / pool.len() as f64;
            pool.iter()
                .map(|(nid, _)| ((*nid).to_string(), conf))
                .collect()
        };
        // Compact resolution provenance stamped onto each repointed edge.
        let resolution_metadata = |confidence: f64, n: usize| -> String {
            let tier = if confidence >= 0.99 {
                "in_file"
            } else if confidence >= 0.93 {
                "scoped"
            } else if confidence >= 0.88 {
                "import"
            } else if confidence >= 0.8 {
                "proximity"
            } else if n == 1 {
                "tenant_unique"
            } else {
                "ambiguous"
            };
            format!(
                "{{\"resolution\":\"{}\",\"confidence\":{:.4},\"candidates\":{}}}",
                tier, confidence, n
            )
        };

        let now = now_utc();
        let mut repointed: u64 = 0;
        let mut tx = self.pool.begin().await?;

        // Pass 1 — target-stub edges: repoint the TARGET to the real node(s),
        // in the edge's own generation (it stays owned by the same file version).
        for d in &target_dangling {
            let peer_name: String = d.get("peer_name");
            let source_file: String = d.get("source_file");
            let source_node_id: String = d.get("source_node_id");
            let generation: String = d.get("generation");
            let caller_class = contained_by.get(&source_node_id).map(String::as_str);
            let caller_lang = node_lang.get(&source_node_id).map(String::as_str);
            let candidates = pick_all(
                &peer_name,
                &source_file,
                &generation,
                caller_class,
                caller_lang,
                false,
            );
            if candidates.is_empty() {
                continue; // external/stdlib or unresolved — leave it a stub.
            }
            let edge_type_str: String = d.get("edge_type");
            let Some(edge_type) = EdgeType::from_str(&edge_type_str) else {
                continue;
            };
            let old_edge_id: String = d.get("edge_id");
            let mut emitted = false;
            for (new_target, confidence) in &candidates {
                // Skip self-loops (e.g. direct recursion) — no signal.
                if &source_node_id == new_target {
                    continue;
                }
                sqlx::query(INSERT_EDGE_SQL)
                    .bind(compute_edge_id(&source_node_id, new_target, edge_type))
                    .bind(&generation)
                    .bind(tenant_id)
                    .bind(&source_node_id)
                    .bind(new_target)
                    .bind(edge_type.as_str())
                    .bind(&source_file)
                    .bind(*confidence)
                    .bind(resolution_metadata(*confidence, candidates.len()))
                    .bind(&now)
                    .execute(&mut *tx)
                    .await?;
                emitted = true;
            }
            if emitted {
                delete_edge_row(&mut tx, tenant_id, &old_edge_id, &generation).await?;
                repointed += 1;
            }
        }

        // Pass 2 — source-stub edges (CONTAINS from a file-less container stub):
        // repoint the SOURCE to the real container node of the same name.
        for d in &source_dangling {
            let peer_name: String = d.get("peer_name");
            let source_file: String = d.get("source_file");
            let target_node_id: String = d.get("target_node_id");
            let generation: String = d.get("generation");
            // A container and its member share a language.
            let ref_lang = node_lang.get(&target_node_id).map(String::as_str);
            // Containment is structural (one owner): keep ONLY a confident match
            // (own-file or unique), never fan out an ambiguous container name.
            let Some(new_source) =
                pick_all(&peer_name, &source_file, &generation, None, ref_lang, true)
                    .into_iter()
                    .find(|(_, c)| *c >= 0.7)
                    .map(|(nid, _)| nid)
            else {
                continue;
            };
            if target_node_id == new_source {
                continue;
            }
            let edge_type_str: String = d.get("edge_type");
            let Some(edge_type) = EdgeType::from_str(&edge_type_str) else {
                continue;
            };
            let old_edge_id: String = d.get("edge_id");
            let weight: f64 = d.get("weight");
            let metadata_json: Option<String> = d.get("metadata_json");
            sqlx::query(INSERT_EDGE_SQL)
                .bind(compute_edge_id(&new_source, &target_node_id, edge_type))
                .bind(&generation)
                .bind(tenant_id)
                .bind(&new_source)
                .bind(&target_node_id)
                .bind(edge_type.as_str())
                .bind(&source_file)
                .bind(weight)
                .bind(&metadata_json)
                .bind(&now)
                .execute(&mut *tx)
                .await?;
            delete_edge_row(&mut tx, tenant_id, &old_edge_id, &generation).await?;
            repointed += 1;
        }

        tx.commit().await?;

        // Drop REFERENCES edges that never resolved (#369): an unresolved
        // REFERENCES edge is almost always a local variable or parameter that
        // merely looked like a top-level symbol at extraction time. Runs before
        // the orphan sweep below so the stubs left behind are collected too.
        let dropped_refs = sqlx::query(
            "DELETE FROM graph_edges
             WHERE tenant_id = ?1 AND edge_type = 'REFERENCES'
               AND target_node_id IN (
                   SELECT node_id FROM graph_nodes
                   WHERE tenant_id = ?1 AND file_path = ''
               )",
        )
        .bind(tenant_id)
        .execute(&self.pool)
        .await?
        .rows_affected();

        // Drop stub nodes that no longer have any edges.
        sqlx::query(
            "DELETE FROM graph_nodes
             WHERE tenant_id = ?1 AND file_path = ''
               AND node_id NOT IN (
                   SELECT source_node_id FROM graph_edges WHERE tenant_id = ?1
                   UNION
                   SELECT target_node_id FROM graph_edges WHERE tenant_id = ?1
               )",
        )
        .bind(tenant_id)
        .execute(&self.pool)
        .await?;

        debug!(
            "Resolved {} stub edges for tenant {} ({} target + {} source dangling examined, \
             {} unresolved REFERENCES dropped)",
            repointed,
            tenant_id,
            target_dangling.len(),
            source_dangling.len(),
            dropped_refs
        );
        Ok(repointed)
    }
}

async fn delete_edge_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    tenant_id: &str,
    edge_id: &str,
    generation: &str,
) -> sqlx::Result<()> {
    sqlx::query(
        "DELETE FROM graph_edges WHERE edge_id = ?1 AND generation = ?2 AND tenant_id = ?3",
    )
    .bind(edge_id)
    .bind(generation)
    .bind(tenant_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
