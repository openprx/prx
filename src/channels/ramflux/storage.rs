//! Indexed content objects and encrypted recovery metadata.
//!
//! Production content uses the transactional database store and ciphertext
//! from [`super::crypto`]. Legacy file names use collision-resistant hashed ids.
//! A first open deletes any pre-existing plaintext left in this directory.

use super::crypto::{self, AccountCrypto, CryptoError};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    #[error("{0}")]
    Io(String),
}

/// Channel files under `<workspace>/ramflux`.
pub struct ChannelStore {
    root: PathBuf,
    backend: parking_lot::Mutex<Option<crate::memory::erasure::SharedErasureStore>>,
}

impl ChannelStore {
    /// Open the store and delete leftover plaintext from before encryption.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the directory cannot be created.
    pub fn open(workspace: &Path) -> Result<Self, StorageError> {
        let root = workspace.join("ramflux");
        std::fs::create_dir_all(&root).map_err(|error| StorageError::Io(error.to_string()))?;
        delete_legacy_plaintext(&root);
        Ok(Self {
            root,
            backend: parking_lot::Mutex::new(None),
        })
    }

    pub fn attach(&self, backend: crate::memory::erasure::SharedErasureStore) {
        *self.backend.lock() = Some(backend);
    }

    pub fn detach(&self) {
        self.backend.lock().take();
    }

    pub fn backend(&self) -> anyhow::Result<crate::memory::erasure::SharedErasureStore> {
        self.backend
            .lock()
            .clone()
            .ok_or_else(|| crate::memory::erasure::Unsupported("transactional erasure").into())
    }

