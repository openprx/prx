#![cfg(test)]

//! D10/C1-C4: chat session load paths must distinguish `None` (no such
//! session -> start fresh) from `Err` (storage failure / corrupt blob ->
//! propagate, fail fast). A `FailingMemory` mock injects storage errors so
//! we can assert load helpers return `Err`, never silently degrade to `None`.
use super::*;
use crate::memory::MemoryEntry;
use async_trait::async_trait;

/// Minimal `Memory` whose `get`/`list` fail; everything else is inert.
struct FailingMemory;

#[async_trait]
impl Memory for FailingMemory {
    fn name(&self) -> &str {
        "failing"
    }
    async fn store(
        &self,
        _key: &str,
        _content: &str,
        _category: MemoryCategory,
        _session_id: Option<&str>,
    ) -> Result<()> {
        Ok(())
    }
    async fn recall(&self, _query: &str, _limit: usize, _session_id: Option<&str>) -> Result<Vec<MemoryEntry>> {
        Ok(vec![])
    }
    async fn get(&self, _key: &str) -> Result<Option<MemoryEntry>> {
        Err(anyhow::anyhow!("injected storage failure on get"))
    }
    async fn list(&self, _category: Option<&MemoryCategory>, _session_id: Option<&str>) -> Result<Vec<MemoryEntry>> {
        Err(anyhow::anyhow!("injected storage failure on list"))
    }
    async fn forget(&self, _key: &str) -> Result<bool> {
        Ok(false)
    }
    async fn count(&self) -> Result<usize> {
        Ok(0)
    }
    async fn health_check(&self) -> bool {
        false
    }
}

/// `Memory` returning a fixed set of entries from `list` and exact `get`.
struct StaticMemory {
    entries: Vec<MemoryEntry>,
}

#[async_trait]
impl Memory for StaticMemory {
    fn name(&self) -> &str {
        "static"
    }
    async fn store(
        &self,
        _key: &str,
        _content: &str,
        _category: MemoryCategory,
        _session_id: Option<&str>,
    ) -> Result<()> {
        Ok(())
    }
    async fn recall(&self, _query: &str, _limit: usize, _session_id: Option<&str>) -> Result<Vec<MemoryEntry>> {
        Ok(vec![])
    }
    async fn get(&self, key: &str) -> Result<Option<MemoryEntry>> {
        Ok(self.entries.iter().find(|e| e.key == key).cloned())
    }
    async fn list(&self, _category: Option<&MemoryCategory>, _session_id: Option<&str>) -> Result<Vec<MemoryEntry>> {
        Ok(self.entries.clone())
    }
    async fn forget(&self, _key: &str) -> Result<bool> {
        Ok(false)
    }
    async fn count(&self) -> Result<usize> {
        Ok(self.entries.len())
    }
    async fn health_check(&self) -> bool {
        true
    }
}

fn entry(key: &str, content: &str) -> MemoryEntry {
    MemoryEntry {
        id: key.to_string(),
        key: key.to_string(),
        content: content.to_string(),
        category: MemoryCategory::Conversation,
        timestamp: "now".to_string(),
        session_id: None,
        score: None,
        tags: None,
        access_count: None,
        useful_count: None,
        source: None,
        source_confidence: None,
        verification_status: None,
        lifecycle_state: None,
        compressed_from: None,
    }
}

fn session_entry(id: &str) -> MemoryEntry {
    let mut s = session::ChatSession::new("p", "m");
    s.id = id.to_string();
    let json = s.to_json().expect("test: serialize session");
    entry(&format!("{}:{}", session::SESSION_MEMORY_PREFIX, id), &json)
}

// C1: storage Err on get must propagate, not collapse to Ok(None).
#[tokio::test]
async fn load_session_by_id_propagates_storage_error() {
    let mem = FailingMemory;
    let result = load_session_by_id(&mem, "abc").await;
    assert!(result.is_err(), "storage error must surface as Err, not Ok(None)");
}

