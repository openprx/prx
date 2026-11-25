//! The error of the session transcript read on a backend that does not have it.

/// `Memory::session_transcript_rows` is not implemented by this backend. The
/// transcript loader answers it by reading the display projection instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SessionTranscriptUnsupported;

impl std::fmt::Display for SessionTranscriptUnsupported {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the session transcript read is unsupported by this memory backend")
    }
}

impl std::error::Error for SessionTranscriptUnsupported {}
