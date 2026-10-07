//! The tiers a resolved stub edge can land in, and the receiver hint that adds
//! the most precise one.

/// A call whose receiver's declared type names the callee's class.
pub(super) const RECEIVER_CONFIDENCE: f64 = 0.97;

/// The receiver types the extractor stamped on a CALLS stub edge
/// (`{"receiver_types": [...]}`, see `graph::extractor::receivers`).
pub(super) fn receiver_types(metadata_json: Option<&str>) -> Option<Vec<String>> {
    let value: serde_json::Value = serde_json::from_str(metadata_json?).ok()?;
    let types: Vec<String> = value
        .get("receiver_types")?
        .as_array()?
        .iter()
        .filter_map(|t| t.as_str().map(str::to_string))
        .collect();
    (!types.is_empty()).then_some(types)
}

/// Compact resolution provenance stamped onto each repointed edge: the tier its
/// confidence implies, and how many candidates the name had.
pub(super) fn resolution_metadata(confidence: f64, candidates: usize) -> String {
    let tier = if confidence >= 0.99 {
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
            receiver_types(Some(r#"{"receiver_types":["FirestoreFinanceBatch"]}"#)),
            Some(vec!["FirestoreFinanceBatch".to_string()])
        );
        assert_eq!(receiver_types(Some(r#"{"module":"x"}"#)), None);
        assert_eq!(receiver_types(None), None);
        assert!(resolution_metadata(RECEIVER_CONFIDENCE, 1).contains("\"receiver\""));
        assert!(resolution_metadata(0.95, 1).contains("\"scoped\""));
        assert!(resolution_metadata(0.5, 2).contains("\"ambiguous\""));
    }
}
