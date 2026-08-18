//! File delete processing.
//!
//! Handles `QueueOperation::Delete` for file items: reference-counted Qdrant
//! point deletion, tracked_files cleanup, FTS5 cleanup, and missing-file
//! reconciliation.

use std::path::Path;
use std::sync::OnceLock;
use std::time::Instant;

use sqlx::SqlitePool;
use tracing::{debug, info, warn};

use crate::context::ProcessingContext;
use crate::fts_batch_processor::{FtsBatchConfig, FtsBatchProcessor};
use crate::processing_timings::{self, PhaseTiming};
use crate::tracked_files_schema;
use crate::tree_sitter::detect_language;
use crate::unified_queue_processor::{UnifiedProcessorError, UnifiedProcessorResult};
use crate::unified_queue_schema::UnifiedQueueItem;

/// What to do with a branch-prune delete whose target generation is COVERED by
/// another generation on a live branch (issue #224 stage 3).
///
/// Set via `WQM_BRANCH_PRUNE_COVERED_DELETE`:
/// * `off` — never delete a covered generation (pre-stage-3 behaviour).
/// * `dry` — **default**: log what WOULD be deleted, mutate nothing.
/// * `on`  — delete covered stale generations.
///
/// Default `dry` because this is the only deletion-capable half of #224: the
/// numbers get reviewed on a live cycle before anything is removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CoveredDeletePolicy {
    Off,
    Dry,
    On,
}

/// Parse the policy from the env value. Pure (unit-tested); unknown/absent
/// values fall back to `Dry` — an unreadable knob must never start deleting.
pub(crate) fn parse_covered_delete_policy(raw: Option<&str>) -> CoveredDeletePolicy {
    match raw.map(str::trim) {
        Some("on") => CoveredDeletePolicy::On,
        Some("off") => CoveredDeletePolicy::Off,
        _ => CoveredDeletePolicy::Dry,
    }
}

/// Cached read of `WQM_BRANCH_PRUNE_COVERED_DELETE` (env is process-stable).
fn covered_delete_policy() -> CoveredDeletePolicy {
    static POLICY: OnceLock<CoveredDeletePolicy> = OnceLock::new();
    *POLICY.get_or_init(|| {
        let raw = std::env::var("WQM_BRANCH_PRUNE_COVERED_DELETE").ok();
        let policy = parse_covered_delete_policy(raw.as_deref());
        info!("[stage3] covered-generation delete policy: {:?}", policy);
        policy
    })
}

/// What to do when a `(path, branch)` strip finds the branch tag on MORE than
/// one generation (issue #224 overlap groups).
///
/// A branch names exactly ONE blob per path, so at most one generation may
/// legitimately hold its tag; the primary strip targets the newest holder
/// (what `lookup_tracked_file`'s `ORDER BY updated_at DESC LIMIT 1` returns —
/// and what reads serve), and every OLDER holder is shadowed debris the
/// primary strip can never reach. Left alone, the shadowed tag survives
/// forever and the overlap census only grows.
///
/// Set via `WQM_BRANCH_STRIP_ALL_GENERATIONS`:
/// * `off` — strip only the primary holder (pre-#224 behaviour).
/// * `dry` — **default**: log the shadowed holders that WOULD be stripped,
///   mutate nothing beyond the primary.
/// * `on`  — run the same reference-counted removal on every shadowed holder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShadowedStripPolicy {
    Off,
    Dry,
    On,
}

/// Parse the policy from the env value. Pure (unit-tested); unknown/absent
/// values fall back to `Dry` — an unreadable knob must never start deleting.
pub(crate) fn parse_shadowed_strip_policy(raw: Option<&str>) -> ShadowedStripPolicy {
    match raw.map(str::trim) {
        Some("on") => ShadowedStripPolicy::On,
        Some("off") => ShadowedStripPolicy::Off,
        _ => ShadowedStripPolicy::Dry,
    }
}

/// Cached read of `WQM_BRANCH_STRIP_ALL_GENERATIONS` (env is process-stable).
fn shadowed_strip_policy() -> ShadowedStripPolicy {
    static POLICY: OnceLock<ShadowedStripPolicy> = OnceLock::new();
    *POLICY.get_or_init(|| {
        let raw = std::env::var("WQM_BRANCH_STRIP_ALL_GENERATIONS").ok();
        let policy = parse_shadowed_strip_policy(raw.as_deref());
        info!("[overlap] shadowed-holder strip policy: {:?}", policy);
        policy
    })
}