    fn put_content(
        &self,
        origin: crate::memory::erasure::ErasureOrigin,
        kind: &str,
        key: &str,
        path: &Path,
        bytes: &[u8],
    ) -> Result<(), StorageError> {
        let backend = self.backend.lock().clone();
        backend.map_or_else(
            || write_file(path, bytes),
            |backend| {
                backend
                    .put_blob(&origin, kind, &name(key), bytes)
                    .map_err(|error| StorageError::Io(error.to_string()))
            },
        )
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Write one memory or message blob.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when sealing or the write fails.
    #[cfg(test)]
    pub fn put_message(
        &self,
        crypto: &AccountCrypto,
        account_id: &str,
        conversation_id: &str,
        message_id: &str,
        plaintext: &[u8],
    ) -> Result<PathBuf, StorageError> {
        let blob = crypto.seal(account_id, conversation_id, message_id, plaintext)?;
        let path = self.message_path(account_id, conversation_id, message_id);
        self.put_content(
            crate::memory::erasure::ErasureOrigin::new(account_id, conversation_id, message_id),
            "message",
            message_id,
            &path,
            &blob,
        )?;
        Ok(path)
    }

    /// Read one message blob. Tests use this to prove a round trip; erase does not.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the file is missing or cannot be opened.
    #[cfg(test)]
    pub fn get_message(
        &self,
        crypto: &AccountCrypto,
        account_id: &str,
        conversation_id: &str,
        message_id: &str,
    ) -> Result<Vec<u8>, StorageError> {
        let path = self.message_path(account_id, conversation_id, message_id);
        let backend = self.backend.lock().clone();
        let blob = if let Some(backend) = backend {
            let origin = crate::memory::erasure::ErasureOrigin::new(account_id, conversation_id, message_id);
            backend
                .get_blob(&origin.account, &origin.conversation, "message", &name(message_id))
                .map_err(|error| StorageError::Io(error.to_string()))?
                .ok_or_else(|| StorageError::Io("message missing".to_string()))?
        } else {
            std::fs::read(&path).map_err(|error| StorageError::Io(error.to_string()))?
        };
        Ok(crypto.open(account_id, conversation_id, message_id, &blob)?)
    }

    pub fn put_tool_event(
        &self,
        crypto: &AccountCrypto,
        account_id: &str,
        conversation_id: &str,
        event_id: &str,
        message_id: &str,
        plaintext: &[u8],
    ) -> Result<PathBuf, StorageError> {
        let blob = crypto.seal(account_id, conversation_id, event_id, plaintext)?;
        let path = self.event_path(account_id, conversation_id, event_id);
        self.put_content(
            crate::memory::erasure::ErasureOrigin::new(account_id, conversation_id, message_id),
            "event",
            event_id,
            &path,
            &blob,
        )?;
        Ok(path)
    }

    pub fn put_media(
        &self,
        crypto: &AccountCrypto,
        account_id: &str,
        conversation_id: &str,
        object_id: &str,
        message_id: &str,
        plaintext: &[u8],
    ) -> Result<PathBuf, StorageError> {
        let blob = crypto.seal(account_id, conversation_id, object_id, plaintext)?;
        let path = self.media_path(account_id, conversation_id, object_id);
        self.put_content(
            crate::memory::erasure::ErasureOrigin::new(account_id, conversation_id, message_id),
            "media",
            object_id,
            &path,
            &blob,
        )?;
        Ok(path)
    }

    pub fn put_index(
        &self,
        crypto: &AccountCrypto,
        account_id: &str,
        conversation_id: &str,
        message_id: &str,
        plaintext: &[u8],
    ) -> Result<PathBuf, StorageError> {
        let record = name(message_id);
        let blob = crypto.seal(account_id, conversation_id, &record, plaintext)?;
        let path = self.index_path(account_id, conversation_id, message_id);
        write_file(&path, &blob)?;
        Ok(path)
    }

    pub fn read_index(
        &self,
        crypto: &AccountCrypto,
        account_id: &str,
        conversation_id: &str,
        message_id: &str,
    ) -> Result<Vec<u8>, StorageError> {
        let path = self.index_path(account_id, conversation_id, message_id);
        self.read_index_file(crypto, account_id, conversation_id, &path)
    }

    pub fn read_index_file(
        &self,
        crypto: &AccountCrypto,
        account_id: &str,
        conversation_id: &str,
        path: &Path,
    ) -> Result<Vec<u8>, StorageError> {
        let record = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let blob = std::fs::read(path).map_err(|error| StorageError::Io(error.to_string()))?;
        Ok(crypto.open(account_id, conversation_id, &record, &blob)?)
    }

    #[must_use]
    pub fn message_path(&self, account_id: &str, conversation_id: &str, message_id: &str) -> PathBuf {
        self.root
            .join("mem")
            .join(name(account_id))
            .join(name(conversation_id))
            .join(name(message_id))
    }

    #[must_use]
    pub fn event_path(&self, account_id: &str, conversation_id: &str, event_id: &str) -> PathBuf {
        self.root
            .join("events")
            .join(name(account_id))
            .join(name(conversation_id))
            .join(name(event_id))
    }

    #[must_use]
    pub fn media_path(&self, account_id: &str, conversation_id: &str, object_id: &str) -> PathBuf {
        self.root
            .join("media")
            .join(name(account_id))
            .join(name(conversation_id))
            .join(name(object_id))
    }

    #[must_use]
    pub fn index_path(&self, account_id: &str, conversation_id: &str, message_id: &str) -> PathBuf {
        self.root
            .join("index")
            .join(name(account_id))
            .join(name(conversation_id))
            .join(name(message_id))
    }

    /// Message ids are the file names, which are hashes. Callers that need the
    /// original id read the sealed entry. This lists the hashed file stems so
    /// a conversation erase can open each one.
    #[must_use]
    pub fn marker_path(&self, account_id: &str, conversation_id: &str) -> PathBuf {
        self.root
            .join("markers")
            .join(name(account_id))
            .join(name(conversation_id))
    }
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), StorageError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| StorageError::Io(error.to_string()))?;
    }
    std::fs::write(path, bytes).map_err(|error| StorageError::Io(error.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| StorageError::Io(error.to_string()))?;
    }
    Ok(())
}

fn name(value: &str) -> String {
    crypto::id_hash(value)
}

fn delete_legacy_plaintext(root: &Path) {
    let legacy = root.join("plaintext");
    if !legacy.exists() {
        return;
    }
    tracing::warn!(
        path = %legacy.display(),
        "deleted pre-existing plaintext ramflux channel data"
    );
    let _ = std::fs::remove_dir_all(legacy);
}

/// Overwrite a file with zeros and remove it. Missing files are success.
pub fn secure_remove(path: &Path) -> Result<(), StorageError> {
    if !path.exists() {
        return Ok(());
    }
    crypto::overwrite_and_remove(path).map_err(|error| StorageError::Io(error.to_string()))
}
