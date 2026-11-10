use super::*;

#[test]
fn test_build_dequeue_query_high_priority_pass_has_op_desc() {
    // High-priority pass: op=DESC means delete(10) > add(5) > scan(1)
    let q = build_dequeue_query(None, None, "DESC", "DESC", "ASC");
    assert!(
        q.contains("END DESC"),
        "high-priority pass should have op_order=DESC"
    );
    assert!(
        q.contains("q.created_at ASC"),
        "high-priority pass should have FIFO created_at order"
    );
}

#[test]
fn test_build_dequeue_query_low_priority_pass_has_op_desc() {
    // Low-priority pass: op=DESC even on the anti-starvation pass because delete
    // must always take precedence regardless of which priority direction is active.
    let q = build_dequeue_query(None, None, "ASC", "DESC", "DESC");
    assert!(
        q.contains("END DESC"),
        "low-priority pass should still have op_order=DESC (delete always first)"
    );
    assert!(
        q.contains("q.created_at DESC"),
        "low-priority pass should have LIFO created_at order"
    );
}

#[test]
fn test_build_dequeue_query_tenant_filter() {
    let q = build_dequeue_query(Some("t1"), None, "DESC", "DESC", "ASC");
    assert!(q.contains("tenant_id = ?2"), "should include tenant filter");
}

#[test]
fn test_build_dequeue_query_item_type_filter() {
    let q = build_dequeue_query(None, Some(ItemType::File), "DESC", "DESC", "ASC");
    assert!(
        q.contains("item_type = ?2"),
        "should include item_type filter"
    );
}

#[test]
fn test_build_dequeue_query_both_filters() {
    let q = build_dequeue_query(Some("t1"), Some(ItemType::Folder), "DESC", "DESC", "ASC");
    assert!(q.contains("tenant_id = ?2"), "should include tenant filter");
    assert!(
        q.contains("item_type = ?3"),
        "should include item_type filter"
    );
    // With age-promotion thresholds added: ?4 = warning, ?5 = critical, ?6 = limit,
    // ?7 = the candidate id list.
    assert!(
        q.contains("LIMIT ?6"),
        "limit should be ?6 with both filters + age thresholds"
    );
    assert!(
        q.contains("json_each(?7)"),
        "candidates should be the last parameter"
    );
}

#[test]
fn test_build_dequeue_query_ranks_only_candidates() {
    // The ranking must be bounded by the candidate list in every filter shape:
    // without it the query sorts the whole queue under the write lock again.
    for (tenant, itype, param) in [
        (None, None, "?5"),
        (Some("t1"), None, "?6"),
        (None, Some(ItemType::File), "?6"),
        (Some("t1"), Some(ItemType::File), "?7"),
    ] {
        let q = build_dequeue_query(tenant, itype, "DESC", "DESC", "ASC");
        assert!(
            q.contains(&format!(
                "q.queue_id IN (SELECT value FROM json_each({}))",
                param
            )),
            "candidate filter must bind {} for tenant={:?} item_type={:?}",
            param,
            tenant,
            itype
        );
    }
}

#[test]
fn test_build_dequeue_query_includes_age_promotion_case() {
    // The age-promotion CASE must appear in the ORDER BY and must reference
    // both threshold placeholders.
    let q = build_dequeue_query(None, None, "DESC", "DESC", "ASC");
    assert!(
        q.contains("strftime('%s','now')"),
        "should compute item age in seconds"
    );
    assert!(
        q.contains("strftime('%s', q.created_at)"),
        "should subtract created_at to get age"
    );
    // No filters → warning=?2, critical=?3, limit=?4
    assert!(
        q.contains(">= ?3 THEN 2"),
        "critical threshold (?3) bumps priority to +2"
    );
    assert!(
        q.contains(">= ?2 THEN 1"),
        "warning threshold (?2) bumps priority to +1"
    );
}

#[test]
fn test_build_dequeue_query_age_promotion_respects_priority_order_flip() {
    // The age-promotion CASE must use {priority_order} like the active CASE,
    // so the DESC/ASC fairness flip inverts both consistently.
    let q_desc = build_dequeue_query(None, None, "DESC", "DESC", "ASC");
    let q_asc = build_dequeue_query(None, None, "ASC", "DESC", "DESC");

    // DESC pass: age-promotion ends with DESC (older = higher rank)
    let desc_age_section = q_desc
        .split(">= ?2 THEN 1")
        .nth(1)
        .expect("split should yield section after warning case");
    assert!(
        desc_age_section.trim_start().starts_with("ELSE 0\n"),
        "DESC pass: age CASE should be followed by ELSE 0 then END DESC"
    );

    let asc_age_section = q_asc
        .split(">= ?2 THEN 1")
        .nth(1)
        .expect("split should yield section after warning case");
    assert!(
        asc_age_section.trim_start().starts_with("ELSE 0\n"),
        "ASC pass: age CASE should be followed by ELSE 0 then END ASC"
    );

    // Sanity: priority_order shows up multiple times (active CASE + age CASE).
    assert!(
        q_desc.matches("END DESC").count() >= 3,
        "DESC pass should have at least 3 END DESC (delete, tenant/add, active CASE, age CASE, op CASE)"
    );
}

#[test]
fn test_build_dequeue_query_age_case_placed_before_active_case() {
    // The age-promotion CASE must appear BEFORE the active-vs-inactive CASE
    // so that age can override the active ranking once thresholds are crossed.
    let q = build_dequeue_query(None, None, "DESC", "DESC", "ASC");
    let age_idx = q
        .find(">= ?3 THEN 2")
        .expect("age-promotion CASE should be present");
    let active_idx = q
        .find("w.is_active > 0")
        .expect("active CASE should be present");
    assert!(
        age_idx < active_idx,
        "age-promotion CASE must come before active-vs-inactive CASE"
    );

    // And both must come AFTER the delete CASE.
    let delete_idx = q
        .find("WHEN q.op = 'delete' THEN 1")
        .expect("delete CASE should be present");
    assert!(
        delete_idx < age_idx,
        "delete CASE must take precedence over age promotion"
    );
}
