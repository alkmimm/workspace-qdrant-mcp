//! Orphan-tenant purge executed under `AdminWriteService.PurgeTenant`.
//!
//! Removes ALL state for a tenant that is no longer backed by a live checkout:
//! its Qdrant points across the 4 canonical collections and its rows in
//! `watch_folders` / `unified_queue` / `tracked_files`. Guarded so a tenant
//! whose watch root still exists on disk (a live project) is never purged —
//! that would re-create the data loss of issue #299 from the other direction.
//!
//! Lives in the service layer (not the WriteActor) because it needs Qdrant
//! access via `ReembedContext.storage_client`; the WriteActor is SQLite-only.

use std::path::Path;

use wqm_common::constants::CANONICAL_COLLECTIONS;

use crate::services::reembed::ReembedContext;

/// Result of a purge (or a dry-run preview).
pub struct PurgeOutcome {
    pub dry_run: bool,
    pub sqlite_rows_deleted: u32,
    pub qdrant_points_deleted: u64,
    pub message: String,
}

/// Return the first registered watch root that still exists on disk, if any.
/// A present root means the tenant is a live project, not an orphan — pure so
/// it is unit-testable without a database or Qdrant.
fn find_live_root(roots: &[String]) -> Option<&str> {
    roots
        .iter()
        .find(|r| Path::new(r.as_str()).exists())
        .map(String::as_str)
}

/// Execute (or preview) a purge of `tenant_id`.
///
/// - Refuses if any of the tenant's `watch_folders` roots still exists on disk.
/// - `dry_run`: report the Qdrant point counts that would be deleted; no writes.
/// - Otherwise `confirm` must be true; deletes Qdrant points per collection then
///   the tenant's SQLite rows in one transaction.
pub async fn execute_purge(
    ctx: &ReembedContext,
    tenant_id: &str,
    dry_run: bool,
    confirm: bool,
) -> Result<PurgeOutcome, String> {
    let tenant_id = tenant_id.trim();
    if tenant_id.is_empty() {
        return Err("tenant_id must not be empty".to_string());
    }

    // ── Guard: never purge a tenant that still has a live checkout ──────────
    let roots: Vec<String> =
        sqlx::query_scalar::<_, String>("SELECT path FROM watch_folders WHERE tenant_id = ?1")
            .bind(tenant_id)
            .fetch_all(&ctx.pool)
            .await
            .map_err(|e| format!("database error: {}", e))?;

    if let Some(live) = find_live_root(&roots) {
        return Err(format!(
            "refusing to purge tenant '{}': watch root still exists on disk: {} \
             (this looks like a live project, not an orphan)",
            tenant_id, live
        ));
    }

    // ── Count Qdrant points per collection (report) ─────────────────────────
    let mut qdrant_total: u64 = 0;
    let mut per_coll: Vec<String> = Vec::new();
    for &coll in CANONICAL_COLLECTIONS {
        let n = ctx
            .storage_client
            .count_points(coll, Some(tenant_id))
            .await
            .map_err(|e| format!("qdrant count failed for '{}': {}", coll, e))?;
        qdrant_total += n;
        per_coll.push(format!("{}={}", coll, n));
    }

    if dry_run {
        return Ok(PurgeOutcome {
            dry_run: true,
            sqlite_rows_deleted: 0,
            qdrant_points_deleted: qdrant_total,
            message: format!(
                "DRY RUN — would purge tenant '{}': {} Qdrant point(s) [{}], \
                 {} watch-folder root(s) (all missing on disk)",
                tenant_id,
                qdrant_total,
                per_coll.join(", "),
                roots.len(),
            ),
        });
    }

    if !confirm {
        return Err(
            "PurgeTenant requires confirm=true to mutate (set dry_run=true to preview)".to_string(),
        );
    }

    // ── Delete Qdrant points across the 4 canonical collections ─────────────
    let mut deleted_points: u64 = 0;
    for &coll in CANONICAL_COLLECTIONS {
        let n = ctx
            .storage_client
            .delete_points_by_tenant(coll, tenant_id)
            .await
            .map_err(|e| format!("qdrant delete failed for '{}': {}", coll, e))?;
        deleted_points += n;
    }

    // ── Delete SQLite rows in one transaction ───────────────────────────────
    let mut tx = ctx
        .pool
        .begin()
        .await
        .map_err(|e| format!("transaction error: {}", e))?;

    // Defer FK enforcement to commit time. `tracked_files` has a FK to
    // `watch_folders` with NO ON DELETE CASCADE, so a naive parent-first delete
    // fails (code 787). Deferring checks the constraint once, at commit, after
    // ALL of the tenant's rows are gone — robust to delete order and to any
    // other non-cascade FK into the tenant's tables.
    sqlx::query("PRAGMA defer_foreign_keys = ON")
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("database error (defer_foreign_keys): {}", e))?;

    let mut rows: u32 = 0;

    // tracked_files (child of watch_folders) first. It may lack a tenant_id
    // column in older schema versions — tolerate that.
    match sqlx::query("DELETE FROM tracked_files WHERE tenant_id = ?1")
        .bind(tenant_id)
        .execute(&mut *tx)
        .await
    {
        Ok(r) => rows += r.rows_affected() as u32,
        Err(e) => {
            let msg = e.to_string();
            if !(msg.contains("no such column") || msg.contains("has no column named")) {
                return Err(format!("database error (tracked_files): {}", e));
            }
        }
    }

    // project_components also has a NO-ACTION FK to watch_folders, keyed by
    // watch_folder_id (not tenant_id) — delete the tenant's components before
    // its watch_folders. Tolerate the table not existing in older schemas.
    match sqlx::query(
        "DELETE FROM project_components WHERE watch_folder_id IN \
         (SELECT watch_id FROM watch_folders WHERE tenant_id = ?1)",
    )
    .bind(tenant_id)
    .execute(&mut *tx)
    .await
    {
        Ok(r) => rows += r.rows_affected() as u32,
        Err(e) => {
            let msg = e.to_string();
            if !(msg.contains("no such table") || msg.contains("no such column")) {
                return Err(format!("database error (project_components): {}", e));
            }
        }
    }

    rows += sqlx::query("DELETE FROM unified_queue WHERE tenant_id = ?1")
        .bind(tenant_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("database error (unified_queue): {}", e))?
        .rows_affected() as u32;

    // watch_folders (parent) last.
    rows += sqlx::query("DELETE FROM watch_folders WHERE tenant_id = ?1")
        .bind(tenant_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("database error (watch_folders): {}", e))?
        .rows_affected() as u32;

    tx.commit()
        .await
        .map_err(|e| format!("commit error: {}", e))?;

    Ok(PurgeOutcome {
        dry_run: false,
        sqlite_rows_deleted: rows,
        qdrant_points_deleted: deleted_points,
        message: format!(
            "Purged tenant '{}': {} Qdrant point(s), {} SQLite row(s)",
            tenant_id, deleted_points, rows
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::find_live_root;

    #[test]
    fn find_live_root_flags_an_existing_path_and_ignores_missing_ones() {
        // temp_dir() is a real directory that exists; the joined child does not.
        let existing = std::env::temp_dir().to_string_lossy().to_string();
        let missing = std::env::temp_dir()
            .join("wqm-purge-does-not-exist-xyz")
            .to_string_lossy()
            .to_string();

        assert_eq!(find_live_root(&[]), None);
        assert_eq!(find_live_root(&[missing.clone()]), None);
        assert_eq!(
            find_live_root(&[missing, existing.clone()]).map(str::to_string),
            Some(existing)
        );
    }
}