/// Strip `item.branch` from every generation of `(watch_folder, path)` that
/// still holds it beyond the already-processed primary row (#224).
///
/// Runs AFTER the primary strip so the newest holder keeps today's exact
/// semantics (guards, refcounts, F-035 error propagation). Shadowed holders
/// go through the same [`delete_tracked_file`] per row — same preserve
/// guards, same reference-counted point deletion, and the path-keyed
/// cleanups stay protected by the in-situ survivor check. Failures on a
/// shadowed row are logged and skipped: the next `(path, branch)` strip
/// retries them, and a debris row must never fail the primary operation.
#[allow(clippy::too_many_arguments)]
pub(super) async fn strip_shadowed_holders(
    ctx: &ProcessingContext,
    item: &UnifiedQueueItem,
    pool: &SqlitePool,
    watch_folder_id: &str,
    relative_path: &str,
    abs_file_path: &str,
    primary_file_id: i64,
    timings: &mut Vec<PhaseTiming>,
) {
    let policy = shadowed_strip_policy();
    if policy == ShadowedStripPolicy::Off {
        return;
    }

    let holders = match tracked_files_schema::lookup_tracked_files_holding_branch(
        pool,
        watch_folder_id,
        relative_path,
        &item.branch,
    )
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            warn!(
                "[overlap] holder lookup failed for '{}' branch '{}': {} — skipping \
                 shadowed-holder sweep",
                relative_path, item.branch, e
            );
            return;
        }
    };
    let shadowed: Vec<_> = holders
        .into_iter()
        .filter(|h| h.file_id != primary_file_id)
        .collect();
    if shadowed.is_empty() {
        return;
    }

    let ids: Vec<i64> = shadowed.iter().map(|h| h.file_id).collect();
    if policy == ShadowedStripPolicy::Dry {
        info!(
            "[overlap-dry] '{}': branch '{}' also held by {} shadowed generation(s) \
             (file_ids={:?}) — WOULD strip; set WQM_BRANCH_STRIP_ALL_GENERATIONS=on to enable",
            relative_path,
            item.branch,
            shadowed.len(),
            ids
        );
        return;
    }

    let mut stripped = 0usize;
    for holder in &shadowed {
        match delete_tracked_file(
            ctx,
            item,
            pool,
            watch_folder_id,
            relative_path,
            abs_file_path,
            holder,
            timings,
            Instant::now(),
        )
        .await
        {
            Ok(()) => stripped += 1,
            Err(e) => warn!(
                "[overlap] failed to strip shadowed holder file_id={} of '{}' branch '{}': {} \
                 — continuing (next strip retries)",
                holder.file_id, relative_path, item.branch, e
            ),
        }
    }
    info!(
        "[overlap] '{}': stripped branch '{}' from {}/{} shadowed generation(s) (file_ids={:?})",
        relative_path,
        item.branch,
        stripped,
        shadowed.len(),
        ids
    );
}

/// Process file delete operation with tracked_files awareness (Task 506 + Task 519).
///
/// **F-035 contract:** Qdrant delete failures block SQLite cleanup and return
/// `Err`. The tracked_files row stays intact and the queue row enters retry
/// via `mark_unified_failed`. Without this, stale Qdrant vectors stayed
/// retrievable after the local row was wiped, with no record that cleanup
/// had failed.
pub(super) async fn process_file_delete(
    ctx: &ProcessingContext,
    item: &UnifiedQueueItem,
    pool: &SqlitePool,
    watch_folder_id: &str,
    relative_path: &str,
    abs_file_path: &str,
) -> UnifiedProcessorResult<()> {
    let delete_start = Instant::now();
    let mut timings: Vec<PhaseTiming> = Vec::new();
    let target_path = Path::new(abs_file_path);
    let detected_language = detect_language(target_path);

    if delete_target_still_exists(target_path) {
        if bypasses_on_disk_skip(item) {
            // Reconciler-driven delete (branch pruning or ignore-rule exclusion),
            // NOT a stale watcher event: the file is still on disk but its index
            // entry must be reconciled anyway — a pruned branch's tag dropped, or a
            // now-ignored file removed. Fall through to the reference-counted
            // removal below.
            debug!(
                "Reconciler delete for '{}' on branch '{}': file still on disk \
                 — proceeding (not a stale watcher event)",
                abs_file_path, item.branch
            );
        } else {
            info!(
                "Skipping stale delete for existing file on disk: {}",
                abs_file_path
            );
            record_delete_timings(ctx, item, pool, detected_language, &timings).await;
            return Ok(());
        }
    }

    if let Ok(true) = tracked_files_schema::is_incremental(pool, abs_file_path).await {
        info!(
            "Skipping delete for incremental file (safety net): {}",
            abs_file_path
        );
        return Ok(());
    }

    if let Ok(Some(existing)) = tracked_files_schema::lookup_tracked_file(
        pool,
        watch_folder_id,
        relative_path,
        Some(item.branch.as_str()),
    )
    .await
    {
        timings.push(PhaseTiming {
            phase: "lookup",
            duration_ms: delete_start.elapsed().as_millis() as u64,
        });

        let delete_result = delete_tracked_file(
            ctx,
            item,
            pool,
            watch_folder_id,
            relative_path,
            abs_file_path,
            &existing,
            &mut timings,
            delete_start,
        )
        .await;

        // #224: the primary strip only reaches the newest holder; sweep any
        // shadowed generations still carrying this tag (policy-gated). Only
        // after a successful primary — a failed primary retries the whole item.
        if delete_result.is_ok() {
            strip_shadowed_holders(
                ctx,
                item,
                pool,
                watch_folder_id,
                relative_path,
                abs_file_path,
                existing.file_id,
                &mut timings,
            )
            .await;
        }

        record_delete_timings(ctx, item, pool, detected_language, &timings).await;
        return delete_result;
    }

    // Reaching here proves only that no generation is tagged `item.branch` — NOT
    // that the path is untracked. Under Layer 2 (#124) a path legitimately carries
    // several content generations, each tagged with the branches holding it, and
    // `delete_points_by_filter` matches on (file_path, tenant_id) with no branch
    // or base_point scope. Sweeping here therefore wipes the points of generations
    // nobody asked to delete, leaving their `tracked_files` rows advertising chunks
    // Qdrant no longer has — the same path-keyed blast radius #273 had to dodge for
    // graph edges and keyword extractions. Observed live 2026-07-16: 5 example-monorepo
    // protos holding 308 chunk_count between them and 0 points, re-swept ~40x/hour
    // by a delete that could never converge (#224). The filter sweep exists for a
    // genuinely orphaned path, so require that no generation survives.
    if let Ok(Some(other)) =
        tracked_files_schema::lookup_tracked_file(pool, watch_folder_id, relative_path, None).await
    {
        debug!(
            "Skipping fallback filter-delete for '{}': not tracked on branch '{}', but \
             generation file_id={} still holds it (branches={:?}) — a path-keyed filter \
             delete would wipe that generation's points",
            relative_path, item.branch, other.file_id, other.branches
        );
        record_delete_timings(ctx, item, pool, detected_language, &timings).await;
        return Ok(());
    }

    // Fallback: file genuinely absent from tracked_files — attempt Qdrant filter delete.
    // F-035: a fallback Qdrant delete failure is still a real failure — the
    // points may exist but cannot be deleted. Surface it so retry metadata is
    // populated.
    let fallback_result =
        fallback_qdrant_delete(ctx, item, abs_file_path, delete_start, &mut timings).await;
    record_delete_timings(ctx, item, pool, detected_language, &timings).await;
    fallback_result
}

