//! Where the language server says a called name is defined, carried on the
//! CALLS stub edge from the ingest pass that asked it to the resolver that
//! binds the stub (`sqlite_store::candidate_pick`).
//!
//! The server reports a callee's NAME, FILE and LINE, never the node it is: a
//! method's id includes its class (`compute_member_node_id`), and a free
//! function's does not. Guessing the id at ingest minted a `Function` node
//! for every method callee — a node no definition has, so the precise edge
//! hung off a phantom. The resolver instead binds the site to the definition
//! that actually sits there, once every file of the tenant has been parsed.

use serde_json::{Map, Value};

/// One definition site: project-relative file and 0-indexed line.
pub type LspSite = (String, u32);

const SITES_KEY: &str = "lsp_sites";
const ONLY_KEY: &str = "lsp_only";

/// The definition sites one CALLS stub carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspSites {
    /// The in-project sites. Empty: the server resolved every call by this
    /// name into a dependency or the SDK — nothing of ours is the target.
    pub sites: Vec<LspSite>,
    /// Tree-sitter saw no call by this name (the server names a constructor
    /// `__init__` where the source says `Foo()`): when no site binds, the stub
    /// stays unresolved instead of falling back to a guess by name.
    pub lsp_only: bool,
}

/// `metadata` (a CALLS edge's JSON, possibly carrying receiver types) with
/// the server's definition `sites` added.
pub fn with_lsp_sites(metadata: Option<&str>, sites: &[LspSite], lsp_only: bool) -> String {
    let mut object = metadata
        .and_then(|m| serde_json::from_str::<Value>(m).ok())
        .and_then(|v| match v {
            Value::Object(o) => Some(o),
            _ => None,
        })
        .unwrap_or_else(Map::new);
    object.insert(SITES_KEY.to_string(), serde_json::json!(sites));
    if lsp_only {
        object.insert(ONLY_KEY.to_string(), Value::Bool(true));
    }
    Value::Object(object).to_string()
}

/// The sites a CALLS stub carries; `None` when the server was not asked.
pub fn lsp_sites(metadata: Option<&str>) -> Option<LspSites> {
    let value: Value = serde_json::from_str(metadata?).ok()?;
    let sites: Vec<LspSite> = value
        .get(SITES_KEY)?
        .as_array()?
        .iter()
        .filter_map(|site| {
            let pair = site.as_array()?;
            let file = pair.first()?.as_str()?.to_string();
            let line = u32::try_from(pair.get(1)?.as_u64()?).ok()?;
            Some((file, line))
        })
        .collect();
    let lsp_only = value.get(ONLY_KEY).and_then(Value::as_bool) == Some(true);
    Some(LspSites { sites, lsp_only })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sites_round_trip_beside_the_receiver_types() {
        let meta = with_lsp_sites(
            Some(r#"{"receiver_types":["Batch"]}"#),
            &[("lib/writes.dart".to_string(), 41)],
            false,
        );
        let value: Value = serde_json::from_str(&meta).unwrap();
        assert_eq!(value["receiver_types"][0], "Batch", "{meta}");
        assert_eq!(
            lsp_sites(Some(&meta)),
            Some(LspSites {
                sites: vec![("lib/writes.dart".to_string(), 41)],
                lsp_only: false,
            })
        );
    }

    #[test]
    fn a_call_tree_sitter_never_saw_is_marked_lsp_only() {
        let meta = with_lsp_sites(None, &[("app/models.py".to_string(), 7)], true);
        assert!(lsp_sites(Some(&meta)).unwrap().lsp_only);
    }

    #[test]
    fn a_call_resolved_into_a_dependency_carries_no_site() {
        let meta = with_lsp_sites(None, &[], false);
        assert_eq!(
            lsp_sites(Some(&meta)),
            Some(LspSites {
                sites: Vec::new(),
                lsp_only: false,
            })
        );
    }

    #[test]
    fn metadata_the_server_never_touched_carries_none() {
        assert_eq!(lsp_sites(Some(r#"{"receiver_types":["X"]}"#)), None);
        assert_eq!(lsp_sites(Some("not json")), None);
        assert_eq!(lsp_sites(None), None);
    }
}
