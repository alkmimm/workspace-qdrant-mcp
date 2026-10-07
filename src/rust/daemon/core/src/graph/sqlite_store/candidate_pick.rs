//! The by-name pick behind stub resolution: which real definition(s) a stub
//! name resolves to, each with the confidence of the tier that chose it
//! (docs/plans/2026-06-24-code-graph-resolution-roadmap.md, R1/R2/R2.5/R4/R7):
//!
//! | tier                          | result                 |
//! |-------------------------------|------------------------|
//! | language-server site (R8)     | `[(node, 1.0)]` each   |
//! | receiver-typed member (R7)    | `[(member, 0.97)]` each, plus the tiers below for untyped sites |
//! | own-file definition           | `[(node, 1.0)]`        |
//! | caller's class (R2 scope)     | `[(node, 0.95)]`       |
//! | unique                        | `[(node, 0.7)]`        |
//! | import-anchored (R4)          | `[(node, 0.90)]`       |
//! | same-package (R2.5 proximity) | `[(node, 0.85)]`       |
//! | ambiguous (2..=ceiling)       | `[(c, 1/N)]` KEEP ALL  |
//! | hyper-ambiguous (N > ceiling) | `[]` unresolved        |
//! | library receiver / no match   | `[]` leave it a stub   |
//! | server: into a dependency     | `[]` leave it a stub   |
//!
//! The pool holds only definitions that share a branch with the referring file
//! version: a name defined once per branch is unique on each branch, not
//! ambiguous across all of them, and a definition only another branch has can
//! never be the reference's target.

use std::collections::{HashMap, HashSet};

use sqlx::{Row, SqlitePool};

use super::resolution_tiers::{ReceiverHint, LSP_CONFIDENCE, RECEIVER_CONFIDENCE};
use super::SqliteGraphStore;
use crate::graph::lsp_sites::{LspSite, LspSites};
use crate::graph::{GenerationBranches, GraphDbResult};

/// One real definition a stub name can resolve to. A node unchanged across
/// versions of its file has one row per version, so it is listed once with
/// every generation that defines it.
struct Candidate {
    node_id: String,
    file_path: String,
    symbol_type: String,
    /// The class (struct, impl, …) the definition is a member of.
    parent_symbol: Option<String>,
    /// Each defining generation with the definition's 1-indexed start line
    /// in that version.
    versions: Vec<(String, Option<u32>)>,
}

/// What a stub resolved to.
#[derive(Debug, Default)]
pub(super) struct Picked {
    /// Each target with the confidence of the tier that chose it.
    pub targets: Vec<(String, f64)>,
    /// The language server located the call (see `graph::lsp_sites`).
    pub located: bool,
}

/// The stub reference being resolved.
pub(super) struct StubRef<'q> {
    pub name: &'q str,
    /// File of the version that holds the reference.
    pub own_file: &'q str,
    /// That version's generation.
    pub generation: &'q str,
    /// The referring node's enclosing container (R2 scope).
    pub caller_class: Option<&'q str>,
    pub caller_lang: Option<&'q str>,
    /// Restrict the pool to container kinds (a CONTAINS parent).
    pub container_only: bool,
    /// What the extractor read about the call's receivers.
    pub receiver: Option<&'q ReceiverHint>,
    /// Where the language server said the callee is defined.
    pub lsp: Option<&'q LspSites>,
}

/// Everything the pick consults, loaded once per resolution pass.
pub(super) struct CandidateIndex<'m> {
    by_name: HashMap<String, Vec<Candidate>>,
    /// node_id -> language, stamped at extraction by the dynamic language
    /// registry. Scopes call/type resolution to the caller's own language: a
    /// TypeScript `.filter()` must not repoint onto a Rust `filter` (R3). Only
    /// known, non-empty languages are recorded, so an unclassified node never
    /// causes an over-drop.
    node_lang: HashMap<String, String>,
    /// Names this tenant declares a container (class, struct, …) for: a
    /// receiver of any OTHER type is a library's, not this project's code.
    container_names: HashSet<String>,
    /// R2 scope map: member node_id -> its enclosing container node_id.
    contained_by: HashMap<String, String>,
    /// R4 import map: (source_file, imported_symbol) -> module locators.
    imports_by_symbol: HashMap<(String, String), Vec<String>>,
    /// Beyond this many equally-plausible candidates the 1/N keep-all is noise,
    /// not recall, so the call is left UNRESOLVED. 0 disables it.
    fanout_ceiling: usize,
    membership: &'m GenerationBranches,
}