/// Delete a file that is present in tracked_files: Qdrant (ref-counted), SQLite,
/// FTS5, graph edges, and keyword extractions.
///
/// **F-035:** if the Qdrant delete fails, `tracked_files` row is preserved and
/// `Err` is returned so the queue row gets retry metadata. Without this guard,
/// stale vectors stayed in Qdrant while local tracking was already wiped.
#[allow(clippy::too_many_arguments)]
pub(super) async fn delete_tracked_file(
    ctx: &ProcessingContext,
    item: &UnifiedQueueItem,
    pool: &SqlitePool,
    watch_folder_id: &str,
    relative_path: &str,
    abs_file_path: &str,
    existing: &tracked_files_schema::TrackedFile,
    timings: &mut Vec<PhaseTiming>,
    delete_start: Instant,
) -> UnifiedProcessorResult<()> {
    let bp = existing.base_point.as_deref();

    // Post-delete predicates (computed before mutating; the queries exclude THIS
    // row). Layer 2 stage 2: one row per content with a `branches` set; the
    // physical point is shared across branches AND clones.
    //   - r_new_empty: dropping item.branch empties THIS row's set (row vanishes).
    //   - other_refs_bp: another row (clone) still references base_point.
    //   - other_holds_x: another row still holds item.branch for base_point.
    let r_new_empty = would_remove_last_tag(&existing.branches, item.branch.as_str());

    // Branch-prune safety net (paired with the marker bypass in
    // `process_file_delete`): when dropping `item.branch` would empty this file's
    // branch set — removing its last index entry, not just one tag — but the file
    // is still present on disk, it is a present-but-mislabeled file, NOT a deleted
    // one. Preserve the entry: a stale-branch label is benign, deleting the index
    // of a working-tree file is not (the corpus-wipe this module's guards exist to
    // prevent). Reconciliation re-tags it under the live branch on a later pass.
    // Watcher/folder deletes never trip this — they reach `delete_tracked_file`
    // only once the file is gone from disk, so the on-disk check is false. The
    // `is_branch_prune_delete` gate scopes the preserve to branch pruning ONLY:
    // an ignore-rule exclusion (`is_ignore_excluded_delete`) wants the now-ignored
    // file gone from the index entirely, so it must fall through and delete even
    // its last tag.
    // Stage 3 (#224) refines this guard with the fact it always lacked: is the
    // on-disk file served by ANOTHER generation on a live branch? The Layer-2
    // model keeps one row per distinct cross-branch content, so a path routinely
    // has several generations and only the live-tagged one answers reads. When
    // `covered_by_live_generation` is stamped, this row is stale debris — the
    // file stays indexed via the covering generation, so preserving it just
    // keeps dead weight AND re-enqueues the same no-op delete every startup
    // (measured 2026-07-15: 1,034 preserves per boot). Without the flag the
    // original protection stands unchanged.
    let covered = is_covered_by_live_generation(item);
    if r_new_empty
        && delete_target_still_exists(Path::new(abs_file_path))
        && is_branch_prune_delete(item)
        && !(covered && covered_delete_policy() == CoveredDeletePolicy::On)
    {
        if covered && covered_delete_policy() == CoveredDeletePolicy::Dry {
            debug!(
                "[stage3-dry] WOULD delete stale generation of '{}' (file_id={}, pruned \
                 branch '{}' is its only tag; another live generation serves the file) — \
                 set WQM_BRANCH_PRUNE_COVERED_DELETE=on to enable",
                relative_path, existing.file_id, item.branch
            );
        } else {
            info!(
                "Preserving index for '{}': pruned branch '{}' is its only tag but the \
                 file is still on disk (mislabeled, not deleted) — leaving entry for \
                 reconciliation to re-tag",
                relative_path, item.branch
            );
        }
        return Ok(());
    }

    let (other_refs_bp, other_holds_x) = match bp {
        Some(bp) => (
            ctx.queue_manager
                .has_other_references(bp, existing.file_id)
                .await
                .unwrap_or(false),
            ctx.queue_manager
                .branch_held_by_other(bp, existing.file_id, &item.branch)
                .await
                .unwrap_or(false),
        ),
        None => (false, false),
    };
    // Delete the physical points only when this row vanishes AND no other row
    // references the base_point.
    let delete_points = r_new_empty && !other_refs_bp;

    let t0 = Instant::now();
    if let Some(bp) = bp {
        if delete_points {
            if let Err(e) = delete_qdrant_points(ctx, item, pool, relative_path, existing).await {
                timings.push(PhaseTiming {
                    phase: "qdrant_delete",
                    duration_ms: t0.elapsed().as_millis() as u64,
                });
                warn!(
                    "Qdrant delete failed for {} — leaving tracked_files row intact and queuing retry: {}",
                    relative_path, e
                );
                return Err(e);
            }
        } else if !other_holds_x {
            // Content stays (another row/clone references it), but this branch is
            // no longer held by any row — drop it from the shared point's array.
            if let Err(e) = ctx
                .storage_client
                .remove_branch_from_base_point(&item.collection, bp, &item.branch)
                .await
            {
                warn!(
                    "Failed to drop branch {} from base_point {} on delete of {}: {}",
                    item.branch, bp, relative_path, e
                );
            }
        }
    }
    timings.push(PhaseTiming {
        phase: "qdrant_delete",
        duration_ms: t0.elapsed().as_millis() as u64,
    });

    // SQLite: remove the branch from the content-row's set (deletes the row when
    // its set empties; CASCADE drops qdrant_chunks).
    let t0 = Instant::now();
    let remaining =
        tracked_files_schema::remove_branch_from_tracked_file(pool, existing.file_id, &item.branch)
            .await;
    timings.push(PhaseTiming {
        phase: "sqlite_cleanup",
        duration_ms: t0.elapsed().as_millis() as u64,
    });
    let row_deleted = matches!(remaining, Ok(0));
    if let Err(e) = &remaining {
        warn!(
            "SQLite branch-remove failed for {}: {}. Marked for reconciliation.",
            relative_path, e
        );
        let _ = tracked_files_schema::mark_needs_reconcile(
            pool,
            existing.file_id,
            &format!("branch_remove_failed: {}", e),
        )
        .await;
        return Ok(());
    }

    if row_deleted {
        // Content fully gone for this ROW — purge FTS5 (keyed by file_id, so it
        // only touches this generation's code_lines).
        let t0 = Instant::now();
        cleanup_fts5(ctx, existing).await;
        timings.push(PhaseTiming {
            phase: "fts5_cleanup",
            duration_ms: t0.elapsed().as_millis() as u64,
        });
        // Graph edges and keyword extractions are keyed by (tenant, PATH), not by
        // file_id — they describe the file, not one content generation. When
        // another generation still tracks this path (stage 3 covered delete, or
        // an overlap-debris strip, #224), purging them here would wipe the
        // graph/keywords of a file that remains fully indexed — the #235/#245
        // "edges wiped, never rebuilt" failure mode, self-inflicted. Only the
        // last generation of a path may clear them. Checked two ways: the
        // producer-stamped `covered` flag (prune deletes) AND an in-situ
        // survivor query — the latter needs no producer cooperation, so it also
        // protects watcher/update/overlap-sweep deletes. On a query error, skip
        // the cleanup: stale graph edges for a truly-gone path are rebuilt on
        // the next ingest of the path, wiping a live file's graph is not.
        let survivor = match tracked_files_schema::other_generation_exists(
            pool,
            watch_folder_id,
            relative_path,
            existing.file_id,
        )
        .await
        {
            Ok(exists) => exists,
            Err(e) => {
                warn!(
                    "[overlap] survivor check failed for '{}' (file_id={}): {} — keeping \
                     path-keyed graph edges + keyword extraction (conservative)",
                    relative_path, existing.file_id, e
                );
                true
            }
        };
        if covered || survivor {
            debug!(
                "'{}' — keeping graph edges + keyword extraction: path still tracked by \
                 another generation (covered={}, survivor={})",
                relative_path, covered, survivor
            );
        } else {
            super::graph_ingest::delete_graph_edges(ctx, &item.tenant_id, relative_path).await;
            let doc_id = crate::generate_document_id(&item.tenant_id, abs_file_path);
            super::keyword_persist::delete_extraction(pool, &doc_id).await;
        }
    } else {
        // Branch removed but content remains — drop the branch from FTS5 metadata
        // so `grep` on this branch no longer matches, keeping code_lines for the
        // other branches.
        remove_branch_from_fts5(ctx, existing.file_id, &item.branch).await;
    }

    info!(
        "Removed branch {} for: {} in {}ms (row_deleted={}, delete_points={})",
        item.branch,
        relative_path,
        delete_start.elapsed().as_millis(),
        row_deleted,
        delete_points
    );
    Ok(())
}

