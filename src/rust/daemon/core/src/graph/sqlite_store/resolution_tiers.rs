//! The tiers a resolved stub edge can land in, and the receiver hint that adds
//! the most precise one.

/// A call whose receiver's declared type names the callee's class.
pub(super) const RECEIVER_CONFIDENCE: f64 = 0.97;

/// A call the language server located: the definition at the site it named.
pub(super) const LSP_CONFIDENCE: f64 = 1.0;

/// What the extractor read about a call's receivers, stamped on its CALLS
/// stub edge (see `graph::extractor::receivers::hint_metadata`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ReceiverHint {
    /// Types of receivers that are declared or constructed variables.
    pub types: Vec<String>,
    /// Classes named as the receiver itself (`Type.member(`).
    pub static_types: Vec<String>,
    /// Some call site had no typed receiver: those keep the by-name tiers.
    pub untyped_sites: bool,
}

/// The receiver hint on a CALLS stub edge; `None` when no site was typed.
pub(super) fn receiver_hint(metadata_json: Option<&str>) -> Option<ReceiverHint> {
    let value: serde_json::Value = serde_json::from_str(metadata_json?).ok()?;
    let names = |key: &str| -> Vec<String> {
        value
            .get(key)
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|t| t.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let hint = ReceiverHint {
        types: names("receiver_types"),
        static_types: names("static_receivers"),
        untyped_sites: value
            .get("untyped_sites")
            .and_then(serde_json::Value::as_bool)
            == Some(true),
    };
    (!hint.types.is_empty() || !hint.static_types.is_empty()).then_some(hint)
}

/// Compact resolution provenance stamped onto each repointed edge: the tier its
/// confidence implies (or `lsp` when the language server `located` the
/// call), and how many candidates the name had.
pub(super) fn resolution_metadata(confidence: f64, candidates: usize, located: bool) -> String {
    let tier = if located {
        "lsp"
    } else if confidence >= 0.99 {
        "in_file"
    } else if confidence >= RECEIVER_CONFIDENCE {
        "receiver"
    } else if confidence >= 0.93 {
        "scoped"
    } else if confidence >= 0.88 {
        "import"
    } else if confidence >= 0.8 {
        "proximity"
    } else if candidates == 1 {
        "tenant_unique"
    } else {
        "ambiguous"
    };
    format!(
        "{{\"resolution\":\"{}\",\"confidence\":{:.4},\"candidates\":{}}}",
        tier, confidence, candidates
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_receiver_hint_reads_back_and_lands_in_its_own_tier() {
        assert_eq!(
            receiver_hint(Some(r#"{"receiver_types":["FirestoreFinanceBatch"]}"#)),
            Some(ReceiverHint {
                types: vec!["FirestoreFinanceBatch".to_string()],
                ..ReceiverHint::default()
            })
        );
        assert_eq!(
            receiver_hint(Some(
                r#"{"static_receivers":["CashFlowBackup"],"untyped_sites":true}"#
            )),
            Some(ReceiverHint {
                static_types: vec!["CashFlowBackup".to_string()],
                untyped_sites: true,
                ..ReceiverHint::default()
            })
        );
        assert_eq!(receiver_hint(Some(r#"{"untyped_sites":true}"#)), None);
        assert_eq!(receiver_hint(Some(r#"{"module":"x"}"#)), None);
        assert_eq!(receiver_hint(None), None);
        assert!(resolution_metadata(RECEIVER_CONFIDENCE, 1, false).contains("\"receiver\""));
        assert!(resolution_metadata(0.95, 1, false).contains("\"scoped\""));
        assert!(resolution_metadata(0.5, 2, false).contains("\"ambiguous\""));
        assert!(resolution_metadata(1.0, 1, false).contains("\"in_file\""));
        assert!(resolution_metadata(LSP_CONFIDENCE, 1, true).contains("\"lsp\""));
    }
}
