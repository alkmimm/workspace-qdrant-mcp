//! Error types for the unified queue processor.

use thiserror::Error;

use crate::document_processor::DocumentProcessorError;

/// Unified queue processor errors
#[derive(Error, Debug)]
pub enum UnifiedProcessorError {
    #[error("Queue operation failed: {0}")]
    QueueOperation(String),

    #[error("Processing failed: {0}")]
    ProcessingFailed(String),

    /// The file's content cannot be extracted, and never will be: a property
    /// of its bytes (see [`DocumentProcessorError::is_deterministic`]).
    /// Classified `permanent_data` — no retry, no resurrection.
    #[error("Unreadable content: {0}")]
    UnreadableContent(String),

    #[error("Storage error: {0}")]
    Storage(String),

    #[error("Embedding error: {0}")]
    Embedding(String),

    /// Embedding subsystem is within its backoff window after a failed init.
    /// The item should be re-leased without incrementing its retry count.
    #[error("Embedding subsystem temporarily unavailable: {0}")]
    EmbeddingUnavailable(String),

    #[error("File not found: {0}")]
    FileNotFound(String),

    #[error("Invalid payload: {0}")]
    InvalidPayload(String),

    #[error("Unsupported operation: {0}")]
    UnsupportedOperation(String),

    #[error("Shutdown requested")]
    ShutdownRequested,
}

impl UnifiedProcessorError {
    /// A document-processing failure as a queue failure. Until 2026-10-08
    /// every one became `ProcessingFailed`, which the classifier reads as
    /// transient: a CSV with invalid UTF-8 or a binary `.doc` was retried
    /// three times and then resurrected, failing identically each time.
    pub(crate) fn from_document(e: DocumentProcessorError) -> Self {
        if e.is_deterministic() {
            Self::UnreadableContent(e.to_string())
        } else {
            Self::ProcessingFailed(e.to_string())
        }
    }
}

/// Result type for unified processor operations
pub type UnifiedProcessorResult<T> = Result<T, UnifiedProcessorError>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unified_queue_processor::UnifiedQueueProcessor;

    fn category(e: DocumentProcessorError) -> &'static str {
        UnifiedQueueProcessor::classify_error(&UnifiedProcessorError::from_document(e))
    }

    /// tecsul 2026-10-07: two CSVs with invalid UTF-8 and a binary `.doc`
    /// were each tried three times as `[transient_infrastructure]`.
    #[test]
    fn content_that_cannot_be_extracted_is_a_permanent_failure() {
        for e in [
            DocumentProcessorError::CsvExtraction("invalid utf-8 in field 0".into()),
            DocumentProcessorError::BinaryFile("eagle/howto.doc".into()),
            DocumentProcessorError::PdfExtraction("corrupt xref".into()),
            DocumentProcessorError::UnsupportedFormat("x.xyz".into()),
            DocumentProcessorError::EmptyFile("a.txt".into()),
        ] {
            let message = e.to_string();
            assert_eq!(category(e), "permanent_data", "{message}");
        }
    }

    #[test]
    fn failures_that_can_heal_are_still_retried() {
        for e in [
            DocumentProcessorError::Io(std::io::Error::other("disk busy")),
            DocumentProcessorError::OcrError("tesseract not found".into()),
            DocumentProcessorError::TaskError("join error".into()),
        ] {
            let message = e.to_string();
            assert_eq!(category(e), "transient_infrastructure", "{message}");
        }
        // A missing file keeps its "gone" classification (by message).
        assert_eq!(
            category(DocumentProcessorError::FileNotFound("x.rs".into())),
            "permanent_gone"
        );
    }
}