/// Delete Qdrant points for a tracked file.
///
/// **F-035:** propagate Qdrant errors so the caller can short-circuit SQLite
/// cleanup. Returning `Ok(())` on failure silently dropped the local row
/// while leaving the vectors retrievable in Qdrant.
async fn delete_qdrant_points(
    ctx: &ProcessingContext,
    item: &UnifiedQueueItem,
    pool: &SqlitePool,
    relative_path: &str,
    existing: &tracked_files_schema::TrackedFile,
) -> UnifiedProcessorResult<()> {
    let point_ids = tracked_files_schema::get_chunk_point_ids(pool, existing.file_id)
        .await
        .unwrap_or_default();

    if point_ids.is_empty() {
        return Ok(());
    }

    // Layer 2: delete by the EXACT shared point IDs for this content-row, NOT by
    // a `file_path` filter. Two branches can hold the same path with different
    // content (different base_point ⇒ different points, identical file_path
    // payload); a path filter would wipe the other branch's vectors. This branch
    // only runs when `has_other_references == false`, so the base_point is truly
    // orphaned and deleting exactly these IDs is correct. F-035: Qdrant errors
    // propagate so the queue row retries (delete_points_by_ids errors on fault).
    ctx.storage_client
        .delete_points_by_ids(&item.collection, &point_ids)
        .await
        .map_err(|e| {
            UnifiedProcessorError::Storage(format!(
                "Qdrant delete failed for {} ({} tracked points): {}",
                relative_path,
                point_ids.len(),
                e
            ))
        })?;
    Ok(())
}

