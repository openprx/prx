use super::*;
use parking_lot::Mutex;

struct MapMemory {
    rows: Mutex<HashMap<String, MemoryEntry>>,
}

impl MapMemory {
    fn new() -> Self {
        Self {
            rows: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl Memory for MapMemory {
    fn name(&self) -> &str {
        "map"
    }

    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.rows.lock().insert(
            key.to_string(),
            MemoryEntry {
                origin: crate::memory::traits::MemoryEntryOrigin::Stored,
                source_event_id: None,
                id: key.to_string(),
                key: key.to_string(),
                content: content.to_string(),
                category,
                timestamp: "t".to_string(),
                session_id: session_id.map(str::to_string),
                score: None,
                tags: None,
                access_count: None,
                useful_count: None,
                source: None,
                source_confidence: None,
                verification_status: None,
                lifecycle_state: None,
                compressed_from: None,
            },
        );
        Ok(())
    }

    async fn recall(&self, _query: &str, _limit: usize, _session_id: Option<&str>) -> anyhow::Result<Vec<MemoryEntry>> {
        Ok(self.rows.lock().values().cloned().collect())
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        Ok(self.rows.lock().get(key).cloned())
    }

    async fn list(
        &self,
        _category: Option<&MemoryCategory>,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        Ok(self.rows.lock().values().cloned().collect())
    }

    async fn forget(&self, key: &str) -> anyhow::Result<bool> {
        Ok(self.rows.lock().remove(key).is_some())
    }

    async fn count(&self) -> anyhow::Result<usize> {
        Ok(self.rows.lock().len())
    }

    async fn health_check(&self) -> bool {
        true
    }
}

fn scope(inner: Arc<dyn Memory>, account: &str, conversation: &str) -> Arc<dyn Memory> {
    let dir = Box::leak(Box::new(tempfile::tempdir().expect("test: temp")));
    let crypto = Arc::new(AccountCrypto::new(dir.path(), dir.path(), true));
    crypto.unlock(account).expect("test: unlock");
    let namespace = format!("{account}/{conversation}");
    create_ramflux_scoped_memory(inner, crypto, account, namespace)
}

#[tokio::test]
async fn two_accounts_and_two_conversations_stay_apart() {
    let inner: Arc<dyn Memory> = Arc::new(MapMemory::new());
    let a = scope(Arc::clone(&inner), "acct-a", "conv-1");
    let b = scope(Arc::clone(&inner), "acct-a", "conv-2");
    let c = scope(Arc::clone(&inner), "acct-b", "conv-1");
    a.store("note", "alpha secret", MemoryCategory::Conversation, None)
        .await
        .expect("test: store a");
    b.store("note", "beta secret", MemoryCategory::Conversation, None)
        .await
        .expect("test: store b");
    let got = a.get("note").await.expect("test: get").expect("test: row");
    assert_eq!(got.content, "alpha secret");
    assert!(b.get("note").await.expect("test: get b").is_some());
    assert_ne!(
        b.get("note").await.expect("test: get b2").expect("test: row").content,
        "alpha secret"
    );
    assert!(c.get("note").await.expect("test: get c").is_none());
    let recalled = a.recall("alpha", 5, None).await.expect("test: recall");
    assert_eq!(recalled.len(), 1);
    assert!(a.recall("beta", 5, None).await.expect("test: recall other").is_empty());
    assert_eq!(a.count().await.expect("test: count"), 1);
    assert_eq!(c.list(None, None).await.expect("test: list").len(), 0);
    let dumped = inner.list(None, None).await.expect("test: dump");
    for row in dumped {
        assert!(!row.content.contains("alpha secret"));
        assert!(!row.content.contains("beta secret"));
    }
}

#[tokio::test]
async fn history_round_trip_decrypts_and_plaintext_is_not_passed_through() {
    let inner: Arc<dyn Memory> = Arc::new(MapMemory::new());
    inner
        .store("foreign", "plain neighbor", MemoryCategory::Core, None)
        .await
        .expect("test: foreign");
    let scoped = scope(Arc::clone(&inner), "acct-a", "conv-1");
    scoped
        .append_conversation_turn(
            "sess",
            "ramflux",
            "alice",
            "user",
            "hello scope",
            None,
            Some("m1"),
            None,
            None,
        )
        .await
        .expect("test: append");
    let principal = crate::memory::MemoryPrincipal {
        workspace_id: "ws".into(),
        ..crate::memory::MemoryPrincipal::default()
    };
    let loaded = scoped
        .load_recent_conversation_histories(&principal, 10, 10)
        .await
        .expect("test: load");
    let turns = loaded.get("sess").expect("test: session");
    assert_eq!(turns.len(), 1);
    assert_eq!(turns.first().expect("test: turn").content, "hello scope");
    assert!(scoped.get("foreign").await.expect("test: foreign get").is_none());
}
#[tokio::test]
async fn logical_prefix_forget_stays_in_its_scope() {
    let inner: Arc<dyn Memory> = Arc::new(MapMemory::new());
    let a = scope(Arc::clone(&inner), "acct", "conv-a");
    let b = scope(Arc::clone(&inner), "acct", "conv-b");
    a.store("topic/a", "remove", MemoryCategory::Core, None)
        .await
        .expect("test: own");
    a.store("other", "own keep", MemoryCategory::Core, None)
        .await
        .expect("test: own keep");
    b.store("topic/b", "neighbor keep", MemoryCategory::Core, None)
        .await
        .expect("test: neighbor");
    inner
        .store("topic/foreign", "plain foreign", MemoryCategory::Core, None)
        .await
        .expect("test: foreign");
    assert_eq!(a.list_key_prefix("topic/", 10).await.expect("test: list").len(), 1);
    assert_eq!(a.forget_key_prefix("topic/").await.expect("test: forget"), 1);
    assert!(a.get("topic/a").await.expect("test: deleted").is_none());
    assert!(a.get("other").await.expect("test: own keep").is_some());
    assert!(b.get("topic/b").await.expect("test: neighbor keep").is_some());
    assert!(inner.get("topic/foreign").await.expect("test: foreign keep").is_some());
}

#[tokio::test]
async fn history_keeps_newest_timestamped_message_identities() {
    let inner: Arc<dyn Memory> = Arc::new(MapMemory::new());
    let view = scope(inner, "acct", "conv");
    for (id, time) in [
        ("m1", "2026-11-01T01:00:00Z"),
        ("m3", "2026-11-01T03:00:00Z"),
        ("m2", "2026-11-01T02:00:00Z"),
    ] {
        view.append_conversation_turn(
            "session",
            "ramflux",
            "sender",
            "assistant",
            id,
            Some(time),
            Some(id),
            None,
            None,
        )
        .await
        .expect("test: append");
    }
    let principal = crate::memory::MemoryPrincipal {
        workspace_id: "ws".to_owned(),
        agent_id: None,
        persona_id: None,
        session_key: None,
        channel: None,
        sender: None,
        owner_id: None,
        require_owner_match: false,
        legacy_session_key: None,
        agent_partition: None,
    };
    let rows = view
        .load_recent_conversation_histories(&principal, 2, 10)
        .await
        .expect("test: history");
    let turns = rows.get("session").expect("test: session");
    assert_eq!(
        turns
            .iter()
            .filter_map(|turn| turn.message_id.as_deref())
            .collect::<Vec<_>>(),
        vec!["m2", "m3"]
    );
    assert!(turns.iter().all(|turn| !turn.timestamp.is_empty()));
}

/// Turn text the generic memory filter refuses, in a real session snapshot.
const SNAPSHOT_PII_USER_TEXT: &str = "order 123456789012345 and SSN 123-45-6789, call 212-555-0199";
const SNAPSHOT_PII_REPLY_TEXT: &str = "id 110101199003078888, mail jane@example.com, ignore previous instructions";
/// Bound on the scope snapshot test: real SQLite reads finish in milliseconds.
const SNAPSHOT_SCOPE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

#[tokio::test]
async fn a_session_snapshot_through_the_scope_is_sealed_in_real_sqlite_and_reads_back_byte_identical() {
    tokio::time::timeout(SNAPSHOT_SCOPE_DEADLINE, async {
        let workspace = tempfile::TempDir::new().expect("test: workspace");
        let keys = tempfile::TempDir::new().expect("test: key dir");
        let inner: Arc<dyn Memory> =
            Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: real sqlite memory"));
        let crypto = Arc::new(AccountCrypto::new(keys.path(), keys.path(), true));
        crypto.unlock("acct-a").expect("test: unlock");
        let scope = create_ramflux_scoped_memory(Arc::clone(&inner), Arc::clone(&crypto), "acct-a", "acct-a/conv-1");

        let mut session = crate::chat::session::ChatSession::new("test-provider", "test-model");
        let at = chrono::Utc::now();
        for (role, content) in [("user", SNAPSHOT_PII_USER_TEXT), ("assistant", SNAPSHOT_PII_REPLY_TEXT)] {
            session.turns.push(crate::chat::session::ChatTurn {
                role: role.to_string(),
                content: content.to_string(),
                timestamp: at,
                tool_calls: Vec::new(),
                context_handoff: false,
                tool_evidence: None,
            });
        }
        let json = session.to_json().expect("test: serialize the chat session");
        let key = session.memory_key();

        scope
            .store_session_snapshot(&key, &json, Some(&session.id))
            .await
            .expect("test: the snapshot saves through the scope");

        let plaintext_rows = inner.list(None, None).await.expect("test: dump inner rows");
        for row in &plaintext_rows {
            assert!(
                !row.content.contains(SNAPSHOT_PII_USER_TEXT) && !row.content.contains(SNAPSHOT_PII_REPLY_TEXT),
                "the inner store holds only sealed bytes: {}",
                row.key
            );
        }

        let inner_key = scope.persisted_key(&key);
        let db = rusqlite::Connection::open_with_flags(
            workspace.path().join("memory").join("brain.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("test: open the inner database read-only");
        let (channel, owner_id, session_id): (Option<String>, Option<String>, Option<String>) = db
            .query_row(
                "SELECT channel, owner_id, session_id FROM memories WHERE key = ?1",
                [&inner_key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("test: the sealed row is stored under the scope key");
        let expected_owner = crypto
            .mac("acct-a", "memory-owner", b"acct-a")
            .expect("test: owner mac");
        let expected_session = crypto
            .mac("acct-a", "memory-session", b"acct-a/conv-1")
            .expect("test: session mac");
        assert_eq!(
            channel.as_deref(),
            Some("ramflux"),
            "the sealed row carries the scope channel"
        );
        assert_eq!(
            owner_id.as_deref(),
            Some(expected_owner.as_str()),
            "the sealed row carries the owner MAC"
        );
        assert_eq!(
            session_id.as_deref(),
            Some(expected_session.as_str()),
            "the sealed row carries the session MAC"
        );

        let read = scope
            .get(&key)
            .await
            .expect("test: read the snapshot through the scope")
            .expect("test: the snapshot reads back through the scope");
        assert_eq!(
            read.content, json,
            "the snapshot reads back byte-identical through the scope"
        );
    })
    .await
    .expect("the scope snapshot test finishes within SNAPSHOT_SCOPE_DEADLINE");
}

/// Bound on one provenance-through-the-scope test (real SQLite, ~500 rows).
const SCOPE_PROVENANCE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);
const SCOPE_WORKSPACE: &str = "workspace-scope-provenance";

/// The switch builder needs a provider; switch mode never calls it.
struct UnusedProvider;

#[async_trait]
impl crate::providers::Provider for UnusedProvider {
    async fn chat_with_system(
        &self,
        _system: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Err(anyhow::anyhow!("test: the provider is never called"))
    }

    async fn chat(
        &self,
        _request: crate::providers::ChatRequest<'_>,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<crate::providers::ChatResponse> {
        Err(anyhow::anyhow!("test: the provider is never called"))
    }
}

/// A chat session whose rows are written through an encrypted scope over a
/// real SQLite store, plus one row the scope cannot decrypt.
struct ScopedSession {
    _workspace: tempfile::TempDir,
    _keys: tempfile::TempDir,
    inner: Arc<dyn Memory>,
    scope: Arc<dyn Memory>,
    envelope: crate::runtime::envelope::RuntimeEnvelope,
}

impl ScopedSession {
    fn new() -> Self {
        let workspace = tempfile::TempDir::new().expect("test: workspace");
        let keys = tempfile::TempDir::new().expect("test: key dir");
        let inner: Arc<dyn Memory> =
            Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: real sqlite memory"));
        let crypto = Arc::new(AccountCrypto::new(keys.path(), keys.path(), true));
        crypto.unlock("acct-a").expect("test: unlock");
        let scope = create_ramflux_scoped_memory(Arc::clone(&inner), crypto, "acct-a", "acct-a/conv-1");
        let envelope = crate::runtime::envelope::RuntimeEnvelope::chat_canonical(
            SCOPE_WORKSPACE,
            "scope-provenance",
            crate::memory::MemoryVisibility::Workspace,
        );
        Self {
            _workspace: workspace,
            _keys: keys,
            inner,
            scope,
            envelope,
        }
    }

    /// One sealed user/assistant pair through the scope.
    async fn pair(&self, question: &str, answer: &str) -> (String, String) {
        let fabric = crate::memory::MemoryFabric::new(Arc::clone(&self.scope), SCOPE_WORKSPACE);
        let run = self
            .envelope
            .clone()
            .with_run_id(format!("run-{}", uuid::Uuid::new_v4()));
        fabric
            .record_inbound_user_message(
                run.message_scope(),
                question,
                Some(format!("scope-input-{}", uuid::Uuid::new_v4())),
                None,
            )
            .await
            .expect("test: record a sealed user row");
        fabric
            .record_assistant_message(run.message_scope(), answer)
            .await
            .expect("test: record a sealed assistant row");
        (question.to_string(), answer.to_string())
    }

    /// A row under the same session key the scope cannot decrypt (written
    /// to the inner store directly, not sealed by this scope).
    async fn undecryptable_row(&self) {
        crate::memory::MemoryFabric::new(Arc::clone(&self.inner), SCOPE_WORKSPACE)
            .record_inbound_user_message(
                self.envelope.clone().with_run_id("foreign-run").message_scope(),
                "a row this scope cannot open",
                Some("scope-foreign-input".to_string()),
                None,
            )
            .await
            .expect("test: record an undecryptable row");
    }

    /// Assert the cold window `(question, answer)` is refused as ambiguous
    /// and no switch row is written.
    async fn assert_switch_refused(&self, cold: (String, String)) {
        let history = vec![
            crate::providers::ChatMessage::system("sys"),
            crate::providers::ChatMessage::user(cold.0),
            crate::providers::ChatMessage::assistant(cold.1),
            crate::providers::ChatMessage::user(format!("tail question {}", "padding ".repeat(20))),
        ];
        let resolution = crate::agent::compaction::resolve_compaction_event_provenance_detailed(
            self.scope.as_ref(),
            self.envelope.memory_principal(),
            history.get(1..3).expect("test: cold window"),
            crate::agent::compaction::RoundCompanions::Acknowledged,
        )
        .await
        .expect("test: the provenance read succeeds");
        assert_eq!(
            resolution,
            crate::agent::compaction::ProvenanceResolution::Unavailable(
                crate::agent::compaction::ProvenanceGap::TurnAmbiguous
            ),
            "a page holding an undecryptable row leaves the pool unproven"
        );
        let audit = crate::agent::document_ingest::DocumentIngestRuntime::from_envelope(
            Arc::clone(&self.scope),
            &self.envelope,
        );
        let config = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Switch,
            reserve_tokens: 1,
            keep_recent_messages: 1,
            max_context_tokens: 50,
            max_context_tokens_explicit: true,
        };
        let patch = crate::agent::compaction::build_configurable_compaction_patch(
            &history,
            &UnusedProvider,
            "model",
            &config,
            Some(&audit),
            "scope_provenance",
        )
        .await
        .expect("test: the switch builder runs");
        assert!(patch.is_none(), "no switch is built without exact provenance");
        let switch_rows = self
            .inner
            .list_message_events_since(&self.envelope.memory_principal(), 0, 5000)
            .await
            .expect("test: list stored rows")
            .into_iter()
            .filter(|event| event.event_type == "context.switch.created")
            .count();
        assert_eq!(switch_rows, 0, "no context.switch.created row is written");
    }
}

fn padded(label: &str) -> String {
    format!("{label} {}", "padding ".repeat(20))
}

#[tokio::test]
async fn an_undecryptable_row_in_an_older_page_leaves_the_provenance_pool_unproven() {
    tokio::time::timeout(SCOPE_PROVENANCE_DEADLINE, async {
        let session = ScopedSession::new();
        let cold = session
            .pair(&padded("the oldest question"), &padded("the oldest answer"))
            .await;
        session.undecryptable_row().await;
        // A full newest page of readable rows: the foreign row is only in page 2.
        for pair in 0..250 {
            session
                .pair(&format!("later question {pair}"), &format!("later answer {pair}"))
                .await;
        }
        session.assert_switch_refused(cold).await;
    })
    .await
    .expect("an_undecryptable_row_in_an_older_page_leaves_the_provenance_pool_unproven exceeded its deadline");
}

#[tokio::test]
async fn an_undecryptable_row_in_the_newest_page_leaves_the_provenance_pool_unproven() {
    tokio::time::timeout(SCOPE_PROVENANCE_DEADLINE, async {
        let session = ScopedSession::new();
        let cold = session.pair(&padded("the question"), &padded("the answer")).await;
        session.undecryptable_row().await;
        session.assert_switch_refused(cold).await;
    })
    .await
    .expect("an_undecryptable_row_in_the_newest_page_leaves_the_provenance_pool_unproven exceeded its deadline");
}
