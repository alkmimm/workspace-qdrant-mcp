//! gRPC status mapping for text-search failures.
//!
//! Extracted from `text_search_service.rs` (which is over the 500-line file
//! limit) so the error-classification section touched by the invalid_argument
//! change lives in its own module.

use tonic::Status;
use tracing::{debug, error};
use workspace_qdrant_core::SearchDbError;

/// Map a search failure to the right gRPC status.
///
/// A bad regex or bad `path_glob` is CALLER input, not a server fault, so it
/// must surface as `invalid_argument` (with a RE2 hint) — mirroring how the rest
/// of the daemon validates input — rather than `internal`, which implies a bug
/// and reads as retryable. Every other `SearchDbError` (DB/IO/migration) stays
/// `internal`. Shared by `search` and `count_matches` via `execute_or_cached`.
pub(crate) fn map_search_error(e: SearchDbError) -> Status {
    match e {
        SearchDbError::InvalidPattern(msg) => {
            // Log at debug, not error: a malformed client pattern is not a server
            // fault (so it must not pollute error-level alerting), but a storm of
            // them is still worth being visible in the daemon log.
            debug!("TextSearch rejected invalid pattern: {msg}");
            Status::invalid_argument(format!(
                "Invalid search pattern: {msg}. The FTS engine is RE2-based: look-around \
                 (e.g. (?<!x)) and backreferences are unsupported; use \\b or a fixed \
                 pattern. For a path filter, pathGlob must match the absolute path and \
                 multi-segment literals must be adjacent."
            ))
        }
        other => {
            error!("TextSearch failed: {:?}", other);
            Status::internal(format!("Search failed: {other}"))
        }
    }
}