/// Clean up FTS5 code_lines for a fully-removed file (non-fatal).
async fn cleanup_fts5(ctx: &ProcessingContext, existing: &tracked_files_schema::TrackedFile) {
    if let Some(sdb) = &ctx.search_db {
        let processor = FtsBatchProcessor::new(sdb, FtsBatchConfig::default());
        if let Err(e) = processor.delete_file(existing.file_id).await {
            warn!(
                "FTS5: failed to delete code_lines for file_id={}: {} (non-fatal)",
                existing.file_id, e
            );
        } else {
            debug!("FTS5: deleted code_lines for file_id={}", existing.file_id);
        }
    }
}

/// Drop a single branch from a file's FTS5 `file_metadata` row WITHOUT deleting
/// its `code_lines` (Layer 2 stage 2 — other branches still hold the content).
async fn remove_branch_from_fts5(ctx: &ProcessingContext, file_id: i64, branch: &str) {
    if let Some(sdb) = &ctx.search_db {
        if let Err(e) = sdb.remove_branch_from_file_metadata(file_id, branch).await {
            warn!(
                "FTS5: failed to drop branch {} from file_id={}: {} (non-fatal)",
                branch, file_id, e
            );
        }
    }
}

/// Fallback delete when the file is not in tracked_files: attempt Qdrant filter delete.
///
/// **F-035:** Qdrant errors are real failures (network/auth/server fault),
/// not "points might not exist" — the Qdrant client returns Ok with zero
/// affected points if nothing matches the filter. Propagate Err so the
/// caller queues a retry instead of silently swallowing the failure.
async fn fallback_qdrant_delete(
    ctx: &ProcessingContext,
    item: &UnifiedQueueItem,
    abs_file_path: &str,
    delete_start: Instant,
    timings: &mut Vec<PhaseTiming>,
) -> UnifiedProcessorResult<()> {
    debug!(
        "File not in tracked_files, attempting Qdrant filter delete: {}",
        abs_file_path
    );
    let t0 = Instant::now();
    let result = ctx
        .storage_client
        .delete_points_by_filter(&item.collection, abs_file_path, &item.tenant_id)
        .await;
    timings.push(PhaseTiming {
        phase: "qdrant_delete",
        duration_ms: t0.elapsed().as_millis() as u64,
    });
    match result {
        Ok(_) => {
            info!(
                "Deleted points for file (fallback) in {}ms: {}",
                delete_start.elapsed().as_millis(),
                abs_file_path
            );
            Ok(())
        }
        Err(e) => Err(UnifiedProcessorError::Storage(format!(
            "Qdrant fallback delete failed for {}: {}",
            abs_file_path, e
        ))),
    }
}

/// Record timing data for delete operations.
async fn record_delete_timings(
    ctx: &ProcessingContext,
    item: &UnifiedQueueItem,
    pool: &SqlitePool,
    language: Option<&str>,
    timings: &[PhaseTiming],
) {
    let _ = ctx; // ProcessingContext not needed for recording, but kept for consistency
    processing_timings::record_timings(
        pool,
        &item.queue_id,
        item.item_type.as_str(),
        item.op.as_str(),
        &item.tenant_id,
        &item.collection,
        language,
        timings,
    )
    .await;
}

fn delete_target_still_exists(path: &Path) -> bool {
    path.metadata()
        .map(|metadata| metadata.is_file())
        .unwrap_or(false)
}

/// True when dropping `branch` would empty this file's branch set — i.e. it is
/// the file's last remaining tag, so removing it deletes the whole index entry
/// (Qdrant point + tracked row), not just one branch label. A multi-branch file
/// returns false: dropping one tag leaves the others (and the shared point).
fn would_remove_last_tag(branches: &[String], branch: &str) -> bool {
    branches.iter().all(|b| b.as_str() == branch)
}

