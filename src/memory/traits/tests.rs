use super::*;

// ---------------------------------------------------------------------
// V02-R5 follow-up F2: the trait-level defaults must fail closed.
//
// `SqliteMemory` and `PostgresMemory` override every `*_with_context*`
// read with a real owner-scoped predicate. Every other backend — markdown,
// none, lucid and the wasm plugin wrappers — inherits the defaults in this
// file, which used to discard the context and answer from the unscoped
// read. An owner-isolated MCP caller therefore read the whole store on any
// of them, and an unknown backend name falls back to markdown.
//
// `MarkdownMemory` is the fixture because it inherits every default here
// unchanged and really returns rows, so "the caller reads what it must not"
// is observable rather than argued.
// ---------------------------------------------------------------------

const UNSCOPED_KEY: &str = "first-owner-note";
const UNSCOPED_CONTENT: &str = "alpha rollout note authored by the first owner";
const UNSCOPED_QUERY: &str = "rollout";

/// A read context from a trusted, owner-isolated origin whose owner is not
/// the one that authored the row.
fn isolated_reader_context(owner_id: Option<&str>) -> MemoryWriteContext {
    MemoryWriteContext {
        agent_partition: None,
        session_key: None,
        workspace_id: Some("ws-isolation".to_string()),
        channel: Some("mcp".to_string()),
        chat_type: Some("gateway".to_string()),
        chat_id: Some("mcp:shared-subject".to_string()),
        sender_id: owner_id.map(str::to_string),
        raw_sender: Some("shared-subject".to_string()),
        owner_id: owner_id.map(str::to_string),
        require_owner_match: true,
    }
}

async fn unscoped_backend_with_one_row() -> (tempfile::TempDir, crate::memory::markdown::MarkdownMemory) {
    let workspace = tempfile::TempDir::new().expect("test: temp workspace");
    let memory = crate::memory::markdown::MarkdownMemory::new(workspace.path());
    memory
        .store(UNSCOPED_KEY, UNSCOPED_CONTENT, MemoryCategory::Core, None)
        .await
        .expect("test: store a row on the unscoped backend");
    assert!(
        !memory
            .recall(UNSCOPED_QUERY, 10, None)
            .await
            .expect("test: unscoped recall")
            .is_empty(),
        "sanity: the fixture backend really returns the row to an unscoped read"
    );
    (workspace, memory)
}

/// MUTATION GUARD: drop the `require_owner_match` short-circuit from
/// `recall_with_context` / `get_with_context` in this file and this goes
/// red — the isolated reader gets the other owner's row back.
#[tokio::test]
async fn an_owner_isolated_reader_gets_nothing_from_a_backend_without_owner_scoping() {
    let (_workspace, memory) = unscoped_backend_with_one_row().await;
    let reader = isolated_reader_context(Some("owner:ws-isolation:agent:issuer-b.example:shared-subject"));

    let recalled = memory
        .recall_with_context(UNSCOPED_QUERY, 10, None, Some(&reader))
        .await
        .expect("test: isolated recall");
    assert!(
        recalled.is_empty(),
        "an unscoped backend cannot prove ownership, so it must answer nothing: {:?}",
        recalled.iter().map(|entry| entry.key.as_str()).collect::<Vec<_>>()
    );

    assert!(
        memory
            .get_with_context(UNSCOPED_KEY, Some(&reader))
            .await
            .expect("test: isolated keyed read")
            .is_none(),
        "an exact key must not cross the boundary on an unscoped backend either"
    );
}

/// The isolation bit outranks the requested mode here exactly as it does in
/// both SQL adapters, so an `Observe` request cannot reopen the boundary.
#[tokio::test]
async fn an_observe_request_does_not_reopen_the_boundary_on_an_unscoped_backend() {
    let (_workspace, memory) = unscoped_backend_with_one_row().await;
    let reader = isolated_reader_context(Some("owner:ws-isolation:agent:issuer-b.example:shared-subject"));

    assert!(
        memory
            .recall_with_context_mode(UNSCOPED_QUERY, 10, None, Some(&reader), MemoryReadMode::Observe)
            .await
            .expect("test: observe-mode recall under isolation")
            .is_empty(),
        "Observe must not reopen the owner boundary on an unscoped backend"
    );
    assert!(
        memory
            .get_with_context_mode(UNSCOPED_KEY, Some(&reader), MemoryReadMode::Observe)
            .await
            .expect("test: observe-mode keyed read under isolation")
            .is_none(),
        "Observe must not reopen the owner boundary for the keyed read either"
    );
}

