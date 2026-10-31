//! At-rest files for the ramflux channel.
//!
//! Content is ciphertext from [`super::crypto`]. File names are random ids.
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
        Ok(Self { root })
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
    pub fn put_message(
        &self,
        crypto: &AccountCrypto,
        account_id: &str,
        conversation_id: &str,
        message_id: &str,
        plaintext: &[u8],
    ) -> Result<PathBuf, StorageError> {
        let blob = crypto.seal(account_id, conversation_id, plaintext)?;
        let path = self.message_path(account_id, conversation_id, message_id);
        write_file(&path, &blob)?;
        Ok(path)
    }

    /// Read one message blob.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the file is missing or cannot be opened.
    pub fn get_message(
        &self,
        crypto: &AccountCrypto,
        account_id: &str,
        conversation_id: &str,
        message_id: &str,
    ) -> Result<Vec<u8>, StorageError> {
        let path = self.message_path(account_id, conversation_id, message_id);
        let blob = std::fs::read(&path).map_err(|error| StorageError::Io(error.to_string()))?;
        Ok(crypto.open(account_id, conversation_id, &blob)?)
    }

    pub fn put_tool_event(
        &self,
        crypto: &AccountCrypto,
        account_id: &str,
        conversation_id: &str,
        event_id: &str,
        plaintext: &[u8],
    ) -> Result<PathBuf, StorageError> {
        let blob = crypto.seal(account_id, conversation_id, plaintext)?;
        let path = self.event_path(account_id, event_id);
        write_file(&path, &blob)?;
        Ok(path)
    }

    pub fn put_media(
        &self,
        crypto: &AccountCrypto,
        account_id: &str,
        conversation_id: &str,
        object_id: &str,
        plaintext: &[u8],
    ) -> Result<PathBuf, StorageError> {
        let blob = crypto.seal(account_id, conversation_id, plaintext)?;
        let path = self.media_path(&format!("{account_id}-{object_id}"));
        write_file(&path, &blob)?;
        Ok(path)
    }

    #[must_use]
    pub fn message_path(&self, account_id: &str, conversation_id: &str, message_id: &str) -> PathBuf {
        self.root
            .join("mem")
            .join(safe(account_id))
            .join(safe(conversation_id))
            .join(safe(message_id))
    }

    #[must_use]
    pub fn event_path(&self, account_id: &str, event_id: &str) -> PathBuf {
        self.root.join("events").join(safe(account_id)).join(safe(event_id))
    }

    #[must_use]
    pub fn media_path(&self, object_id: &str) -> PathBuf {
        self.root.join("media").join(safe(object_id))
    }

    #[must_use]
    pub fn marker_path(&self, account_id: &str, conversation_id: &str) -> PathBuf {
        self.root
            .join("markers")
            .join(safe(account_id))
            .join(safe(conversation_id))
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

fn safe(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
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
