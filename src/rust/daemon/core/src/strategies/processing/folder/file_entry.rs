//! One file met by a folder scan (the filesystem walk or the git fast path):
//! the exclusion and indexability gates, mtime pruning, the size cap, and the
//! enqueue.

use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use tracing::{debug, warn};
use wqm_common::paths::{CanonicalPath, RelativePath};

use crate::allowed_extensions::AllowedExtensions;
use crate::file_classification::classify_file_type;
use crate::patterns::exclusion::should_exclude_file_in;
use crate::queue_operations::QueueManager;
use crate::unified_queue_schema::{FilePayload, ItemType, QueueOperation, UnifiedQueueItem};

/// Process a single file entry encountered during scan.
///
/// `baseline` is the parsed mtime pruning threshold: files with
/// `mtime <= baseline` are skipped as unchanged. `uplift` selects the
/// queue operation: `File/Uplift` (forced re-processing) instead of
/// `File/Add`.
///
/// Returns 1 if the file was enqueued, 0 otherwise.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn process_file_entry(
    path: &Path,
    watch_folder_root: &CanonicalPath,
    item: &UnifiedQueueItem,
    queue_manager: &Arc<QueueManager>,
    allowed_extensions: &Arc<AllowedExtensions>,
    baseline: Option<&SystemTime>,
    uplift: bool,
    files_excluded: &mut u64,
    errors: &mut u64,
) -> u64 {
    let abs_path = path.to_string_lossy();

    if should_exclude_file_in(Path::new(watch_folder_root.as_str()), &abs_path) {
        *files_excluded += 1;
        return 0;
    }

    // `is_indexable`, not `is_allowed`: a project's .pdf/.docx is indexed into
    // its library (the queue routes the item there), like the watcher and the
    // startup reconciler already treated it.
    if !allowed_extensions.is_indexable(&abs_path, &item.collection) {
        *files_excluded += 1;
        return 0;
    }

    let metadata = match path.metadata() {
        Ok(m) => m,
        Err(e) => {
            warn!("Failed to get metadata for {}: {}", abs_path, e);
            *errors += 1;
            return 0;
        }
    };

    if !uplift && should_prune_by_mtime(baseline, &metadata) {
        debug!("mtime prune: skipping unchanged file {}", abs_path);
        *files_excluded += 1;
        return 0;
    }

    if metadata.len() > crate::strategies::processing::max_ingest_file_bytes() {
        debug!(
            "Skipping large file: {} ({} bytes)",
            abs_path,
            metadata.len()
        );
        *files_excluded += 1;
        return 0;
    }

    enqueue_scanned_file(
        path,
        &abs_path,
        watch_folder_root,
        &metadata,
        item,
        queue_manager,
        uplift,
        errors,
    )
    .await
}

/// Check mtime pruning: returns `true` if the file is unchanged since baseline.
fn should_prune_by_mtime(baseline: Option<&SystemTime>, metadata: &std::fs::Metadata) -> bool {
    if let Some(bl) = baseline {
        metadata.modified().map(|m| m <= *bl).unwrap_or(false)
    } else {
        false
    }
}

