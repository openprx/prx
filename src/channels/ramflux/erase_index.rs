//! Persisted erase index for one ramflux message.
//!
//! Each record written for a message is appended here in the same order, so a
//! later `message_deleted` can forget those keys after a process restart.

use super::crypto::AccountCrypto;
use super::storage::{self, ChannelStore, StorageError};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct EraseEntry {
    pub message_id: String,
    pub memory_keys: Vec<String>,
    pub media_ids: Vec<String>,
    pub history_keys: Vec<String>,
}

/// Remember one memory key, media id, or history key for `message_id`.
pub fn note(
    store: &ChannelStore,
    crypto: &AccountCrypto,
    account_id: &str,
    conversation_id: &str,
    message_id: &str,
    memory_key: Option<&str>,
    media_id: Option<&str>,
    history_key: Option<&str>,
) {
    let mut entry = match load(store, crypto, account_id, conversation_id, message_id) {
        Ok(entry) => entry,
        Err(error) => {
            tracing::warn!(%error, "ramflux erase metadata could not be read");
            return;
        }
    }
    .unwrap_or_else(|| EraseEntry {
        message_id: message_id.to_string(),
        ..EraseEntry::default()
    });
    entry.message_id = message_id.to_string();
    if let Some(key) = memory_key {
        if !entry.memory_keys.iter().any(|existing| existing == key) {
            entry.memory_keys.push(key.to_string());
        }
    }
    if let Some(id) = media_id {
        if !entry.media_ids.iter().any(|existing| existing == id) {
            entry.media_ids.push(id.to_string());
        }
    }
    if let Some(key) = history_key {
        if !entry.history_keys.iter().any(|existing| existing == key) {
            entry.history_keys.push(key.to_string());
        }
    }
    let Ok(bytes) = serde_json::to_vec(&entry) else {
        return;
    };
    if let Err(error) = store.put_index(crypto, account_id, conversation_id, message_id, &bytes) {
        tracing::warn!(error = %error, "ramflux erase index was not written");
    }
}

pub fn load(
    store: &ChannelStore,
    crypto: &AccountCrypto,
    account_id: &str,
    conversation_id: &str,
    message_id: &str,
) -> Result<Option<EraseEntry>, String> {
    if !store.index_path(account_id, conversation_id, message_id).exists() {
        return Ok(None);
    }
    let bytes = store
        .read_index(crypto, account_id, conversation_id, message_id)
        .map_err(|error| error.to_string())?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| error.to_string())
}

/// Remove the index file after its keys have been erased.
pub fn remove(
    store: &ChannelStore,
    account_id: &str,
    conversation_id: &str,
    message_id: &str,
) -> Result<(), StorageError> {
    let path = store.index_path(account_id, conversation_id, message_id);
    if path.exists() {
        storage::secure_remove(&path)?;
    }
    Ok(())
}