/// Fail closed, not fail open: an isolated caller with no owner id at all
/// reaches nothing rather than falling back to the unscoped read.
#[tokio::test]
async fn an_isolated_reader_without_an_owner_id_reads_nothing_on_an_unscoped_backend() {
    let (_workspace, memory) = unscoped_backend_with_one_row().await;

    for owner in [None, Some("   ")] {
        let reader = isolated_reader_context(owner);
        assert!(
            memory
                .recall_with_context(UNSCOPED_QUERY, 10, None, Some(&reader))
                .await
                .expect("test: isolated recall without an owner id")
                .is_empty(),
            "an isolated caller with owner {owner:?} must reach nothing"
        );
    }
}

/// The byte-for-byte lock: without the isolation bit the defaults still
/// delegate to the unscoped read, so every in-process caller is unchanged.
#[tokio::test]
async fn a_non_isolated_reader_still_gets_the_legacy_unscoped_answer() {
    let (_workspace, memory) = unscoped_backend_with_one_row().await;
    let mut reader = isolated_reader_context(Some("owner:ws-isolation:agent:issuer-b.example:shared-subject"));
    reader.require_owner_match = false;

    assert!(
        memory
            .recall_with_context(UNSCOPED_QUERY, 10, None, Some(&reader))
            .await
            .expect("test: non-isolated recall")
            .iter()
            .any(|entry| entry.content.contains(UNSCOPED_CONTENT)),
        "a non-isolated caller keeps the historical unscoped answer"
    );
    assert!(
        memory
            .get_with_context(UNSCOPED_KEY, Some(&reader))
            .await
            .expect("test: non-isolated keyed read")
            .is_some(),
        "a non-isolated keyed read keeps the historical unscoped answer"
    );
    assert!(
        memory
            .recall_with_context(UNSCOPED_QUERY, 10, None, None)
            .await
            .expect("test: recall with no context at all")
            .iter()
            .any(|entry| entry.content.contains(UNSCOPED_CONTENT)),
        "a caller with no context at all keeps the historical unscoped answer"
    );
}

/// A minimal backend that inherits every default on this trait and whose
/// `forget` really deletes.
///
/// `MarkdownMemory` is append-only — its `forget` always answers `false` —
/// so it cannot tell a refusal apart from "nothing was deleted". This
/// double can, which is the only way to make the delete assertions below
/// non-vacuous. It stores no owner and has no visibility predicate, exactly
/// like every backend the defaults serve.
#[derive(Default)]
struct DeletableUnscopedBackend {
    rows: parking_lot::Mutex<Vec<(String, String)>>,
}

