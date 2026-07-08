//! Hashing utilities shared between daemon and CLI
//!
//! Provides canonical implementations for idempotency key generation
//! and content/file hashing using SHA256.

use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::fmt;
use std::path::Path;

use crate::queue_types::{ItemType, QueueOperation};

/// Generate a comprehensive idempotency key for unified queue deduplication
///
/// Creates a deterministic key from all relevant queue item attributes to prevent
/// duplicate processing. This function is cross-language compatible.
///
/// # Format
/// Input string: `{item_type}|{op}|{tenant_id}|{collection}|{payload_json}`
/// Output: SHA256 hash truncated to 32 hex characters
///
/// # Errors
/// Returns an error if tenant_id or collection is empty, or if the operation
/// is not valid for the given item type.
pub fn generate_idempotency_key(
    item_type: ItemType,
    op: QueueOperation,
    tenant_id: &str,
    collection: &str,
    payload_json: &str,
) -> Result<String, IdempotencyKeyError> {
    // Validate inputs
    if tenant_id.is_empty() {
        return Err(IdempotencyKeyError::EmptyTenantId);
    }
    if collection.is_empty() {
        return Err(IdempotencyKeyError::EmptyCollection);
    }
    if !op.is_valid_for(item_type) {
        return Err(IdempotencyKeyError::InvalidOperationForType {
            item_type,
            operation: op,
        });
    }

    // Construct canonical input string
    let input = format!(
        "{}|{}|{}|{}|{}",
        item_type, op, tenant_id, collection, payload_json
    );

    // Hash and truncate to 32 hex chars (16 bytes)
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let hash = hasher.finalize();

    let hash_hex: String = hash[..16].iter().map(|b| format!("{:02x}", b)).collect();

    Ok(hash_hex)
}

/// Generate a simple idempotency key (type:collection:hash format)
///
/// Uses format: `{item_type}:{collection}:{identifier_hash}`
/// Hash is truncated to 16 hex chars (8 bytes).
pub fn generate_simple_idempotency_key(
    item_type: ItemType,
    collection: &str,
    identifier: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(identifier.as_bytes());
    let hash = hasher.finalize();
    let hash_hex: String = hash[..8].iter().map(|b| format!("{:02x}", b)).collect();
    format!("{}:{}:{}", item_type, collection, hash_hex)
}

/// Errors that can occur during idempotency key generation
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdempotencyKeyError {
    /// tenant_id cannot be empty
    EmptyTenantId,
    /// collection cannot be empty
    EmptyCollection,
    /// The operation is not valid for the given item type
    InvalidOperationForType {
        item_type: ItemType,
        operation: QueueOperation,
    },
}

impl fmt::Display for IdempotencyKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IdempotencyKeyError::EmptyTenantId => write!(f, "tenant_id cannot be empty"),
            IdempotencyKeyError::EmptyCollection => write!(f, "collection cannot be empty"),
            IdempotencyKeyError::InvalidOperationForType {
                item_type,
                operation,
            } => {
                write!(
                    f,
                    "operation '{}' is not valid for item type '{}'",
                    operation, item_type
                )
            }
        }
    }
}

impl std::error::Error for IdempotencyKeyError {}

/// Normalize line endings to `\n` for stable, EOL-agnostic content identity.
///
/// Converts CRLF (`\r\n`) and lone CR (`\r`) to LF (`\n`). Returns the input
/// borrowed unchanged when it contains no `\r` (the common case), so this is
/// zero-copy for LF-only content.
///
/// This is what makes the SAME file committed with Windows (CRLF) and Unix (LF)
/// line endings map to ONE `base_point` / ONE Qdrant point instead of two — see
/// [`compute_file_hash`] and [`compute_base_point`].
pub fn normalize_line_endings(content: &str) -> Cow<'_, str> {
    if !content.contains('\r') {
        return Cow::Borrowed(content);
    }
    Cow::Owned(content.replace("\r\n", "\n").replace('\r', "\n"))
}