/// True when this `file|delete` was enqueued by branch pruning (cleanup of a
/// now-deleted git branch's tags), identified by the `metadata` marker
/// [`crate::startup::reconciliation::branch_prune::BRANCH_PRUNE_DELETE_METADATA`]
/// stamps. Such deletes must run the branch-scoped, reference-counted removal
/// even when the file still exists on disk: the file lives on the live branch,
/// but its tag for the *pruned* branch must be dropped. Watcher and folder
/// deletes carry no such marker, so they keep the stale-file-on-disk skip.
fn is_branch_prune_delete(item: &UnifiedQueueItem) -> bool {
    use crate::startup::reconciliation::branch_prune::BRANCH_PRUNE_REASON;
    let Some(meta) = item.metadata.as_deref() else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(meta) else {
        return false;
    };
    value.get("reason").and_then(|r| r.as_str()) == Some(BRANCH_PRUNE_REASON)
}

/// True when branch pruning stamped `covered_by_live_generation` on this delete
/// (issue #224 stage 3): another generation of the same path carries a live
/// branch, so this row is stale Layer-2 debris rather than a mislabeled corpus.
///
/// Computed at enqueue time (only the reconciler holds the git live set) — see
/// `branch_prune::BRANCH_PRUNE_COVERED_DELETE_METADATA`. Absent, malformed, or
/// non-prune metadata → `false`: every unknown shape keeps the conservative
/// preserve behaviour, including deletes queued before this flag existed.
fn is_covered_by_live_generation(item: &UnifiedQueueItem) -> bool {
    if !is_branch_prune_delete(item) {
        return false;
    }
    let Some(meta) = item.metadata.as_deref() else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(meta) else {
        return false;
    };
    value
        .get("covered_by_live_generation")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// True when this `file|delete` was enqueued by ignore-rule reconciliation
/// (`ignore_sync`) because the file is now excluded by a `.wqmignore` rule yet is
/// still on disk. Identified by the `reason` field in the **payload** (where
/// `ignore_enqueue::build_payload` puts it — `IGNORE_RULE_CHANGE_REASON`), not
/// the metadata. Like a branch-prune delete it must bypass the stale-file-on-disk
/// skip — the file exists but its index entry must go because it is now ignored —
/// but UNLIKE branch pruning it removes the entry entirely (it is NOT gated by the
/// preserve guard): an ignored file should leave the index even when it is its own
/// last tag.
fn is_ignore_excluded_delete(item: &UnifiedQueueItem) -> bool {
    use crate::startup::reconciliation::ignore_enqueue::IGNORE_RULE_CHANGE_REASON;
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&item.payload_json) else {
        return false;
    };
    value.get("reason").and_then(|r| r.as_str()) == Some(IGNORE_RULE_CHANGE_REASON)
}

/// True when a delete is reconciler-driven (branch pruning or ignore-rule
/// exclusion) and must therefore bypass the "skip a stale delete for a file still
/// on disk" guard. Watcher and folder deletes carry no such marker and keep the
/// skip — that guard exists to absorb their stale events.
fn bypasses_on_disk_skip(item: &UnifiedQueueItem) -> bool {
    is_branch_prune_delete(item) || is_ignore_excluded_delete(item)
}

/// Clean up tracked records and Qdrant points for a file that no longer exists on disk.
///
/// **F-035:** if Qdrant deletion fails, returns `Err` and leaves the
/// tracked_files row intact. Without this, missing-file cleanup wiped the
/// local row while Qdrant still held vectors that could surface in search.
pub(super) async fn cleanup_missing_file(
    ctx: &ProcessingContext,
    item: &UnifiedQueueItem,
    pool: &SqlitePool,
    watch_folder_id: &str,
    relative_path: &str,
    abs_file_path: &str,
) -> UnifiedProcessorResult<()> {
    if let Ok(Some(existing)) = tracked_files_schema::lookup_tracked_file(
        pool,
        watch_folder_id,
        relative_path,
        Some(item.branch.as_str()),
    )
    .await
    {
        debug!(
            "File no longer exists, removing branch {} from tracked record: {}",
            item.branch, relative_path
        );
        // Reuse the branch-set delete path (Layer 2 stage 2): drop this branch
        // from the content-row's set, GC the row + shared points only when no
        // branch/clone holds the content anymore. Preserves F-035 (a Qdrant
        // failure returns Err, leaving the row for retry).
        let mut timings: Vec<PhaseTiming> = Vec::new();
        delete_tracked_file(
            ctx,
            item,
            pool,
            watch_folder_id,
            relative_path,
            abs_file_path,
            &existing,
            &mut timings,
            Instant::now(),
        )
        .await?;

        // #224: same shadowed-holder concern as the Delete and Update paths —
        // the file is gone from this tree, so every generation still tagged
        // with this branch is equally stale for it.
        strip_shadowed_holders(
            ctx,
            item,
            pool,
            watch_folder_id,
            relative_path,
            abs_file_path,
            existing.file_id,
            &mut timings,
        )
        .await;
    }
    Ok(())
}

