//! Chunk splitting and boundary detection.
//!
//! Handles splitting oversized semantic chunks into smaller fragments with
//! overlap, and provides text-based fallback chunking for unsupported languages.

use std::path::Path;

use tracing::warn;

use crate::tokenizer::{estimated_token_count, ModelTokenizer};
use crate::tree_sitter::types::SemanticChunk;

/// Overlap size for fragmented chunks (in characters).
pub(super) const FRAGMENT_OVERLAP: usize = 500;

/// Round a byte index down to the nearest UTF-8 char boundary.
/// Prevents panics when slicing `&str` at byte offsets computed from
/// arithmetic on `content.len()` (which counts bytes, not chars).
pub(crate) fn safe_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Handle chunks that exceed the maximum size by splitting them.
///
/// When `tokenizer` is `Some`, real token counts from the embedding model's
/// tokenizer drive both the oversize gate and a post-split sanity check.
/// When `None`, falls back to the `content.len() / 4` heuristic — fine for
/// tests and CI without the HF model cache, but the real tokenizer should
/// always be passed in production.
pub(super) fn handle_oversized_chunks(
    chunks: Vec<SemanticChunk>,
    _source: &str,
    max_chunk_size: usize,
    tokenizer: Option<&ModelTokenizer>,
) -> Vec<SemanticChunk> {
    let mut result = Vec::new();

    for chunk in chunks {
        if estimated_token_count(&chunk.content, tokenizer) > max_chunk_size {
            // Split into fragments with overlap
            let fragments = split_chunk_with_overlap(&chunk, max_chunk_size);
            // Post-split sanity check: char-based splitting may still produce
            // fragments that exceed the real-token budget. Warn (don't fail)
            // so downstream embedding doesn't silently truncate.
            if let Some(tk) = tokenizer {
                for frag in &fragments {
                    if let Ok(n) = tk.count_tokens(&frag.content) {
                        if n > max_chunk_size {
                            warn!(
                                file = %frag.file_path,
                                symbol = %frag.symbol_name,
                                fragment_index = ?frag.fragment_index,
                                tokens = n,
                                budget = max_chunk_size,
                                "Fragment still exceeds token budget after char-based split"
                            );
                        }
                    }
                }
            }
            result.extend(fragments);
        } else {
            result.push(chunk);
        }
    }

    result
}

/// Split a large chunk into smaller fragments with overlap.
///
/// All byte-offset arithmetic uses `safe_char_boundary` to avoid panics
/// on multi-byte UTF-8 characters (e.g., box-drawing chars, CJK, emoji).
fn split_chunk_with_overlap(chunk: &SemanticChunk, max_chunk_size: usize) -> Vec<SemanticChunk> {
    let content = &chunk.content;
    let target_size = max_chunk_size * 4; // Convert tokens to chars (approximate)

    if content.len() <= target_size {
        return vec![chunk.clone()];
    }

    let mut fragments = Vec::new();
    let mut start = 0;
    // Use saturating_sub to avoid overflow when target_size < FRAGMENT_OVERLAP
    let step_size = target_size.saturating_sub(FRAGMENT_OVERLAP).max(1);
    let total_fragments = (content.len() + step_size - 1) / step_size;

    let mut fragment_index = 0;
    while start < content.len() {
        let window_end = safe_char_boundary(content, (start + target_size).min(content.len()));

        // Prefer to break at a line boundary, but only when that boundary still
        // covers at least one full stride. A long run without line breaks (e.g.
        // minified JS, single-line JSON, lockfiles, base64 blobs) would otherwise
        // make `find_line_boundary` snap `actual_end` back to the same early
        // newline every iteration; combined with the old overlap pull-back the
        // loop would never advance `start`, allocating fragments without bound
        // until the process exhausts memory. Falling back to `window_end`
        // guarantees `actual_end >= start + step_size`, keeping the stride below
        // strictly forward with no coverage gap.
        let line_end = find_line_boundary(content, start, window_end);
        let min_end = (start + step_size).min(content.len());
        let actual_end = if line_end >= min_end {
            line_end
        } else {
            window_end
        };

        let fragment_content = &content[start..actual_end];

        // Calculate line numbers for this fragment
        let lines_before_start = content[..start].matches('\n').count();
        let lines_in_fragment = fragment_content.matches('\n').count();
        let fragment_start_line = chunk.start_line + lines_before_start;
        let fragment_end_line = fragment_start_line + lines_in_fragment;

        let mut fragment = SemanticChunk::new(
            chunk.chunk_type,
            &chunk.symbol_name,
            fragment_content,
            fragment_start_line,
            fragment_end_line,
            &chunk.language,
            &chunk.file_path,
        )
        .as_fragment(fragment_index, total_fragments);

        // Preserve metadata from original chunk
        if let Some(ref parent) = chunk.parent_symbol {
            fragment = fragment.with_parent(parent.clone());
        }
        if fragment_index == 0 {
            // Only the first fragment carries the function-level metadata:
            // docstring, signature, and CALLS. All fragments share ONE graph
            // node_id (same symbol_name|file_path|type), so the graph extractor
            // builds the CALLS edges once from fragment 0. Without carrying
            // `calls` here, an oversized (fragmented) function — e.g. a large
            // match-dispatch handler — gets ZERO outgoing CALLS edges in the
            // graph, so `relations`/`impact`/`usages` under-report its callees.
            // (signature still drives USES_TYPE; parent_symbol drives CONTAINS.)
            if let Some(ref doc) = chunk.docstring {
                fragment = fragment.with_docstring(doc.clone());
            }
            if let Some(ref sig) = chunk.signature {
                fragment = fragment.with_signature(sig.clone());
            }
            if !chunk.calls.is_empty() {
                fragment = fragment.with_calls(chunk.calls.clone());
            }
        }

        fragments.push(fragment);

        fragment_index += 1;

        if actual_end >= content.len() {
            break;
        }
        // Stride forward by `step_size`, which yields the intended
        // `FRAGMENT_OVERLAP`-char overlap between consecutive fragments
        // (step_size = target_size - FRAGMENT_OVERLAP). Because `actual_end` is
        // guaranteed `>= start + step_size` above, this never leaves a gap.
        let next_start = safe_char_boundary(content, start + step_size);
        // Snap the next fragment's start back to the beginning of the line it
        // lands in, so a fragment begins at a line boundary instead of in the
        // middle of a token — a stride landing inside `configKey` would
        // otherwise open the fragment at `gKey`. Only the fragment *end* used
        // to honor line boundaries; the start did not. Accept the line anchor
        // only when it is strictly ahead of `start` AND pulls back no more than
        // the overlap budget: a long run without interior newlines (minified
        // JS, single-line JSON, lockfiles, base64 blobs) has no usable boundary
        // there, so fall back to the raw char-boundary stride — mirroring the
        // `window_end` fallback used for the end above and preserving forward
        // progress. `next_start` is the floor, so anchoring only ever grows the
        // overlap and never opens a coverage gap. The final `actual_end`
        // fallback covers the degenerate `step_size == 1` case where
        // char-boundary rounding could otherwise stall `start` and loop forever.
        let line_start = line_start_at(content, next_start);
        start = if line_start > start && next_start - line_start <= FRAGMENT_OVERLAP {
            line_start
        } else if next_start > start {
            next_start
        } else {
            actual_end
        };
    }

    fragments
}

