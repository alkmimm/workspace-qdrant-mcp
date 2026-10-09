//! Plain text and code file extraction with encoding detection.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use chardet::detect;
use encoding_rs::Encoding;
use tracing::warn;

use crate::document_processor::types::{DocumentProcessorError, DocumentProcessorResult};

/// A file's bytes decoded to text (see [`decode_text`]).
pub(crate) struct DecodedText {
    pub(crate) text: String,
    /// `utf-8`, or the upper-cased label of the detected encoding.
    pub(crate) encoding: String,
    /// chardet's confidence, when the encoding was detected.
    pub(crate) confidence: Option<f32>,
    /// The detected encoding failed too: `text` is lossy UTF-8.
    pub(crate) lossy: bool,
}

/// Decode a text file's bytes — the ONE decoding every index uses, so the
/// chunks (vectors, payload) and the FTS5 lines hold the same text. Until
/// 2026-10-08 the FTS5 reader required UTF-8: every ISO-8859-1 / UTF-16 file
/// was in semantic search and silently missing from `grep`.
///
/// `None` = binary content.
pub(crate) fn decode_text(bytes: &[u8]) -> Option<DecodedText> {
    // Try UTF-8 first (the overwhelming majority of source/text files).
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Some(DecodedText {
            text: text.to_string(),
            encoding: "utf-8".to_string(),
            confidence: None,
            lossy: false,
        });
    }

    // Reject binary content BEFORE the legacy/lossy decodes below. chardet will
    // happily map an executable/image to a single-byte charset (e.g. latin-1)
    // that "decodes without errors", and the lossy fallback would turn it into
    // garbage "text" — which then feeds the chunker a multi-hundred-KB blob
    // with no line breaks (a Mach-O `bookshelf` test fixture did exactly this
    // and drove memexd to an OOM). Heuristic (same as git): a NUL byte in the
    // first 8 KiB means binary. UTF-16/32 text legitimately contains NUL bytes,
    // so skip the gate when a UTF BOM is present — the encoding path below
    // decodes those correctly.
    const BINARY_SNIFF_LEN: usize = 8192;
    let has_utf_bom = bytes.starts_with(&[0xFF, 0xFE]) // UTF-16 LE / UTF-32 LE
        || bytes.starts_with(&[0xFE, 0xFF]) // UTF-16 BE
        || bytes.starts_with(&[0x00, 0x00, 0xFE, 0xFF]); // UTF-32 BE
    if !has_utf_bom && bytes[..bytes.len().min(BINARY_SNIFF_LEN)].contains(&0) {
        return None;
    }

    // Detect the encoding with chardet and decode with it.
    let (charset, confidence, _) = detect(bytes);
    let encoding = charset.to_uppercase();
    if let Some(detected) = Encoding::for_label(encoding.as_bytes()) {
        let (decoded, _, had_errors) = detected.decode(bytes);
        if !had_errors {
            return Some(DecodedText {
                text: decoded.into_owned(),
                encoding,
                confidence: Some(confidence),
                lossy: false,
            });
        }
    }

    // Fallback: decode as UTF-8 with lossy conversion.
    Some(DecodedText {
        text: String::from_utf8_lossy(bytes).into_owned(),
        encoding,
        confidence: Some(confidence),
        lossy: true,
    })
}

/// Read a file's text for an index, decoded by [`decode_text`]. Binary
/// content is an `InvalidData` error: the caller leaves the file out.
pub(crate) async fn read_text(path: &Path) -> std::io::Result<String> {
    let bytes = tokio::fs::read(path).await?;
    decode_text(&bytes)
        .map(|decoded| decoded.text)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "binary content (NUL byte, no UTF BOM)",
            )
        })
}

/// Extract text file with encoding detection
pub fn extract_text_with_encoding(
    file_path: &Path,
) -> DocumentProcessorResult<(String, HashMap<String, String>)> {
    let mut metadata = HashMap::new();
    metadata.insert("source_format".to_string(), "text".to_string());

    let mut file = File::open(file_path)?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;

    let Some(decoded) = decode_text(&buffer) else {
        return Err(DocumentProcessorError::BinaryFile(
            file_path.display().to_string(),
        ));
    };
    metadata.insert("encoding".to_string(), decoded.encoding);
    if let Some(confidence) = decoded.confidence {
        metadata.insert("encoding_confidence".to_string(), confidence.to_string());
    }
    if decoded.lossy {
        warn!(
            "Encoding detection failed for {:?}, using lossy UTF-8",
            file_path
        );
        metadata.insert("encoding_fallback".to_string(), "true".to_string());
    }
    Ok((decoded.text, metadata))
}