// Ok(None): genuine absence still maps to a fresh-session path.
#[tokio::test]
async fn load_session_by_id_missing_returns_ok_none() {
    let mem = StaticMemory { entries: vec![] };
    let result = load_session_by_id(&mem, "missing").await;
    assert!(matches!(result, Ok(None)), "absent session must be Ok(None)");
}

// C2: corrupt blob under an exact session key is data corruption -> Err.
#[tokio::test]
async fn load_session_by_id_corrupt_blob_is_error() {
    let mem = StaticMemory {
        entries: vec![entry(
            &format!("{}:bad", session::SESSION_MEMORY_PREFIX),
            "{not valid json",
        )],
    };
    let result = load_session_by_id(&mem, "bad").await;
    assert!(result.is_err(), "corrupt stored blob must be Err, not Ok(None)");
}

// C3 (id consistency): embedded id that disagrees with the requested id must be Err.
// If `chat_session:<id>` stores a blob whose `id` field is a different value, resuming
// it would silently continue with the wrong session (D10 review finding).
#[tokio::test]
async fn load_session_by_id_embedded_id_mismatch_is_error() {
    // Build a valid session blob whose embedded id is "other-id".
    let mut s = session::ChatSession::new("p", "m");
    s.id = "other-id".to_string();
    let json = s.to_json().expect("test: serialize session");
    // Store it under the key for "requested-id" — key/embedded-id disagree.
    let mem = StaticMemory {
        entries: vec![entry(
            &format!("{}:requested-id", session::SESSION_MEMORY_PREFIX),
            &json,
        )],
    };
    let result = load_session_by_id(&mem, "requested-id").await;
    assert!(
        result.is_err(),
        "embedded id mismatch must be Err, not Ok(Some(wrong_session))"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("refusing") && msg.contains("bury"),
        "error message should mention 'refusing' and 'bury': {msg}"
    );
}

// Happy path: valid stored session round-trips.
#[tokio::test]
async fn load_session_by_id_valid_round_trips() {
    let mem = StaticMemory {
        entries: vec![session_entry("good")],
    };
    let result = load_session_by_id(&mem, "good").await.expect("test: load");
    assert_eq!(result.map(|s| s.id), Some("good".to_string()));
}

// C3: storage Err on list must propagate from load_latest_session.
#[tokio::test]
async fn load_latest_session_propagates_storage_error() {
    let mem = FailingMemory;
    assert!(
        load_latest_session(&mem).await.is_err(),
        "list error must surface as Err"
    );
}

#[tokio::test]
async fn load_latest_session_empty_returns_ok_none() {
    let mem = StaticMemory { entries: vec![] };
    assert!(matches!(load_latest_session(&mem).await, Ok(None)));
}

// Corrupt exact session entry must not be silently filtered out of latest/list.
#[tokio::test]
async fn load_latest_session_corrupt_entry_is_error() {
    let mem = StaticMemory {
        entries: vec![entry(&format!("{}:rotten", session::SESSION_MEMORY_PREFIX), "{corrupt")],
    };
    assert!(
        load_latest_session(&mem).await.is_err(),
        "corrupt session entry must surface as Err, not be dropped as 'no session'"
    );
}

// C4: list_saved_sessions must propagate the storage error (no unwrap_or_default).
#[tokio::test]
async fn list_saved_sessions_propagates_storage_error() {
    let mem = FailingMemory;
    assert!(
        list_saved_sessions(&mem).await.is_err(),
        "list error must surface, not print 'No saved sessions'"
    );
}

// Non-session entries (wrong prefix) are skipped, not errored.
#[test]
fn valid_session_entry_skips_non_session_entries() {
    let e = entry("unrelated:key", "whatever");
    assert!(matches!(valid_session_entry(&e), Ok(None)));
}

// Key/id disagreement on an exact session entry is corruption -> Err.
#[test]
fn valid_session_entry_key_id_mismatch_is_error() {
    let mut s = session::ChatSession::new("p", "m");
    s.id = "embedded-id".to_string();
    let json = s.to_json().expect("test: serialize");
    let e = entry(&format!("{}:key-id", session::SESSION_MEMORY_PREFIX), &json);
    assert!(valid_session_entry(&e).is_err());
}