/// Streaming SHA256 of raw file bytes (no normalization). Used for large files
/// and as the binary fallback in [`compute_file_hash`].
fn hash_file_bytes_streaming(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let bytes_read = file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Compute the content-identity hash of a file.
///
/// For UTF-8 text the hash is taken over EOL-NORMALIZED content (see
/// [`normalize_line_endings`]), so a CRLF copy and an LF copy of the same file
/// share one hash — and therefore one `base_point` — collapsing the cross-EOL
/// duplication that otherwise stored the file twice and inflated search/grep
/// results. Non-UTF-8 (binary) files are hashed byte-for-byte, unchanged.
///
/// Files larger than `MAX_NORMALIZE_BYTES` are streamed raw (no normalization):
/// source/config/CI files — the ones with CRLF/LF churn — are far below the
/// cap, and buffering a large file just to normalize it is not worth the memory.
///
/// NOTE: this is the file's IDENTITY for base_point + change detection, not a
/// byte-integrity checksum. A pure line-ending change no longer counts as a
/// content change — by design, since the indexed text is identical.
pub fn compute_file_hash(path: &Path) -> std::io::Result<String> {
    const MAX_NORMALIZE_BYTES: u64 = 8 * 1024 * 1024; // 8 MiB
    let too_large = std::fs::metadata(path)
        .map(|m| m.len() > MAX_NORMALIZE_BYTES)
        .unwrap_or(true);
    if too_large {
        return hash_file_bytes_streaming(path);
    }

    let bytes = std::fs::read(path)?;
    match std::str::from_utf8(&bytes) {
        Ok(text) => {
            let normalized = normalize_line_endings(text);
            Ok(compute_content_hash(&normalized))
        }
        Err(_) => {
            // Binary file: hash the raw bytes (already in memory) unchanged.
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            Ok(format!("{:x}", hasher.finalize()))
        }
    }
}

/// Compute SHA256 hash of a string (for chunk content)
pub fn compute_content_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Normalize a file path for stable ID generation.
///
/// Ensures the same physical file always produces the same path string:
/// - Uses forward slashes
/// - Strips trailing slashes
pub fn normalize_path_for_id(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    normalized.trim_end_matches('/').to_string()
}

/// Compute the base point hash: `SHA256(tenant_id|relative_path|file_hash)[:32]`
///
/// The base point uniquely identifies a specific VERSION of a specific file's
/// content. It is shared across all chunks of that file version, across Qdrant
/// and the search DB, **and across branches** (Layer 2): identical content on
/// different branches maps to ONE base_point / ONE physical Qdrant point. The
/// set of branches that hold that content lives in the point's `branches[]`
/// payload — NOT in this identity hash. See docs/specs/21-cross-branch-dedup.md.
///
/// - `tenant_id`: derived from git remote URL hash (git) or path hash (non-git)
/// - `relative_path`: file path relative to project root (normalized)
/// - `file_hash`: SHA256 of file content or git blob SHA
///
/// NOTE: the TypeScript MCP server replicates this formula in
/// `src/typescript/mcp-server/src/utils/base-point.ts` — keep them in lockstep
/// (the golden-value test below guards Rust↔TS drift).
pub fn compute_base_point(tenant_id: &str, relative_path: &str, file_hash: &str) -> String {
    let normalized = normalize_path_for_id(relative_path);
    let input = format!("{}|{}|{}", tenant_id, normalized, file_hash);
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let hash = hasher.finalize();
    hash[..16].iter().map(|b| format!("{:02x}", b)).collect()
}

/// Compute a Qdrant point ID from a base point and chunk index.
///
/// Formula: `SHA256(base_point|chunk_index)[:32]`
///
/// Each chunk of a file version gets a unique, deterministic point ID.
pub fn compute_point_id(base_point: &str, chunk_index: u32) -> String {
    let input = format!("{}|{}", base_point, chunk_index);
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let hash = hasher.finalize();
    hash[..16].iter().map(|b| format!("{:02x}", b)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_idempotency_key_generation() {
        let key1 = generate_idempotency_key(
            ItemType::File,
            QueueOperation::Add,
            "proj_abc123",
            "my-project-code",
            r#"{"file_path":"/path/to/file.rs"}"#,
        )
        .unwrap();

        assert_eq!(key1.len(), 32);

        let key2 = generate_idempotency_key(
            ItemType::File,
            QueueOperation::Add,
            "proj_abc123",
            "my-project-code",
            r#"{"file_path":"/path/to/file.rs"}"#,
        )
        .unwrap();
        assert_eq!(key1, key2);

        let key3 = generate_idempotency_key(
            ItemType::File,
            QueueOperation::Add,
            "proj_abc123",
            "my-project-code",
            r#"{"file_path":"/path/to/other.rs"}"#,
        )
        .unwrap();
        assert_ne!(key1, key3);
    }

    #[test]
    fn test_idempotency_key_validation() {
        let result = generate_idempotency_key(
            ItemType::File,
            QueueOperation::Add,
            "",
            "my-collection",
            "{}",
        );
        assert_eq!(result, Err(IdempotencyKeyError::EmptyTenantId));

        let result =
            generate_idempotency_key(ItemType::File, QueueOperation::Add, "proj", "", "{}");
        assert_eq!(result, Err(IdempotencyKeyError::EmptyCollection));

        let result = generate_idempotency_key(
            ItemType::Collection,
            QueueOperation::Add,
            "proj",
            "col",
            "{}",
        );
        assert!(matches!(
            result,
            Err(IdempotencyKeyError::InvalidOperationForType { .. })
        ));
    }

    #[test]
    fn test_simple_idempotency_key() {
        let key1 =
            generate_simple_idempotency_key(ItemType::File, "my-collection", "/path/to/file.txt");
        let key2 =
            generate_simple_idempotency_key(ItemType::File, "my-collection", "/path/to/file.txt");
        assert_eq!(key1, key2);

        let key3 =
            generate_simple_idempotency_key(ItemType::File, "my-collection", "/path/to/other.txt");
        assert_ne!(key1, key3);
    }

    #[test]
    fn test_compute_content_hash() {
        let hash1 = compute_content_hash("hello world");
        let hash2 = compute_content_hash("hello world");
        let hash3 = compute_content_hash("different content");

        assert_eq!(hash1, hash2);
        assert_ne!(hash1, hash3);
        assert_eq!(hash1.len(), 64);
    }

    #[test]
    fn test_normalize_line_endings() {
        assert_eq!(&*normalize_line_endings("a\r\nb"), "a\nb");
        assert_eq!(&*normalize_line_endings("a\rb"), "a\nb");
        assert_eq!(&*normalize_line_endings("a\r\nb\r\n"), "a\nb\n");
        // Mixed CRLF + LF collapses to LF.
        assert_eq!(&*normalize_line_endings("a\r\nb\nc"), "a\nb\nc");
        // LF-only content is returned borrowed (zero-copy).
        assert!(matches!(normalize_line_endings("a\nb"), Cow::Borrowed(_)));
    }

    #[test]
    fn test_content_hash_is_eol_invariant() {
        // The identity hash of CRLF and LF versions of the same text must match
        // — this is what collapses the cross-EOL duplicate base_points that
        // otherwise stored the same file twice and inflated search/grep results.
        let crlf =
            compute_content_hash(&normalize_line_endings("fn main() {\r\n    ok();\r\n}\r\n"));
        let lf = compute_content_hash(&normalize_line_endings("fn main() {\n    ok();\n}\n"));
        assert_eq!(crlf, lf);

        // And the base_point built from those hashes is identical.
        let bp_crlf = compute_base_point("t", "src/main.rs", &crlf);
        let bp_lf = compute_base_point("t", "src/main.rs", &lf);
        assert_eq!(bp_crlf, bp_lf);
    }

    #[test]
    fn test_cross_language_compatibility() {
        // This test vector should produce the same hash in Rust, Python, and TypeScript
        // Input: "file|add|proj_abc123|my-project-code|{}"
        let key = generate_idempotency_key(
            ItemType::File,
            QueueOperation::Add,
            "proj_abc123",
            "my-project-code",
            "{}",
        )
        .unwrap();

        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(key.len(), 32);
    }

    #[test]
    fn test_normalize_path_for_id() {
        assert_eq!(normalize_path_for_id("src/main.rs"), "src/main.rs");
        assert_eq!(normalize_path_for_id("src\\main.rs"), "src/main.rs");
        assert_eq!(normalize_path_for_id("src/dir/"), "src/dir");
        assert_eq!(
            normalize_path_for_id("src\\dir\\file.rs"),
            "src/dir/file.rs"
        );
    }

    #[test]
    fn test_compute_base_point_deterministic() {
        let bp1 = compute_base_point("tenant_abc", "src/main.rs", "deadbeef");
        let bp2 = compute_base_point("tenant_abc", "src/main.rs", "deadbeef");
        assert_eq!(bp1, bp2);
        assert_eq!(bp1.len(), 32);
        assert!(bp1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_compute_base_point_different_file_hash() {
        let bp1 = compute_base_point("tenant_abc", "src/main.rs", "hash_v1");
        let bp2 = compute_base_point("tenant_abc", "src/main.rs", "hash_v2");
        assert_ne!(bp1, bp2);
    }

    /// Layer 2 invariant: branch is no longer part of the identity, so the same
    /// (tenant, path, content) maps to ONE base_point regardless of branch. The
    /// branch set lives in the point's `branch` array payload, not the hash.
    #[test]
    fn test_compute_base_point_is_branch_agnostic() {
        let bp1 = compute_base_point("tenant_abc", "src/main.rs", "deadbeef");
        let bp2 = compute_base_point("tenant_abc", "src/main.rs", "deadbeef");
        assert_eq!(bp1, bp2);
    }

    #[test]
    fn test_compute_base_point_different_path() {
        let bp1 = compute_base_point("tenant_abc", "src/main.rs", "deadbeef");
        let bp2 = compute_base_point("tenant_abc", "src/lib.rs", "deadbeef");
        assert_ne!(bp1, bp2);
    }

    #[test]
    fn test_compute_base_point_different_tenant() {
        let bp1 = compute_base_point("tenant_abc", "src/main.rs", "deadbeef");
        let bp2 = compute_base_point("tenant_xyz", "src/main.rs", "deadbeef");
        assert_ne!(bp1, bp2);
    }

    #[test]
    fn test_compute_base_point_path_normalization() {
        let bp1 = compute_base_point("t", "src/main.rs", "h");
        let bp2 = compute_base_point("t", "src\\main.rs", "h");
        assert_eq!(bp1, bp2);
    }

    #[test]
    fn test_compute_point_id_deterministic() {
        let bp = compute_base_point("tenant_abc", "src/main.rs", "deadbeef");
        let pid1 = compute_point_id(&bp, 0);
        let pid2 = compute_point_id(&bp, 0);
        assert_eq!(pid1, pid2);
        assert_eq!(pid1.len(), 32);
    }

    #[test]
    fn test_compute_point_id_different_chunk_index() {
        let bp = compute_base_point("tenant_abc", "src/main.rs", "deadbeef");
        let pid0 = compute_point_id(&bp, 0);
        let pid1 = compute_point_id(&bp, 1);
        let pid2 = compute_point_id(&bp, 2);
        assert_ne!(pid0, pid1);
        assert_ne!(pid1, pid2);
        assert_ne!(pid0, pid2);
    }

    #[test]
    fn test_compute_point_id_different_base_point() {
        let bp1 = compute_base_point("tenant_abc", "src/main.rs", "hash_v1");
        let bp2 = compute_base_point("tenant_abc", "src/main.rs", "hash_v2");
        let pid1 = compute_point_id(&bp1, 0);
        let pid2 = compute_point_id(&bp2, 0);
        assert_ne!(pid1, pid2);
    }

    #[test]
    fn test_base_point_cross_language_test_vector() {
        // Test vector for cross-language parity (Rust ↔ TypeScript).
        // TypeScript tests in tests/utils/base-point.test.ts must match these
        // values. Layer 2 formula: SHA256(tenant_id|relative_path|file_hash).
        let bp = compute_base_point("test_tenant", "src/example.rs", "abc123hash");
        assert_eq!(bp, "d08103c2d8f553544dabeb4737fd32b4");

        let pid = compute_point_id(&bp, 0);
        assert_eq!(pid, "72deff25f27560ca51dcab1c6b373b8b");
    }
}