/// Extract code file with language metadata
pub fn extract_code(
    file_path: &Path,
    language: &str,
) -> DocumentProcessorResult<(String, HashMap<String, String>)> {
    let mut metadata = HashMap::new();
    metadata.insert("source_format".to_string(), "code".to_string());
    metadata.insert("language".to_string(), language.to_string());

    let (text, mut text_metadata) = extract_text_with_encoding(file_path)?;

    // Merge metadata
    for (k, v) in text_metadata.drain() {
        metadata.entry(k).or_insert(v);
    }

    let line_count = text.lines().count();
    metadata.insert("line_count".to_string(), line_count.to_string());

    Ok((text, metadata))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document_processor::redaction::read_for_index;
    use wqm_common::hashing::normalize_line_endings;

    /// Portuguese prose, enough of it for chardet to pick a Latin encoding
    /// (tecsul's `Error/404.jsp` is exactly this shape). á, é and ç sit at the
    /// same code points in every Latin-1 family charset, so the assertions
    /// hold whichever of them is detected.
    const PT: &str = "<html><body>\r\n<h1>Página não encontrada</h1>\r\n\
        <p>O endereço solicitado não existe. Verifique a configuração e \
        tente novamente; se o erro persistir, é necessário contatar o \
        suporte técnico da aplicação.</p>\r\n</body></html>\r\n";

    fn latin1(text: &str) -> Vec<u8> {
        let (bytes, _, unmappable) = encoding_rs::WINDOWS_1252.encode(text);
        assert!(!unmappable);
        bytes.into_owned()
    }

    fn utf16le_with_bom(text: &str) -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xFE];
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn utf8_text_is_read_as_is() {
        let decoded = decode_text(PT.as_bytes()).expect("UTF-8 is text");
        assert_eq!(decoded.text, PT);
        assert_eq!(decoded.encoding, "utf-8");
        assert!(!decoded.lossy);
    }

    #[test]
    fn latin1_text_is_decoded_not_rejected() {
        let bytes = latin1(PT);
        assert!(
            std::str::from_utf8(&bytes).is_err(),
            "the fixture is not UTF-8"
        );
        let decoded = decode_text(&bytes).expect("Latin-1 is text");
        assert!(!decoded.lossy, "detected as {}", decoded.encoding);
        for word in ["Página", "endereço", "técnico", "é necessário"] {
            assert!(
                decoded.text.contains(word),
                "{word} missing ({}): {:?}",
                decoded.encoding,
                decoded.text
            );
        }
    }

    #[test]
    fn utf16_with_a_bom_is_decoded() {
        let decoded = decode_text(&utf16le_with_bom(PT)).expect("UTF-16 is text");
        assert!(!decoded.lossy, "detected as {}", decoded.encoding);
        assert_eq!(decoded.text, PT);
    }

    #[test]
    fn binary_content_is_not_text() {
        let mut elf = b"\x7fELF\x02\x01\x01\0\0\0\0\0".to_vec();
        elf.extend(std::iter::repeat_n(0xAB, 64));
        assert!(decode_text(&elf).is_none());
    }

    /// The FTS5 reader must see what the chunker sees. Until 2026-10-08 it
    /// required UTF-8: five files across four tenants (tecsul's ISO-8859-1
    /// `404.jsp` and `Worker.java`, a Latin-1 readme, two UTF-16 tool outputs)
    /// were in semantic search and missing from `grep`.
    #[tokio::test]
    async fn the_fts_reader_reads_what_the_chunker_reads() {
        let dir = tempfile::tempdir().unwrap();
        for (name, bytes) in [("404.jsp", latin1(PT)), ("out.txt", utf16le_with_bom(PT))] {
            let path = dir.path().join(name);
            std::fs::write(&path, &bytes).unwrap();
            let (fts, _) = read_for_index(&path).await.expect(name);
            let (chunker, _) = extract_text_with_encoding(&path).unwrap();
            assert_eq!(
                fts.as_str(),
                normalize_line_endings(&chunker).as_ref(),
                "{name}"
            );
            assert!(fts.contains("endereço") && !fts.contains('\r'), "{name}");
        }
        // Binary content: NUL bytes AND invalid UTF-8 (0xAB). NUL alone is
        // valid UTF-8, and valid UTF-8 is read as text, as it always was.
        let blob = dir.path().join("blob.bin");
        std::fs::write(&blob, b"\x7fELF\x02\x01\x01\0\0\0\0\0\xab\xab\xab").unwrap();
        let err = read_for_index(&blob).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