impl DeletableUnscopedBackend {
    fn entry(key: &str, content: &str) -> MemoryEntry {
        MemoryEntry {
            origin: crate::memory::traits::MemoryEntryOrigin::Stored,
            source_event_id: None,
            id: key.to_string(),
            key: key.to_string(),
            content: content.to_string(),
            category: MemoryCategory::Core,
            timestamp: "2026-09-21T00:00:00Z".to_string(),
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
}

#[async_trait]
impl Memory for DeletableUnscopedBackend {
    fn name(&self) -> &str {
        "deletable-unscoped"
    }

    async fn store(
        &self,
        key: &str,
        content: &str,
        _category: MemoryCategory,
        _session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.rows.lock().push((key.to_string(), content.to_string()));
        Ok(())
    }

    async fn recall(
        &self,
        query: &str,
        limit: usize,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        Ok(self
            .rows
            .lock()
            .iter()
            .filter(|(_, content)| content.contains(query))
            .take(limit)
            .map(|(key, content)| Self::entry(key, content))
            .collect())
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        Ok(self
            .rows
            .lock()
            .iter()
            .find(|(stored, _)| stored == key)
            .map(|(stored, content)| Self::entry(stored, content)))
    }

    async fn list(
        &self,
        _category: Option<&MemoryCategory>,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        Ok(self
            .rows
            .lock()
            .iter()
            .map(|(key, content)| Self::entry(key, content))
            .collect())
    }

    async fn forget(&self, key: &str) -> anyhow::Result<bool> {
        let mut rows = self.rows.lock();
        let before = rows.len();
        rows.retain(|(stored, _)| stored != key);
        Ok(rows.len() != before)
    }

    async fn count(&self) -> anyhow::Result<usize> {
        Ok(self.rows.lock().len())
    }

    async fn health_check(&self) -> bool {
        true
    }
}

/// The delete half of the same boundary: `forget_with_context` is the one
/// context-carrying method on this trait that used to discard its context,
/// so delete-by-key was unscoped on every backend that inherits the
/// default. `memory_forget` is not on the default MCP surface, but an
/// operator may expose it, and an unscoped delete from an owner-isolated
/// caller is a cross-owner delete.
///
/// MUTATION GUARD: restore `let _ = context;` in the default
/// `forget_with_context` and the first two assertions go red.
#[tokio::test]
async fn an_owner_isolated_caller_forgets_nothing_on_a_backend_without_owner_scoping() {
    let memory = DeletableUnscopedBackend::default();
    memory
        .store(UNSCOPED_KEY, UNSCOPED_CONTENT, MemoryCategory::Core, None)
        .await
        .expect("test: store a row on the unscoped backend");
    let isolated = isolated_reader_context(Some("owner:ws-isolation:agent:issuer-b.example:shared-subject"));

    assert!(
        !memory
            .forget_with_context(UNSCOPED_KEY, Some(&isolated))
            .await
            .expect("test: isolated delete by key"),
        "an unscoped backend cannot prove ownership, so an isolated caller deletes nothing"
    );
    assert!(
        memory.get(UNSCOPED_KEY).await.expect("test: keyed read").is_some(),
        "the row must still be there: the refusal has to be a refusal, not a silent delete"
    );

    let mut in_process = isolated;
    in_process.require_owner_match = false;
    assert!(
        memory
            .forget_with_context(UNSCOPED_KEY, Some(&in_process))
            .await
            .expect("test: non-isolated delete by key"),
        "a non-isolated caller keeps the historical unscoped delete"
    );
    assert!(
        memory.get(UNSCOPED_KEY).await.expect("test: keyed read").is_none(),
        "and the row really is gone, so the assertions above are not vacuous"
    );
}

/// V02-R9: the write half. A backend that stores no owner (`lucid`,
/// `markdown`, `none`, wasm wrappers) cannot keep an owner-isolated caller
/// inside its own keys, so the default `store_with_context` refuses the
/// write instead of overwriting whatever the key holds.
///
/// MUTATION GUARD: restore `let _ = context;` in the default
/// `store_with_context` and the first two assertions go red.
#[tokio::test]
async fn an_owner_isolated_caller_stores_nothing_on_a_backend_without_owner_scoping() {
    let memory = DeletableUnscopedBackend::default();
    memory
        .store(UNSCOPED_KEY, UNSCOPED_CONTENT, MemoryCategory::Core, None)
        .await
        .expect("test: store a row on the unscoped backend");
    let isolated = isolated_reader_context(Some("owner:ws-isolation:agent:issuer-b.example:shared-subject"));

    let refused = memory
        .store_with_context(UNSCOPED_KEY, "overwrite", MemoryCategory::Core, None, Some(&isolated))
        .await
        .expect_err("test: an isolated write on an unscoped backend must fail");
    assert!(
        refused.to_string().contains("cannot confine a write to an owner"),
        "{refused}"
    );
    assert_eq!(
        memory
            .get(UNSCOPED_KEY)
            .await
            .expect("test: keyed read")
            .map(|entry| entry.content),
        Some(UNSCOPED_CONTENT.to_string()),
        "the refusal has to be a refusal, not a silent overwrite"
    );

    let mut in_process = isolated;
    in_process.require_owner_match = false;
    memory
        .store_with_context(UNSCOPED_KEY, "overwrite", MemoryCategory::Core, None, Some(&in_process))
        .await
        .expect("test: a non-isolated caller keeps the historical write");
    assert!(
        memory
            .list(None, None)
            .await
            .expect("test: list rows")
            .iter()
            .any(|entry| entry.content == "overwrite"),
        "and the write really happened, so the refusal above is not vacuous"
    );
}

#[test]
fn memory_category_display_outputs_expected_values() {
    assert_eq!(MemoryCategory::Core.to_string(), "core");
    assert_eq!(MemoryCategory::Daily.to_string(), "daily");
    assert_eq!(MemoryCategory::Conversation.to_string(), "conversation");
    assert_eq!(
        MemoryCategory::Custom("project_notes".into()).to_string(),
        "project_notes"
    );
}

#[test]
fn memory_category_serde_uses_snake_case() {
    let core = serde_json::to_string(&MemoryCategory::Core).unwrap();
    let daily = serde_json::to_string(&MemoryCategory::Daily).unwrap();
    let conversation = serde_json::to_string(&MemoryCategory::Conversation).unwrap();

    assert_eq!(core, "\"core\"");
    assert_eq!(daily, "\"daily\"");
    assert_eq!(conversation, "\"conversation\"");
}

#[test]
fn memory_write_target_rejects_fitness_even_for_self_system() {
    let error = validate_memory_write_target(
        "self/fitness/daily/2026-09-11",
        Some(crate::self_system::SELF_SYSTEM_SESSION_ID),
    )
    .unwrap_err();

    assert!(error.to_string().contains("use FitnessStore"));
    assert!(
        validate_memory_write_target("self/decisions/item", Some(crate::self_system::SELF_SYSTEM_SESSION_ID))
            .is_ok()
    );
}

#[test]
fn memory_entry_roundtrip_preserves_optional_fields() {
    let entry = MemoryEntry {
        origin: crate::memory::traits::MemoryEntryOrigin::Stored,
        source_event_id: None,
        id: "id-1".into(),
        key: "favorite_language".into(),
        content: "Rust".into(),
        category: MemoryCategory::Core,
        timestamp: "2026-02-16T00:00:00Z".into(),
        session_id: Some("session-abc".into()),
        score: Some(0.98),
        tags: Some(vec!["language".into(), "preference".into()]),
        access_count: Some(4),
        useful_count: Some(3),
        source: Some("task-2026-02-16".into()),
        source_confidence: Some(0.92),
        verification_status: Some(VerificationStatus::Verified),
        lifecycle_state: Some(LifecycleState::Active),
        compressed_from: Some(vec!["old-1".into(), "old-2".into()]),
    };

    let json = serde_json::to_string(&entry).unwrap();
    let parsed: MemoryEntry = serde_json::from_str(&json).unwrap();

    assert_eq!(parsed.id, "id-1");
    assert_eq!(parsed.key, "favorite_language");
    assert_eq!(parsed.content, "Rust");
    assert_eq!(parsed.category, MemoryCategory::Core);
    assert_eq!(parsed.session_id.as_deref(), Some("session-abc"));
    assert_eq!(parsed.score, Some(0.98));
    assert_eq!(parsed.tags.as_ref().map(Vec::len), Some(2));
    assert_eq!(parsed.access_count, Some(4));
    assert_eq!(parsed.useful_count, Some(3));
    assert_eq!(parsed.source.as_deref(), Some("task-2026-02-16"));
    assert_eq!(parsed.source_confidence, Some(0.92));
    assert_eq!(parsed.verification_status, Some(VerificationStatus::Verified));
    assert_eq!(parsed.lifecycle_state, Some(LifecycleState::Active));
    assert_eq!(parsed.compressed_from.as_ref().map(Vec::len), Some(2));
}

#[test]
fn memory_entry_deserialize_legacy_payload_defaults_new_fields() {
    let raw = r#"{
        "id":"id-legacy",
        "key":"legacy_key",
        "content":"legacy content",
        "category":"core",
        "timestamp":"2026-02-01T00:00:00Z",
        "session_id":null,
        "score":0.7
    }"#;
    let parsed: MemoryEntry = serde_json::from_str(raw).unwrap();
    assert_eq!(parsed.id, "id-legacy");
    assert!(parsed.tags.is_none());
    assert!(parsed.access_count.is_none());
    assert!(parsed.useful_count.is_none());
    assert!(parsed.source.is_none());
    assert!(parsed.source_confidence.is_none());
    assert!(parsed.verification_status.is_none());
    assert!(parsed.lifecycle_state.is_none());
    assert!(parsed.compressed_from.is_none());
}
