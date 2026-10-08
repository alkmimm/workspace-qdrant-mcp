//! Progressive single-level directory scan logic.

use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use chrono::DateTime;
use tracing::{debug, warn};
use wqm_common::paths::{CanonicalPath, RelativePath};

use crate::allowed_extensions::AllowedExtensions;
use crate::patterns::exclusion::{is_reincluded_dir, should_exclude_directory};
use crate::patterns::global_ignore;
use crate::patterns::ignore_gate::IgnoreGate;
use crate::queue_operations::QueueManager;
use crate::unified_queue_processor::{UnifiedProcessorError, UnifiedProcessorResult};
use crate::unified_queue_schema::{
    FolderPayload, ItemType, ProjectPayload, QueueOperation, UnifiedQueueItem,
};

use super::file_entry::process_file_entry;

/// Progressive single-level directory scan with mtime-based pruning.
///
/// Enumerates only the immediate children of `dir_path`:
/// - Files: check exclusion + allowlist + mtime, enqueue `(File, Add)`
/// - Directories: check exclusion + mtime, enqueue `(Folder, Scan)`
/// - Directories with `.git`: submodule detection, enqueue `(Tenant, Add)`
///
/// `last_scan` is an ISO 8601 timestamp string. Entries with mtime <= this
/// value are skipped -- they are unchanged since the previous scan. Pass
/// `None` for a full scan (first-time or forced rescan).
///
/// `uplift` makes discovered files enqueue as `File/Uplift` instead of
/// `File/Add` (forced re-processing, see `FolderPayload::uplift`); it is
/// inherited by the subdirectory scans this walk spawns.
///
/// Returns `(files_queued, dirs_queued, files_excluded, errors)`.
pub(crate) async fn scan_directory_single_level(
    dir_path: &Path,
    watch_folder_root: &CanonicalPath,
    item: &UnifiedQueueItem,
    queue_manager: &Arc<QueueManager>,
    allowed_extensions: &Arc<AllowedExtensions>,
    last_scan: Option<&str>,
    uplift: bool,
) -> UnifiedProcessorResult<(u64, u64, u64, u64)> {
    let mut files_queued = 0u64;
    let mut dirs_queued = 0u64;
    let mut files_excluded = 0u64;
    let mut errors = 0u64;

    let baseline: Option<SystemTime> = last_scan.and_then(parse_iso8601_to_system_time);
    let Some(gate) = scan_gate(dir_path, Path::new(watch_folder_root.as_str())) else {
        return Ok((0, 0, 1, 0));
    };

    let entries = std::fs::read_dir(dir_path).map_err(|e| {
        UnifiedProcessorError::ProcessingFailed(format!(
            "Failed to read directory {}: {}",
            dir_path.display(),
            e
        ))
    })?;

    for entry in entries {
        let Some((entry, file_type)) = readable_entry(entry, dir_path, &mut errors) else {
            continue;
        };
        let path = entry.path();

        if file_type.is_dir() {
            if gate.is_ignored(&path, true) {
                files_excluded += 1;
                continue;
            }
            dirs_queued += process_directory_entry(
                &path,
                &entry.file_name().to_string_lossy().to_string(),
                watch_folder_root,
                item,
                queue_manager,
                last_scan,
                uplift,
                &mut errors,
            )
            .await;
        } else if file_type.is_file() {
            if gate.is_ignored(&path, false) {
                files_excluded += 1;
                continue;
            }
            files_queued += process_file_entry(
                &path,
                watch_folder_root,
                item,
                queue_manager,
                allowed_extensions,
                baseline.as_ref(),
                uplift,
                &mut files_excluded,
                &mut errors,
            )
            .await;
        }
        // Symlinks are skipped (no follow)
    }

    Ok((files_queued, dirs_queued, files_excluded, errors))
}

/// A readable directory entry with its type, or `None` — counted in `errors`
/// — when the entry or its type cannot be read.
fn readable_entry(
    entry: std::io::Result<std::fs::DirEntry>,
    dir_path: &Path,
    errors: &mut u64,
) -> Option<(std::fs::DirEntry, std::fs::FileType)> {
    let entry = match entry {
        Ok(e) => e,
        Err(e) => {
            warn!("Failed to read dir entry in {}: {}", dir_path.display(), e);
            *errors += 1;
            return None;
        }
    };
    match entry.file_type() {
        Ok(file_type) => Some((entry, file_type)),
        Err(e) => {
            warn!(
                "Failed to get file type for {}: {}",
                entry.path().display(),
                e
            );
            *errors += 1;
            None
        }
    }
}

