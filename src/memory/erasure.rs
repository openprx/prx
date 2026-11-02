//! Backend contract for transactional, message-owned retention.

use std::sync::Arc;

/// Opaque, hashed identifiers. No message text belongs in this record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErasureOrigin {
    pub account: String,
    pub conversation: String,
    pub message: String,
}

impl ErasureOrigin {
    #[must_use]
    pub fn new(account: &str, conversation: &str, message: &str) -> Self {
        Self {
            account: crate::security::ramflux_aead::id_hash(account),
            conversation: crate::security::ramflux_aead::id_hash(conversation),
            message: crate::security::ramflux_aead::id_hash(message),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("memory capability unsupported: {0}")]
pub struct Unsupported(pub &'static str);

/// Synchronous object operations share the database writer and transaction
/// boundary with the erase index. Implementations must never publish an
/// unindexed object. Channel callers already perform synchronous local IO.
pub trait ErasureStore: Send + Sync {
    fn put_blob(&self, origin: &ErasureOrigin, kind: &str, key: &str, bytes: &[u8]) -> anyhow::Result<()>;
    fn get_blob(&self, account: &str, conversation: &str, kind: &str, key: &str) -> anyhow::Result<Option<Vec<u8>>>;
    fn list_blobs(&self, account: &str, conversation: &str, kind: &str) -> anyhow::Result<Vec<(String, Vec<u8>)>>;
    fn remove_blob(&self, account: &str, conversation: &str, kind: &str, key: &str) -> anyhow::Result<()>;
    fn begin_delete(&self, origin: &ErasureOrigin) -> anyhow::Result<()>;
    fn is_deleted(&self, origin: &ErasureOrigin) -> anyhow::Result<bool>;
    fn origins(&self, account: &str, conversation: Option<&str>) -> anyhow::Result<Vec<ErasureOrigin>>;
    fn run_ids(&self, origin: &ErasureOrigin) -> anyhow::Result<Vec<String>>;
    /// Delete indexed data atomically. Keep the index until checkpoint succeeds.
    fn erase_rows(&self, origin: &ErasureOrigin) -> anyhow::Result<()>;
    /// Close the write fence and remove the index after the checkpoint.
    fn finish_delete(&self, origin: &ErasureOrigin) -> anyhow::Result<()>;
}

pub type SharedErasureStore = Arc<dyn ErasureStore>;
