//! Hard delete for one message, one conversation, or a whole account.
//!
//! Files are overwritten with zeros before removal. Account erase also
//! destroys the wrapped data key. Conversation erase leaves the key in place.
//! Media lives at `media/{hash(account)}/{hash(object)}`, so removing one
//! account directory cannot match another account's hash.

use super::crypto::{self, AccountCrypto};
use super::storage::{self, ChannelStore, StorageError};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EraseReport {
    pub memory_rows: usize,
    pub media_files: usize,
    pub tool_events: usize,
    pub drafts: usize,
    pub key_material: bool,
}

/// Erase one message blob and its draft sidecar.
///
/// # Errors
///
/// Returns [`StorageError`] when a file cannot be overwritten.
pub fn erase_message(
    store: &ChannelStore,
    account_id: &str,
    conversation_id: &str,
    message_id: &str,
) -> Result<EraseReport, StorageError> {
    let mut report = EraseReport::default();
    let message = store.message_path(account_id, conversation_id, message_id);
    if message.exists() {
        storage::secure_remove(&message)?;
        report.memory_rows = 1;
    }
    let draft = message.with_extension("draft");
    if draft.exists() {
        storage::secure_remove(&draft)?;
        report.drafts = 1;
    }
    Ok(report)
}

/// Erase one cached attachment. The object id is matched exactly via its hash.
///
/// # Errors
///
/// Returns [`StorageError`] when the file cannot be overwritten.
pub fn erase_media(
    store: &ChannelStore,
    account_id: &str,
    conversation_id: &str,
    object_id: &str,
) -> Result<bool, StorageError> {
    let path = store.media_path(account_id, conversation_id, object_id);
    if !path.exists() {
        return Ok(false);
    }
    storage::secure_remove(&path)?;
    Ok(true)
}

/// Erase one conversation's messages, tool events, and marker. The DEK stays.
///
/// # Errors
///
/// Returns [`StorageError`] when a directory cannot be removed.
pub fn erase_conversation(
    store: &ChannelStore,
    account_id: &str,
    conversation_id: &str,
) -> Result<EraseReport, StorageError> {
    let mut report = EraseReport::default();
    let message = store.message_path(account_id, conversation_id, "placeholder");
    let events = store.event_path(account_id, conversation_id, "placeholder");
    let message_dir = message
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| store.root().to_path_buf());
    let event_dir = events
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| store.root().to_path_buf());
    report.memory_rows = remove_tree_files(&message_dir)?;
    report.tool_events = remove_tree_files(&event_dir)?;
    let media = store.media_path(account_id, conversation_id, "placeholder");
    if let Some(media_dir) = media.parent() {
        report.media_files = remove_tree_files(media_dir)?;
    }
    let index = store.index_path(account_id, conversation_id, "placeholder");
    if let Some(index_dir) = index.parent() {
        let _ = remove_tree_files(index_dir);
    }
    let marker = store.marker_path(account_id, conversation_id);
    if marker.exists() {
        storage::secure_remove(&marker)?;
        report.drafts = 1;
    }
    Ok(report)
}

/// Erase every file for an account, including the wrapped data key.
///
/// # Errors
///
/// Returns [`StorageError`] when a directory cannot be removed.
pub fn erase_account(
    store: &ChannelStore,
    crypto: &AccountCrypto,
    account_id: &str,
) -> Result<EraseReport, StorageError> {
    let mut report = EraseReport::default();
    let account = crypto::id_hash(account_id);
    report.memory_rows = remove_tree_files(&store.root().join("mem").join(&account))?;
    report.tool_events = remove_tree_files(&store.root().join("events").join(&account))?;
    report.media_files = remove_tree_files(&store.root().join("media").join(&account))?;
    report.drafts = remove_tree_files(&store.root().join("markers").join(&account))?;
    crypto
        .destroy(account_id)
        .map_err(|error| StorageError::Io(error.to_string()))?;
    report.key_material = true;
    Ok(report)
}

/// True when `needle` occurs in any regular file under `root`.
#[cfg(test)]
#[must_use]
pub fn dir_contains(root: &Path, needle: &[u8]) -> bool {
    if needle.is_empty() || !root.exists() {
        return false;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let entries = match std::fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let child = entry.path();
            if child.is_dir() {
                stack.push(child);
                continue;
            }
            if let Ok(bytes) = std::fs::read(&child) {
                if bytes.windows(needle.len()).any(|window| window == needle) {
                    return true;
                }
            }
        }
    }
    false
}

fn remove_tree_files(dir: &Path) -> Result<usize, StorageError> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut count = 0usize;
    let mut stack = vec![dir.to_path_buf()];
    let mut files = Vec::new();
    while let Some(path) = stack.pop() {
        let entries = match std::fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(error) => return Err(StorageError::Io(error.to_string())),
        };
        for entry in entries {
            let entry = entry.map_err(|error| StorageError::Io(error.to_string()))?;
            let child = entry.path();
            if child.is_dir() {
                stack.push(child);
            } else {
                files.push(child);
            }
        }
    }
    for file in &files {
        storage::secure_remove(file)?;
        count = count.saturating_add(1);
    }
    let _ = std::fs::remove_dir_all(dir);
    Ok(count)
}
