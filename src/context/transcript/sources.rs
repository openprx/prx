//! Provenance of an attachment: what its recalled text was derived from.
//!
//! The attachment of a user message (see [`super::Attachment`]) quotes stored
//! rows: earlier events of the session and of the workspace, recalled
//! memories and document chunks. Their ids are written beside the text so a
//! later delete or forget of any of them can find the attachments that still
//! quote it and drop the quote (`memory/sqlite/rf_attachments.rs`).

/// Schema version of the attachment row payload. Version 1 rows carry no
/// `sources` and stay readable; they are matched by text on delete.
pub(crate) const ATTACHMENT_SCHEMA_VERSION: u32 = 2;

/// The ids an attachment's recalled text was built from.
///
/// The lists may name more rows than the stored text finally quotes (a clipped
/// preamble keeps every id it considered), never fewer.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttachmentSources {
    /// Ids of the memories quoted in the memory context.
    #[serde(default)]
    pub memories: Vec<String>,
    /// Event ids of the session and shared-workspace messages quoted.
    #[serde(default)]
    pub events: Vec<String>,
    /// Ids of the documents quoted in the document evidence.
    #[serde(default)]
    pub documents: Vec<String>,
}
