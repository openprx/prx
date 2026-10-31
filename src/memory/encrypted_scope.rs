//! Memory decorator for one ramflux account namespace.
//!
//! Content is sealed with the account DEK before it reaches the inner backend,
//! and opened on the way out. The vector index therefore never sees plaintext.
//! If a backend cannot store an encrypted vector, this namespace simply is not
//! indexed: [`EncryptedScope::supports_document_ingest`] is false, and the
//! stored text is ciphertext. The sqlite and postgres backends themselves are
//! unchanged.

use crate::channels::ramflux::crypto::{AccountCrypto, CryptoError};
use crate::memory::traits::{Memory, MemoryCategory, MemoryEntry};
use async_trait::async_trait;
use base64::Engine;
use std::sync::Arc;

/// Encrypting view of `inner` for a single `agent_account_id`.
pub struct EncryptedScope {
    inner: Arc<dyn Memory>,
    crypto: Arc<AccountCrypto>,
    account_id: String,
}

impl EncryptedScope {
    #[must_use]
    pub fn new(inner: Arc<dyn Memory>, crypto: Arc<AccountCrypto>, account_id: impl Into<String>) -> Self {
        Self {
            inner,
            crypto,
            account_id: account_id.into(),
        }
    }

    fn conversation<'a>(session_id: Option<&'a str>) -> &'a str {
        session_id.unwrap_or("default")
    }

    fn seal(&self, session_id: Option<&str>, content: &str) -> Result<String, CryptoError> {
        let blob = self
            .crypto
            .seal(&self.account_id, Self::conversation(session_id), content.as_bytes())?;
        Ok(base64::engine::general_purpose::STANDARD.encode(blob))
    }

    fn open_entry(&self, entry: &mut MemoryEntry) -> Result<(), CryptoError> {
        let Ok(blob) = base64::engine::general_purpose::STANDARD.decode(entry.content.as_bytes()) else {
            return Ok(());
        };
        if !blob.starts_with(b"RF1") {
            return Ok(());
        }
        let plain = self
            .crypto
            .open(&self.account_id, Self::conversation(entry.session_id.as_deref()), &blob)?;
        entry.content = String::from_utf8(plain).map_err(|_| CryptoError::AadMismatch)?;
        Ok(())
    }
}

#[async_trait]
impl Memory for EncryptedScope {
    fn name(&self) -> &str {
        "ramflux-encrypted"
    }

    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let sealed = self.seal(session_id, content)?;
        self.inner.store(key, &sealed, category, session_id).await
    }

    async fn recall(&self, query: &str, limit: usize, session_id: Option<&str>) -> anyhow::Result<Vec<MemoryEntry>> {
        let mut entries = self.inner.recall(query, limit, session_id).await?;
        for entry in &mut entries {
            self.open_entry(entry)?;
        }
        Ok(entries)
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        let mut entry = self.inner.get(key).await?;
        if let Some(entry) = entry.as_mut() {
            self.open_entry(entry)?;
        }
        Ok(entry)
    }

    async fn list(
        &self,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let mut entries = self.inner.list(category, session_id).await?;
        for entry in &mut entries {
            self.open_entry(entry)?;
        }
        Ok(entries)
    }

    async fn forget(&self, key: &str) -> anyhow::Result<bool> {
        self.inner.forget(key).await
    }

    async fn count(&self) -> anyhow::Result<usize> {
        self.inner.count().await
    }

    async fn health_check(&self) -> bool {
        self.inner.health_check().await
    }

    fn supports_document_ingest(&self) -> bool {
        false
    }
}

/// Factory used only for ramflux account namespaces.
#[must_use]
pub fn create_ramflux_scoped_memory(
    inner: Arc<dyn Memory>,
    crypto: Arc<AccountCrypto>,
    account_id: impl Into<String>,
) -> Arc<dyn Memory> {
    Arc::new(EncryptedScope::new(inner, crypto, account_id))
}