/// Handle Qdrant insert failure by cleaning up stale SQLite state.
pub(super) async fn handle_qdrant_failure(
    _ctx: &ProcessingContext,
    item: &UnifiedQueueItem,
    pool: &SqlitePool,
    watch_folder_id: &str,
    relative_path: &str,
    qdrant_err: &str,
) {
    // Old Qdrant points were deleted but new ones failed to insert.
    // Clean up stale qdrant_chunks so SQLite doesn't reference non-existent points.
    if let Ok(Some(existing)) = tracked_files_schema::lookup_tracked_file(
        pool,
        watch_folder_id,
        relative_path,
        Some(item.branch.as_str()),
    )
    .await
    {
        let cleanup_result: Result<(), String> = async {
            let mut tx = pool.begin().await.map_err(|e| format!("begin tx: {}", e))?;
            tracked_files_schema::delete_qdrant_chunks_tx(&mut tx, existing.file_id)
                .await
                .map_err(|e| format!("delete chunks: {}", e))?;
            tx.commit().await.map_err(|e| format!("commit: {}", e))?;
            Ok(())
        }
        .await;

        match cleanup_result {
            Ok(()) => {
                warn!(
                    "Qdrant insert failed for {}; cleaned up stale SQLite chunks. Error: {}",
                    relative_path, qdrant_err
                );
            }
            Err(cleanup_err) => {
                warn!(
                    "Qdrant insert failed AND chunk cleanup failed for {}: insert={}, cleanup={}",
                    relative_path, qdrant_err, cleanup_err
                );
                let _ = tracked_files_schema::mark_needs_reconcile(
                    pool,
                    existing.file_id,
                    &format!("qdrant_insert_failed_cleanup_failed: {}", cleanup_err),
                )
                .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        bypasses_on_disk_skip, delete_target_still_exists, is_branch_prune_delete,
        is_covered_by_live_generation, is_ignore_excluded_delete, parse_covered_delete_policy,
        parse_shadowed_strip_policy, would_remove_last_tag, CoveredDeletePolicy,
        ShadowedStripPolicy,
    };
    use crate::unified_queue_schema::{ItemType, QueueOperation, QueueStatus, UnifiedQueueItem};

    #[test]
    fn would_remove_last_tag_only_when_branch_is_sole_tag() {
        let b = |s: &str| s.to_string();
        // Sole tag → removing it empties the set (the index entry would vanish).
        assert!(would_remove_last_tag(&[b("fix/feature")], "fix/feature"));
        // Multi-branch → dropping one tag leaves the others (point survives).
        assert!(!would_remove_last_tag(
            &[b("fix/feature"), b("main")],
            "fix/feature"
        ));
        assert!(!would_remove_last_tag(
            &[b("main"), b("dev")],
            "fix/feature"
        ));
    }

    #[test]
    fn delete_target_still_exists_only_for_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("present.ts");
        std::fs::write(&file, "export class Present {}\n").expect("write file");

        assert!(delete_target_still_exists(&file));
        assert!(!delete_target_still_exists(tmp.path()));
        assert!(!delete_target_still_exists(&tmp.path().join("missing.ts")));
    }

    fn delete_item_with_metadata(metadata: Option<&str>) -> UnifiedQueueItem {
        UnifiedQueueItem {
            queue_id: "q1".to_string(),
            idempotency_key: "k1".to_string(),
            item_type: ItemType::File,
            op: QueueOperation::Delete,
            tenant_id: "t1".to_string(),
            collection: "projects".to_string(),
            status: QueueStatus::Pending,
            branch: "claude/deleted-feature".to_string(),
            payload_json: "{}".to_string(),
            metadata: metadata.map(str::to_string),
            created_at: "2026-06-30T00:00:00Z".to_string(),
            updated_at: "2026-06-30T00:00:00Z".to_string(),
            lease_until: None,
            worker_id: None,
            retry_count: 0,
            error_message: None,
            last_error_at: None,
            file_path: Some("src/a.ts".to_string()),
            qdrant_status: None,
            search_status: None,
            decision_json: None,
        }
    }

    #[test]
    fn branch_prune_delete_detected_only_for_marked_items() {
        use crate::startup::reconciliation::branch_prune::BRANCH_PRUNE_DELETE_METADATA;

        // The exact marker branch_prune stamps must be recognized (also guards
        // against drift between the metadata JSON and the reason token).
        assert!(is_branch_prune_delete(&delete_item_with_metadata(Some(
            BRANCH_PRUNE_DELETE_METADATA
        ))));

        // Watcher/folder deletes carry no marker → keep the stale-file-on-disk skip.
        assert!(!is_branch_prune_delete(&delete_item_with_metadata(None)));
        assert!(!is_branch_prune_delete(&delete_item_with_metadata(Some(
            "{}"
        ))));
        assert!(!is_branch_prune_delete(&delete_item_with_metadata(Some(
            r#"{"reason":"watcher"}"#
        ))));

        // Malformed metadata must never be mistaken for a prune marker.
        assert!(!is_branch_prune_delete(&delete_item_with_metadata(Some(
            "not json"
        ))));
    }

    #[test]
    fn ignore_excluded_delete_detected_from_payload_reason() {
        use crate::startup::reconciliation::branch_prune::BRANCH_PRUNE_DELETE_METADATA;
        use crate::startup::reconciliation::ignore_enqueue::IGNORE_RULE_CHANGE_REASON;

        // ignore_sync stamps the reason in the PAYLOAD (not metadata).
        let mut ignored = delete_item_with_metadata(None);
        ignored.payload_json =
            format!(r#"{{"file_path":"src/a.ts","reason":"{IGNORE_RULE_CHANGE_REASON}"}}"#);
        assert!(is_ignore_excluded_delete(&ignored));
        assert!(bypasses_on_disk_skip(&ignored));
        // ...but it is NOT a branch-prune delete → the preserve guard must NOT apply
        // (an ignored file must leave the index even as its own last tag).
        assert!(!is_branch_prune_delete(&ignored));

        // Branch-prune deletes bypass the skip too, but are not ignore-excluded.
        let pruned = delete_item_with_metadata(Some(BRANCH_PRUNE_DELETE_METADATA));
        assert!(bypasses_on_disk_skip(&pruned));
        assert!(!is_ignore_excluded_delete(&pruned));

        // A plain watcher delete (FilePayload, no reason) bypasses nothing.
        let watcher = delete_item_with_metadata(None);
        assert!(!is_ignore_excluded_delete(&watcher));
        assert!(!bypasses_on_disk_skip(&watcher));

        // A different payload reason is not ignore-exclusion.
        let mut other = delete_item_with_metadata(None);
        other.payload_json = r#"{"file_path":"src/a.ts","reason":"something_else"}"#.to_string();
        assert!(!is_ignore_excluded_delete(&other));
    }

    // ── Stage 3 (#224): covered-generation deletes ────────────────────────

    #[test]
    fn covered_flag_read_only_from_a_prune_delete_with_the_flag_set() {
        use crate::startup::reconciliation::branch_prune::{
            BRANCH_PRUNE_COVERED_DELETE_METADATA, BRANCH_PRUNE_DELETE_METADATA,
        };

        // The exact marker branch_prune stamps for a covered generation.
        assert!(is_covered_by_live_generation(&delete_item_with_metadata(
            Some(BRANCH_PRUNE_COVERED_DELETE_METADATA)
        )));

        // A plain prune delete (uncovered) keeps the preserve behaviour.
        assert!(!is_covered_by_live_generation(&delete_item_with_metadata(
            Some(BRANCH_PRUNE_DELETE_METADATA)
        )));
        // Explicit false is honoured.
        assert!(!is_covered_by_live_generation(&delete_item_with_metadata(
            Some(r#"{"reason":"branch_prune","covered_by_live_generation":false}"#)
        )));
        // The flag must NOT be honoured on a non-prune delete — a watcher event
        // carrying it (or a forged one) must never bypass the guard.
        assert!(!is_covered_by_live_generation(&delete_item_with_metadata(
            Some(r#"{"reason":"watcher","covered_by_live_generation":true}"#)
        )));
        // Absent / malformed / wrong-typed metadata → conservative false.
        assert!(!is_covered_by_live_generation(&delete_item_with_metadata(
            None
        )));
        assert!(!is_covered_by_live_generation(&delete_item_with_metadata(
            Some("not json")
        )));
        assert!(!is_covered_by_live_generation(&delete_item_with_metadata(
            Some(r#"{"reason":"branch_prune","covered_by_live_generation":"yes"}"#)
        )));
    }

    // ── #224: shadowed-holder strip policy ─────────────────────────────────

    #[test]
    fn shadowed_strip_policy_defaults_to_dry_for_anything_unknown() {
        assert_eq!(
            parse_shadowed_strip_policy(Some("on")),
            ShadowedStripPolicy::On
        );
        assert_eq!(
            parse_shadowed_strip_policy(Some("off")),
            ShadowedStripPolicy::Off
        );
        assert_eq!(
            parse_shadowed_strip_policy(Some(" on ")),
            ShadowedStripPolicy::On,
            "surrounding whitespace from a .env line must not silently disable the knob"
        );
        // Unset, empty, typo'd, or wrong-cased → dry. Deleting data must
        // require an exact, deliberate opt-in (same contract as the stage-3
        // covered-delete knob).
        assert_eq!(parse_shadowed_strip_policy(None), ShadowedStripPolicy::Dry);
        assert_eq!(
            parse_shadowed_strip_policy(Some("")),
            ShadowedStripPolicy::Dry
        );
        assert_eq!(
            parse_shadowed_strip_policy(Some("ON")),
            ShadowedStripPolicy::Dry
        );
        assert_eq!(
            parse_shadowed_strip_policy(Some("true")),
            ShadowedStripPolicy::Dry
        );
    }

    #[test]
    fn covered_delete_policy_defaults_to_dry_for_anything_unknown() {
        assert_eq!(
            parse_covered_delete_policy(Some("on")),
            CoveredDeletePolicy::On
        );
        assert_eq!(
            parse_covered_delete_policy(Some("off")),
            CoveredDeletePolicy::Off
        );
        assert_eq!(
            parse_covered_delete_policy(Some(" on ")),
            CoveredDeletePolicy::On,
            "surrounding whitespace from a .env line must not silently disable the knob"
        );
        // Unset, empty, typo'd, or wrong-cased → dry. Deleting data must require
        // an exact, deliberate opt-in.
        assert_eq!(parse_covered_delete_policy(None), CoveredDeletePolicy::Dry);
        assert_eq!(
            parse_covered_delete_policy(Some("")),
            CoveredDeletePolicy::Dry
        );
        assert_eq!(
            parse_covered_delete_policy(Some("ON")),
            CoveredDeletePolicy::Dry
        );
        assert_eq!(
            parse_covered_delete_policy(Some("true")),
            CoveredDeletePolicy::Dry
        );
    }
}