/// Build the file payload (anchored to `watch_folder_root`) and enqueue
/// the file. Returns 1 on success, 0 on failure.
#[allow(clippy::too_many_arguments)]
async fn enqueue_scanned_file(
    path: &Path,
    abs_path: &std::borrow::Cow<'_, str>,
    watch_folder_root: &CanonicalPath,
    metadata: &std::fs::Metadata,
    item: &UnifiedQueueItem,
    queue_manager: &Arc<QueueManager>,
    uplift: bool,
    errors: &mut u64,
) -> u64 {
    let file_type_class = classify_file_type(path);

    let abs = match CanonicalPath::from_user_input(abs_path) {
        Ok(a) => a,
        Err(e) => {
            warn!("File path failed canonicalization ({}): {}", abs_path, e);
            *errors += 1;
            return 0;
        }
    };
    let relative = match RelativePath::from_absolute_and_root(&abs, watch_folder_root) {
        Ok(r) => r,
        Err(e) => {
            warn!(
                "File {} not under watch_folder root {} ({}); skipping",
                abs.as_str(),
                watch_folder_root.as_str(),
                e
            );
            *errors += 1;
            return 0;
        }
    };

    let file_payload = FilePayload {
        file_path: relative,
        file_type: Some(file_type_class.as_str().to_string()),
        file_hash: None,
        size_bytes: Some(metadata.len()),
        old_path: None,
    };

    let payload_json = match serde_json::to_string(&file_payload) {
        Ok(j) => j,
        Err(e) => {
            warn!("Failed to serialize FilePayload for {}: {}", abs_path, e);
            *errors += 1;
            return 0;
        }
    };

    // Uplift = forced re-processing: prepare_uplift deletes the old points
    // and the ingest pipeline re-runs regardless of the unchanged-hash +
    // chunker-fingerprint skip that gates Add/Update.
    let op = if uplift {
        QueueOperation::Uplift
    } else {
        QueueOperation::Add
    };
    match queue_manager
        .enqueue_unified(
            ItemType::File,
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
        Ok((_, false)) => 0,
        Err(e) => {
            warn!("Failed to queue file {}: {}", abs_path, e);
            *errors += 1;
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategies::processing::folder::scan::tests::scan_item;

    /// A project root the exclusion gate does not reject by itself. The gate
    /// matches components of the ABSOLUTE path, so a hidden ancestor such as
    /// `/tmp/.tmpXXXX` excludes every file below it: `tempfile::tempdir()` made
    /// `process_file_entry` return 0 whatever the test was about (the uplift
    /// test below had been failing, unnoticed, in no gate).
    fn project_dir() -> tempfile::TempDir {
        let base = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        tempfile::Builder::new()
            .prefix("wqm-file-entry-")
            .tempdir_in(base)
            .unwrap()
    }

    /// `uplift` selects the queue operation for discovered files: Add for
    /// normal discovery, Uplift for forced re-processing (ReembedTenant
    /// force). Both the FS walk and the git fast-path funnel through
    /// `process_file_entry` → `enqueue_scanned_file`, where the op is
    /// chosen — tested directly because the exclusion gates above it match
    /// path segments of tempdirs (`.tmpXXXX`, `tmp/`, the container's
    /// `/build` root) and are not what this test is about.
    #[tokio::test]
    async fn enqueue_scanned_file_op_follows_uplift_flag() {
        let project = tempfile::tempdir().unwrap();
        let file = project.path().join("main.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let metadata = std::fs::metadata(&file).unwrap();
        let abs_path = file.to_string_lossy();
        let root = CanonicalPath::from_user_input(&project.path().to_string_lossy()).unwrap();

        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let qm = Arc::new(QueueManager::new(pool.clone()));
        qm.init_unified_queue().await.unwrap();

        let mut errors = 0u64;
        let queued = enqueue_scanned_file(
            &file,
            &abs_path,
            &root,
            &metadata,
            &scan_item("t-add"),
            &qm,
            false,
            &mut errors,
        )
        .await;
        assert_eq!((queued, errors), (1, 0));

        let queued = enqueue_scanned_file(
            &file,
            &abs_path,
            &root,
            &metadata,
            &scan_item("t-uplift"),
            &qm,
            true,
            &mut errors,
        )
        .await;
        assert_eq!((queued, errors), (1, 0));

        let op_add: String =
            sqlx::query_scalar("SELECT op FROM unified_queue WHERE tenant_id = 't-add'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(op_add, "add");

        let op_uplift: String =
            sqlx::query_scalar("SELECT op FROM unified_queue WHERE tenant_id = 't-uplift'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(op_uplift, "uplift");
    }

    #[tokio::test]
    async fn uplift_bypasses_mtime_pruning() {
        let project = project_dir();
        let file = project.path().join("main.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let root = CanonicalPath::from_user_input(&project.path().to_string_lossy()).unwrap();
        let future_baseline = SystemTime::now() + std::time::Duration::from_secs(86_400);

        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let qm = Arc::new(QueueManager::new(pool.clone()));
        qm.init_unified_queue().await.unwrap();
        let allowed = Arc::new(AllowedExtensions::default());

        let mut excluded = 0u64;
        let mut errors = 0u64;
        let add_queued = process_file_entry(
            &file,
            &root,
            &scan_item("t-pruned-add"),
            &qm,
            &allowed,
            Some(&future_baseline),
            false,
            &mut excluded,
            &mut errors,
        )
        .await;
        assert_eq!(add_queued, 0, "normal Add should still honor mtime pruning");

        let uplift_queued = process_file_entry(
            &file,
            &root,
            &scan_item("t-uplift-mtime"),
            &qm,
            &allowed,
            Some(&future_baseline),
            true,
            &mut excluded,
            &mut errors,
        )
        .await;
        assert_eq!(
            uplift_queued, 1,
            "Uplift is a forced rebuild and must ignore mtime"
        );
        assert_eq!(errors, 0);

        let op: String =
            sqlx::query_scalar("SELECT op FROM unified_queue WHERE tenant_id = 't-uplift-mtime'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(op, "uplift");
    }

    /// The scan refused a project's `.docx` (`is_allowed(.., "projects")`)
    /// while the watcher indexed it: only an edit ever brought one in. Now it is
    /// admitted and the queue routes it to the project's library; a private key
    /// named `*.key` stays out.
    #[tokio::test]
    async fn a_scanned_project_document_is_enqueued_into_the_projects_library() {
        let project = project_dir();
        let doc = project.path().join("guide.docx");
        let key = project.path().join("server.key");
        std::fs::write(&doc, b"PK\x03\x04 not really a zip").unwrap();
        std::fs::write(&key, b"-----BEGIN PRIVATE KEY-----\n").unwrap();
        let root = CanonicalPath::from_user_input(&project.path().to_string_lossy()).unwrap();

        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let qm = Arc::new(QueueManager::new(pool.clone()));
        qm.init_unified_queue().await.unwrap();
        let allowed = Arc::new(AllowedExtensions::default());

        let (mut excluded, mut errors) = (0u64, 0u64);
        for (path, expected) in [(&doc, 1), (&key, 0)] {
            let queued = process_file_entry(
                path,
                &root,
                &scan_item("t-doc"),
                &qm,
                &allowed,
                None,
                false,
                &mut excluded,
                &mut errors,
            )
            .await;
            assert_eq!(queued, expected, "{}", path.display());
        }
        assert_eq!((excluded, errors), (1, 0));

        let rows: Vec<(String, String, String)> =
            sqlx::query_as("SELECT tenant_id, collection, file_path FROM unified_queue")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            rows,
            vec![(
                "t-doc-refs".to_string(),
                "libraries".to_string(),
                "guide.docx".to_string()
            )]
        );
    }
}