impl<'m> CandidateIndex<'m> {
    pub(super) async fn load(
        pool: &SqlitePool,
        tenant_id: &str,
        membership: &'m GenerationBranches,
    ) -> GraphDbResult<Self> {
        let (by_name, node_lang, container_names) = load_candidates(pool, tenant_id).await?;
        Ok(Self {
            by_name,
            node_lang,
            container_names,
            contained_by: load_contained_by(pool, tenant_id).await?,
            imports_by_symbol: load_imports(pool, tenant_id).await?,
            fanout_ceiling: std::env::var("WQM_GRAPH_FANOUT_CEILING")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(16),
            membership,
        })
    }

    pub(super) fn language_of(&self, node_id: &str) -> Option<&str> {
        self.node_lang.get(node_id).map(String::as_str)
    }

    pub(super) fn container_of(&self, node_id: &str) -> Option<&str> {
        self.contained_by.get(node_id).map(String::as_str)
    }

    /// Resolve a stub to real definition node(s), each with its confidence.
    pub(super) fn pick_all(&self, stub: &StubRef) -> Picked {
        let in_pool = self.pool_for(stub);
        if let Some(hint) = stub.lsp {
            let located = self.by_lsp_site(stub, &in_pool, &hint.sites);
            // The server's answer decides — no site means it resolved the
            // name into a dependency, nothing of ours — except a site that
            // binds nothing (a file not graphed yet): a call tree-sitter also
            // saw then falls to the tiers below, as if the server had not
            // answered; a call only the server saw has no other evidence.
            if !located.is_empty() || hint.sites.is_empty() || hint.lsp_only {
                return Picked {
                    targets: located,
                    located: true,
                };
            }
        }
        if in_pool.is_empty() {
            return Picked::default();
        }
        let by_name = || {
            let pool: Vec<(&str, &str)> = in_pool
                .iter()
                .map(|c| (c.node_id.as_str(), c.file_path.as_str()))
                .collect();
            self.by_name_tiers(stub, &pool)
        };
        let targets = match stub.receiver {
            None => by_name(),
            Some(hint) => match self.by_receiver(&in_pool, hint) {
                None => by_name(),
                // What the typed sites prove stands; the untyped ones keep
                // their by-name answer beside it.
                Some(decided) if hint.untyped_sites => merge_targets(decided, by_name()),
                Some(decided) => decided,
            },
        };
        Picked {
            targets,
            located: false,
        }
    }

    /// The definitions sitting where the language server said the callee is:
    /// per site, the candidate in that file whose start line (in a version
    /// the reference can see) is nearest the site — a name can repeat in one
    /// file (two classes' `set`, an overload).
    fn by_lsp_site(
        &self,
        stub: &StubRef,
        pool: &[&Candidate],
        sites: &[LspSite],
    ) -> Vec<(String, f64)> {
        let mut located: Vec<(String, f64)> = Vec::new();
        for (file, line) in sites {
            let nearest = pool
                .iter()
                .filter(|c| &c.file_path == file)
                .min_by_key(|c| self.site_distance(stub, c, *line))
                .map(|c| c.node_id.clone());
            if let Some(node_id) = nearest {
                if !located.iter().any(|(n, _)| *n == node_id) {
                    located.push((node_id, LSP_CONFIDENCE));
                }
            }
        }
        located
    }

    /// Lines between a candidate's start (1-indexed) and a site's 0-indexed
    /// line, over the versions the reference can see.
    fn site_distance(&self, stub: &StubRef, candidate: &Candidate, line: u32) -> u64 {
        let site = i64::from(line) + 1;
        candidate
            .versions
            .iter()
            .filter(|(g, _)| self.membership.co_visible(stub.generation, g))
            .map(|(_, start)| start.map_or(u64::MAX, |s| (i64::from(s) - site).unsigned_abs()))
            .min()
            .unwrap_or(u64::MAX)
    }

    /// Definitions of the stub's name the referring version can see, in its
    /// language (dropped only when BOTH languages are known AND differ).
    fn pool_for(&self, stub: &StubRef) -> Vec<&Candidate> {
        let Some(candidates) = self.by_name.get(stub.name) else {
            return Vec::new();
        };
        candidates
            .iter()
            .filter(|c| {
                !stub.container_only || SqliteGraphStore::is_container_node_type(&c.symbol_type)
            })
            .filter(|c| {
                c.versions
                    .iter()
                    .any(|(g, _)| self.membership.co_visible(stub.generation, g))
            })
            .filter(|c| match (stub.caller_lang, self.language_of(&c.node_id)) {
                (Some(cl), Some(tl)) => cl == tl,
                _ => true,
            })
            .collect()
    }