/// The ignore gate for scanning `dir_path`, or `None` when the directory
/// itself lies under an ignored directory.
///
/// One gate for the project `.gitignore`/`.wqmignore` cascade AND the
/// daemon-wide `global.wqmignore`. The watch-folder root makes ignore rules
/// cascade from ancestor dirs (issue #49) — passing `None` used only the
/// scanned subdirectory's own files, so a project-root `.wqmignore` was missed
/// when scanning a subdirectory. `IgnoreGate` is the same decision the
/// reconciler uses, so the two walk paths can never disagree on eligibility.
///
/// A scan queued before its directory became ignored — a `.wqmignore` added
/// while a project's first scan was still descending — must not keep crawling
/// the excluded subtree. The per-entry checks only see the directory's
/// children, so a rule on an ANCESTOR (`/*` at the root) never reached them:
/// every file was enqueued and then dropped by the dequeue gate, and every
/// subdirectory spawned another scan (tecsul 2026-10-07: ~230 queued FreeRTOS
/// scans kept crawling an excluded `c/`).
fn scan_gate(dir_path: &Path, root: &Path) -> Option<IgnoreGate> {
    let gate = IgnoreGate::for_dir(
        dir_path,
        Some(root),
        global_ignore::resolve_global_ignore_path().as_deref(),
    );
    if gate.is_dir_ignored_with_ancestors(root, dir_path) {
        debug!(
            "Folder scan skipped: {} lies under an ignored directory",
            dir_path.display()
        );
        return None;
    }
    Some(gate)
}

/// Parse an ISO 8601 / RFC 3339 timestamp string into a `SystemTime`.
/// Returns `None` on parse failure (safe fallback: scan everything).
pub(crate) fn parse_iso8601_to_system_time(s: &str) -> Option<SystemTime> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| SystemTime::from(dt))
}

/// Process a single directory entry encountered during scan.
///
/// `last_scan` and `uplift` are propagated into the child `FolderPayload`
/// so that mtime pruning / forced re-processing continue through the
/// entire directory tree.
///
/// Returns 1 if an item was enqueued, 0 otherwise.
#[allow(clippy::too_many_arguments)]
async fn process_directory_entry(
    path: &Path,
    dir_name: &str,
    watch_folder_root: &CanonicalPath,
    item: &UnifiedQueueItem,
    queue_manager: &Arc<QueueManager>,
    last_scan: Option<&str>,
    uplift: bool,
    errors: &mut u64,
) -> u64 {
    // Check directory exclusion. A directory `global.wqmignore` explicitly
    // re-includes (e.g. `!**/src/**/out/` for a hexagonal `ports/out`) is
    // walked even though its bare name is a build-output token — the same
    // override `should_exclude_file_in` applies to the files inside it.
    if should_exclude_directory(dir_name)
        && !is_reincluded_dir(Path::new(watch_folder_root.as_str()), path)
    {
        return 0;
    }

    // Submodule detection: directory with .git -> (Tenant, Add)
    if path.join(".git").exists() {
        return enqueue_submodule(path, item, queue_manager, errors).await;
    }

    // Regular subdirectory -> (Folder, Scan)
    enqueue_subdirectory(
        path,
        watch_folder_root,
        item,
        queue_manager,
        last_scan,
        uplift,
    )
    .await
}

