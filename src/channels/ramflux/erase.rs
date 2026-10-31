//! Hard delete for `inbound.message_deleted` and `agent_revoked`.
//!
//! Files are overwritten with zeros before removal. The report counts what
//! was removed. A scan of the data directory after erase must not find the
//! plaintext or the ciphertext bytes.

use super::crypto::AccountCrypto;
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

/// Erase one message blob and any media or tool-event files that name it.
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
    report.memory_rows = remove_tree_files(&store.root().join("mem").join(safe(account_id)))?;
    report.tool_events = remove_tree_files(&store.root().join("events").join(safe(account_id)))?;
    report.media_files = remove_named_media(store.root(), account_id)?;
    let markers = store.root().join("markers").join(safe(account_id));
    report.drafts = remove_tree_files(&markers)?;
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

fn remove_named_media(root: &Path, account_id: &str) -> Result<usize, StorageError> {
    let dir = root.join("media");
    if !dir.exists() {
        return Ok(0);
    }
    let mut count = 0usize;
    let prefix = safe(account_id);
    let entries = std::fs::read_dir(&dir).map_err(|error| StorageError::Io(error.to_string()))?;
    for entry in entries {
        let entry = entry.map_err(|error| StorageError::Io(error.to_string()))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&prefix) {
            storage::secure_remove(&entry.path())?;
            count = count.saturating_add(1);
        }
    }
    Ok(count)
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