    /// The receiver's type names the callee's class: its member is the target,
    /// wherever the files sit. A variable typed by a class this tenant does not
    /// declare is a library's object (Firestore's `WriteBatch`) — leave the
    /// call unresolved rather than bind it to a same-named method of ours. A
    /// class named as the receiver proves less (`Utils.format()` may name a
    /// namespace or an object), so it never rules our code out. `None` =
    /// undecided: a tenant class without the member (inherited) falls through
    /// to the by-name tiers.
    fn by_receiver(&self, pool: &[&Candidate], hint: &ReceiverHint) -> Option<Vec<(String, f64)>> {
        let named = |p: &str| hint.types.iter().chain(&hint.static_types).any(|t| t == p);
        let members: Vec<(String, f64)> = pool
            .iter()
            .filter(|c| c.parent_symbol.as_deref().is_some_and(named))
            .map(|c| (c.node_id.clone(), RECEIVER_CONFIDENCE))
            .collect();
        if !members.is_empty() {
            return Some(members);
        }
        let library = hint.static_types.is_empty()
            && !hint.types.iter().any(|t| self.container_names.contains(t));
        library.then(Vec::new)
    }

    fn by_name_tiers(&self, stub: &StubRef, pool: &[(&str, &str)]) -> Vec<(String, f64)> {
        // Prefer a definition in the reference's own file (precise).
        if let Some((nid, _)) = pool.iter().find(|(_, fp)| *fp == stub.own_file) {
            return vec![((*nid).to_string(), 1.0)];
        }
        // R2: prefer a candidate in the CALLER's own enclosing class.
        if let Some(cc) = stub.caller_class {
            if let Some((nid, _)) = pool
                .iter()
                .find(|(nid, _)| self.container_of(nid) == Some(cc))
            {
                return vec![((*nid).to_string(), 0.95)];
            }
        }
        if pool.len() == 1 {
            return vec![(pool[0].0.to_string(), 0.7)];
        }
        // CALLS/USES_TYPE only: a CONTAINS parent is never guessed.
        if !stub.container_only {
            if let Some(nid) = self.import_anchored(stub, pool) {
                return vec![(nid.to_string(), 0.9)];
            }
            if let Some(nid) = proximate(stub.own_file, pool) {
                return vec![(nid.to_string(), 0.85)];
            }
        }
        if self.fanout_ceiling > 0 && pool.len() > self.fanout_ceiling {
            return Vec::new();
        }
        // Ambiguous: keep EVERY candidate at confidence 1/N — below the
        // centrality gate (0.6), visible to impact/usages.
        let conf = 1.0 / pool.len() as f64;
        pool.iter()
            .map(|(nid, _)| ((*nid).to_string(), conf))
            .collect()
    }

    /// R4 — the referring file imports THIS name from a module that anchors
    /// EXACTLY ONE candidate's file. A unique anchor only, never a guess.
    fn import_anchored<'p>(&self, stub: &StubRef, pool: &[(&'p str, &str)]) -> Option<&'p str> {
        let modules = self
            .imports_by_symbol
            .get(&(stub.own_file.to_string(), stub.name.to_string()))?;
        let anchored: Vec<&str> = pool
            .iter()
            .filter(|(_, fp)| {
                modules
                    .iter()
                    .any(|m| SqliteGraphStore::import_anchors_file(m, fp))
            })
            .map(|(nid, _)| *nid)
            .collect();
        match anchored.as_slice() {
            [only] => Some(*only),
            _ => None,
        }
    }
}

/// `more` added to `targets`, a node in both keeping its higher confidence.
fn merge_targets(mut targets: Vec<(String, f64)>, more: Vec<(String, f64)>) -> Vec<(String, f64)> {
    for (node_id, confidence) in more {
        match targets.iter_mut().find(|(n, _)| *n == node_id) {
            Some(kept) => kept.1 = kept.1.max(confidence),
            None => targets.push((node_id, confidence)),
        }
    }
    targets
}

/// R2.5 — EXACTLY ONE candidate in the deepest directory prefix shared with
/// the referring file. Repo-root files share depth 0, which leaves the
/// keep-all fan-out untouched when there is no directory signal.
fn proximate<'p>(own_file: &str, pool: &[(&'p str, &str)]) -> Option<&'p str> {
    let max_depth = pool
        .iter()
        .map(|(_, fp)| SqliteGraphStore::shared_dir_depth(own_file, fp))
        .max()
        .unwrap_or(0);
    if max_depth == 0 {
        return None;
    }
    let bucket: Vec<&str> = pool
        .iter()
        .filter(|(_, fp)| SqliteGraphStore::shared_dir_depth(own_file, fp) == max_depth)
        .map(|(nid, _)| *nid)
        .collect();
    match bucket.as_slice() {
        [only] => Some(*only),
        _ => None,
    }
}