/// Enqueue a submodule directory as a Tenant/Add item.
///
/// Returns 1 if enqueued successfully, 0 otherwise.
pub(crate) async fn enqueue_submodule(
    path: &Path,
    item: &UnifiedQueueItem,
    queue_manager: &Arc<QueueManager>,
    errors: &mut u64,
) -> u64 {
    let submodule_payload = ProjectPayload {
        project_root: path.to_string_lossy().to_string(),
        git_remote: None,
        project_type: None,
        old_tenant_id: None,
        is_active: None,
        branch_membership: None,
    };
    let payload_json = serde_json::to_string(&submodule_payload)
        .unwrap_or_else(|_| format!(r#"{{"project_root":"{}"}}"#, path.display()));

    let submodule_tenant = wqm_common::project_id::calculate_tenant_id(path);

    match queue_manager
        .enqueue_unified(
            ItemType::Tenant,
            QueueOperation::Add,
            &submodule_tenant,
            &item.collection,
            &payload_json,
            None,
            None,
        )
        .await
    {
        Ok((_, true)) => {
            debug!("Enqueued submodule as Tenant/Add: {}", path.display());
            1
        }
        Ok((_, false)) => 0,
        Err(e) => {
            warn!("Failed to enqueue submodule {}: {}", path.display(), e);
            *errors += 1;
            0
        }
    }
}

/// Enqueue a regular subdirectory as a Folder/Scan or Folder/Uplift item.
///
/// `last_scan` is embedded in the payload so the child scan can prune
/// unchanged entries without an extra DB query.
///
/// Returns 1 if enqueued successfully, 0 otherwise.
async fn enqueue_subdirectory(
    path: &Path,
    watch_folder_root: &CanonicalPath,
    item: &UnifiedQueueItem,
    queue_manager: &Arc<QueueManager>,
    last_scan: Option<&str>,
    uplift: bool,
) -> u64 {
    // Build a CanonicalPath for the absolute subdir so we can derive the
    // relative form. If the path is not UTF-8 or contains `..` we skip
    // enqueueing rather than store an invalid payload.
    let abs_str = match path.to_str() {
        Some(s) => s,
        None => {
            warn!("Non-UTF-8 subdir path skipped: {}", path.display());
            return 0;
        }
    };
    let abs = match CanonicalPath::from_user_input(abs_str) {
        Ok(p) => p,
        Err(e) => {
            warn!("Subdir path failed canonicalization ({}): {}", abs_str, e);
            return 0;
        }
    };
    let relative = match RelativePath::from_absolute_and_root(&abs, watch_folder_root) {
        Ok(r) => r,
        Err(e) => {
            warn!(
                "Subdir {} is not under watch_folder root {} ({}); skipping",
                abs.as_str(),
                watch_folder_root.as_str(),
                e
            );
            return 0;
        }
    };

    let folder_payload = FolderPayload {
        folder_path: Some(relative),
        recursive: false,
        recursive_depth: 0,
        patterns: vec![],
        ignore_patterns: vec![],
        old_path: None,
        last_scan: last_scan.map(|s| s.to_string()),
        uplift,
    };
    let payload_json = match serde_json::to_string(&folder_payload) {
        Ok(s) => s,
        Err(e) => {
            warn!(
                "Failed to serialize FolderPayload for {}: {}",
                path.display(),
                e
            );
            return 0;
        }
    };

    let op = if uplift {
        QueueOperation::Uplift
    } else {
        QueueOperation::Scan
    };

    match queue_manager
        .enqueue_unified(
            ItemType::Folder,
            op,
            &item.tenant_id,
            &item.collection,
            &payload_json,
            Some(&item.branch),
            None,
        )
        .await
    {
        Ok((_, true)) => 1,
        _ => 0,
    }
}
#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(crate) fn scan_item(tenant: &str) -> UnifiedQueueItem {
        serde_json::from_str(&format!(
            r#"{{
                "queue_id": "q-{tenant}",
                "idempotency_key": "i-{tenant}",
                "item_type": "folder",
                "op": "scan",
                "tenant_id": "{tenant}",
                "collection": "projects",
                "status": "in_progress",
                "branch": "main",
                "payload_json": "{{}}",
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z"
            }}"#
        ))
        .unwrap()
    }

    #[tokio::test]
    async fn enqueue_subdirectory_preserves_branch_and_uplift_op() {
        let project = tempfile::tempdir().unwrap();
        let child = project.path().join("child");
        std::fs::create_dir(&child).unwrap();
        let root = CanonicalPath::from_user_input(&project.path().to_string_lossy()).unwrap();

        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let qm = Arc::new(QueueManager::new(pool.clone()));
        qm.init_unified_queue().await.unwrap();

        let mut item = scan_item("t-subdir");
        item.branch = "dev-clean".to_string();
        let queued = enqueue_subdirectory(
            &child,
            &root,
            &item,
            &qm,
            Some("2026-06-20T20:34:32.253Z"),
            true,
        )
        .await;
        assert_eq!(queued, 1);

        let row: (String, String, String) = sqlx::query_as(
            "SELECT op, branch, payload_json FROM unified_queue WHERE tenant_id = 't-subdir'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(row.0, "uplift");
        assert_eq!(row.1, "dev-clean");
        assert!(row.2.contains(r#""folder_path":"child""#));
        assert!(row.2.contains(r#""uplift":true"#));
    }

    /// tecsul 2026-10-07: a `.wqmignore` added while the first scan was still
    /// descending left ~230 queued scans inside the newly excluded `c/`; each
    /// enqueued its files (dropped at dequeue) and its subdirectories (more
    /// scans). A scan under an ignored ancestor now enqueues nothing, while a
    /// scan in the kept tree still descends. (Only the subdirectory count is
    /// compared for the kept tree: the file exclusion gate matches components
    /// of the absolute path, and a tempdir's `.tmpXXXX` would exclude files.)
    #[tokio::test]
    async fn a_scan_under_an_ignored_ancestor_enqueues_nothing() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join(".wqmignore"), "/*\n!/src/\n").unwrap();
        for dir in ["c/rtos/port", "src/app"] {
            let dir = project.path().join(dir);
            std::fs::create_dir_all(dir.join("sub")).unwrap();
            std::fs::write(dir.join("main.c"), "int main(void) { return 0; }\n").unwrap();
        }
        let root = CanonicalPath::from_user_input(&project.path().to_string_lossy()).unwrap();
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let qm = Arc::new(QueueManager::new(pool.clone()));
        qm.init_unified_queue().await.unwrap();
        let ext = Arc::new(AllowedExtensions::default());
        let item = scan_item("t-ignored-ancestor");

        let ignored = project.path().join("c/rtos/port");
        let outcome = scan_directory_single_level(&ignored, &root, &item, &qm, &ext, None, false)
            .await
            .unwrap();
        assert_eq!(outcome, (0, 0, 1, 0));
        let queued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM unified_queue")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(queued, 0, "nothing under an ignored ancestor is enqueued");

        let kept = project.path().join("src/app");
        let (_, dirs, _, errors) =
            scan_directory_single_level(&kept, &root, &item, &qm, &ext, None, false)
                .await
                .unwrap();
        assert_eq!((dirs, errors), (1, 0), "the kept tree still descends");
    }
}