/// Find the best line boundary for splitting content.
///
/// If `end` is not at end-of-content, search backwards from `end` for a
/// newline within the `[start..end]` range, returning the position after it.
/// Falls back to `end` if no newline is found.
fn find_line_boundary(content: &str, start: usize, end: usize) -> usize {
    if end < content.len() {
        content[start..end]
            .rfind('\n')
            .map(|i| start + i + 1)
            .unwrap_or(end)
    } else {
        end
    }
}

/// Byte index of the start of the line containing `index` — the position just
/// after the preceding `'\n'`, or 0 when there is none. `'\n'` is single-byte
/// ASCII, so the returned index is always a valid UTF-8 char boundary.
fn line_start_at(content: &str, index: usize) -> usize {
    let index = index.min(content.len());
    content[..index].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

/// Create text chunks as a fallback for unsupported languages or parse errors.
pub fn text_chunk_fallback(
    source: &str,
    file_path: &Path,
    max_chunk_size: usize,
) -> Vec<SemanticChunk> {
    let language = file_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("text");
    let file_path_str = file_path.to_string_lossy().to_string();
    let target_chars = max_chunk_size * 4; // Approximate chars per token

    if source.len() <= target_chars {
        // Single chunk for small files
        let end_line = source.matches('\n').count() + 1;
        return vec![SemanticChunk::text(
            source,
            1,
            end_line,
            language,
            file_path_str,
        )];
    }

    // Split into chunks by lines
    let mut chunks = Vec::new();
    let mut current_chunk = String::new();
    let mut chunk_start_line = 1;
    let mut current_line = 1;

    for line in source.lines() {
        let would_exceed = (current_chunk.len() + line.len() + 1) > target_chars;

        if would_exceed && !current_chunk.is_empty() {
            // Emit current chunk
            let end_line = current_line - 1;
            chunks.push(SemanticChunk::text(
                &current_chunk,
                chunk_start_line,
                end_line,
                language,
                &file_path_str,
            ));
            current_chunk.clear();
            chunk_start_line = current_line;
        }

        if !current_chunk.is_empty() {
            current_chunk.push('\n');
        }
        current_chunk.push_str(line);
        current_line += 1;
    }

    // Emit final chunk
    if !current_chunk.is_empty() {
        let end_line = current_line - 1;
        chunks.push(SemanticChunk::text(
            &current_chunk,
            chunk_start_line,
            end_line,
            language,
            &file_path_str,
        ));
    }

    chunks
}