type CandidateMaps = (
    HashMap<String, Vec<Candidate>>,
    HashMap<String, String>,
    HashSet<String>,
);

/// Real candidate nodes (resolved file_path, not file-typed), indexed by
/// symbol_name, one entry per node_id carrying every defining generation;
/// plus each node's language and the tenant's container names.
async fn load_candidates(pool: &SqlitePool, tenant_id: &str) -> GraphDbResult<CandidateMaps> {
    let rows = sqlx::query(
        "SELECT node_id, generation, symbol_name, file_path, symbol_type, language,
                parent_symbol, start_line
         FROM graph_nodes
         WHERE tenant_id = ?1 AND file_path <> '' AND symbol_type <> 'file'",
    )
    .bind(tenant_id)
    .fetch_all(pool)
    .await?;
    let mut by_name: HashMap<String, Vec<Candidate>> = HashMap::new();
    let mut node_lang: HashMap<String, String> = HashMap::new();
    let mut container_names: HashSet<String> = HashSet::new();
    for r in &rows {
        let name: String = r.get("symbol_name");
        let nid: String = r.get("node_id");
        let symbol_type: String = r.get("symbol_type");
        if let Some(l) = r
            .get::<Option<String>, _>("language")
            .filter(|l| !l.is_empty())
        {
            node_lang.insert(nid.clone(), l);
        }
        if SqliteGraphStore::is_container_node_type(&symbol_type) {
            container_names.insert(name.clone());
        }
        let version = (
            r.get::<String, _>("generation"),
            r.get::<Option<i64>, _>("start_line")
                .and_then(|l| u32::try_from(l).ok()),
        );
        let entries = by_name.entry(name).or_default();
        match entries.iter_mut().find(|c| c.node_id == nid) {
            Some(c) => c.versions.push(version),
            None => entries.push(Candidate {
                node_id: nid,
                file_path: r.get("file_path"),
                symbol_type,
                parent_symbol: r.get("parent_symbol"),
                versions: vec![version],
            }),
        }
    }
    Ok((by_name, node_lang, container_names))
}

/// Member node_id -> its enclosing container node_id, from CONTAINS edges.
/// Lets the resolver prefer a same-named callee defined in the CALLER's own
/// class over a tenant-wide collision (R2).
async fn load_contained_by(
    pool: &SqlitePool,
    tenant_id: &str,
) -> GraphDbResult<HashMap<String, String>> {
    Ok(sqlx::query(
        "SELECT source_node_id AS class_id, target_node_id AS member_id
         FROM graph_edges
         WHERE tenant_id = ?1 AND edge_type = 'CONTAINS'",
    )
    .bind(tenant_id)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|r| {
        (
            r.get::<String, _>("member_id"),
            r.get::<String, _>("class_id"),
        )
    })
    .collect())
}

/// (source_file, imported_symbol) -> module locators, read from IMPORTS edge
/// metadata (`{"module":"…"}` stamped at extraction). A call to an imported
/// name anchors to the definition file the caller actually imported — the
/// precise cross-package tier above proximity (R4).
async fn load_imports(
    pool: &SqlitePool,
    tenant_id: &str,
) -> GraphDbResult<HashMap<(String, String), Vec<String>>> {
    let rows = sqlx::query(
        "SELECT DISTINCT e.source_file AS sf, n.symbol_name AS sym, e.metadata_json AS mj
         FROM graph_edges e JOIN graph_nodes n ON e.target_node_id = n.node_id
         WHERE e.tenant_id = ?1 AND e.edge_type = 'IMPORTS'
           AND e.metadata_json LIKE '%\"module\"%'",
    )
    .bind(tenant_id)
    .fetch_all(pool)
    .await?;
    let mut imports: HashMap<(String, String), Vec<String>> = HashMap::new();
    for r in &rows {
        let mj: String = r.get("mj");
        if let Some(module) = SqliteGraphStore::extract_module_field(&mj) {
            let modules = imports.entry((r.get("sf"), r.get("sym"))).or_default();
            if !modules.contains(&module) {
                modules.push(module);
            }
        }
    }
    Ok(imports)
}
