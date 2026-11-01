//! Per-account persona and the `agent_removed_at` memory marker.
//!
//! Missing account fields fall back to the global agent settings. Memory is
//! namespaced as `agent_account_id/conversation_id`. Cross-conversation
//! retrieval stays off unless the account sets it.

use super::storage::{self, ChannelStore, StorageError};
use crate::config::{RamfluxAccountConfig, RamfluxConfig};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Persona {
    pub system_prompt: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub scope_profile: Option<String>,
    pub cross_conversation_retrieval: bool,
}

/// Resolve the persona for `account_id`, falling back to empty optional fields.
pub fn resolve(config: &RamfluxConfig, account_id: &str, workspace: &Path) -> Persona {
    let Some(account) = config.accounts.get(account_id) else {
        return Persona {
            system_prompt: None,
            model: None,
            provider: None,
            scope_profile: None,
            cross_conversation_retrieval: false,
        };
    };
    Persona {
        system_prompt: read_prompt(account, workspace),
        model: account.model.clone(),
        provider: account.provider.clone(),
        scope_profile: account.scope_profile.clone(),
        cross_conversation_retrieval: account.cross_conversation_retrieval,
    }
}

/// `agent_account_id/conversation_id`.
#[must_use]
pub fn namespace(account_id: &str, conversation_id: &str) -> String {
    format!("{account_id}/{conversation_id}")
}

impl Persona {
    /// Directory and AEAD scope for one conversation.
    ///
    /// Cross-conversation retrieval collapses every chat onto the account.
    /// A scope profile qualifies the default per-conversation namespace.
    #[must_use]
    pub fn memory_scope(&self, account_id: &str, conversation_id: &str) -> String {
        if self.cross_conversation_retrieval {
            return account_id.to_string();
        }
        let base = namespace(account_id, conversation_id);
        match self.scope_profile.as_deref() {
            Some(profile) if !profile.is_empty() => format!("{base}#{profile}"),
            _ => base,
        }
    }
}

/// Scope used to store and erase one conversation's bytes.
#[must_use]
pub fn memory_scope(persona: Option<&Persona>, account_id: &str, conversation_id: &str) -> String {
    persona.map_or_else(
        || namespace(account_id, conversation_id),
        |persona| persona.memory_scope(account_id, conversation_id),
    )
}

/// Record that the agent was removed from a conversation. Bytes are kept.
///
/// # Errors
///
/// Returns [`StorageError`] when the marker cannot be written.
pub fn mark_removed(
    store: &ChannelStore,
    account_id: &str,
    conversation_id: &str,
    at_ms: u64,
) -> Result<(), StorageError> {
    let path = store.marker_path(account_id, conversation_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| StorageError::Io(error.to_string()))?;
    }
    std::fs::write(&path, at_ms.to_string()).map_err(|error| StorageError::Io(error.to_string()))
}

/// Clear a removal marker after the agent is added again.
///
/// # Errors
///
/// Returns [`StorageError`] when the marker file cannot be removed.
pub fn clear_removed(store: &ChannelStore, account_id: &str, conversation_id: &str) -> Result<(), StorageError> {
    storage::secure_remove(&store.marker_path(account_id, conversation_id))
}

#[must_use]
pub fn is_removed(store: &ChannelStore, account_id: &str, conversation_id: &str) -> bool {
    is_account_removed(store, account_id) || store.marker_path(account_id, conversation_id).is_file()
}

/// Record that the whole account was erased. Later traffic is not authorized.
///
/// # Errors
///
/// Returns [`StorageError`] when the marker cannot be written.
pub fn mark_account_removed(store: &ChannelStore, account_id: &str) -> Result<(), StorageError> {
    let path = store.marker_path(account_id, "account");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| StorageError::Io(error.to_string()))?;
    }
    std::fs::write(&path, b"removed").map_err(|error| StorageError::Io(error.to_string()))
}

#[must_use]
pub fn is_account_removed(store: &ChannelStore, account_id: &str) -> bool {
    store.marker_path(account_id, "account").is_file()
}

fn read_prompt(account: &RamfluxAccountConfig, workspace: &Path) -> Option<String> {
    let path = account.system_prompt_file.as_ref()?;
    let full = if path.is_absolute() {
        path.clone()
    } else {
        workspace.join(path)
    };
    match std::fs::read_to_string(&full) {
        Ok(text) => Some(text),
        Err(error) => {
            tracing::warn!(path = %full.display(), error = %error, "ramflux persona file was not read");
            None
        }
    }
}
