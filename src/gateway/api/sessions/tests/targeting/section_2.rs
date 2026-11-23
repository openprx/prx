/// MA-01b3 part 1: transcript/event writers stamp the resolved agent scope.
/// Real POST for Named X, Main mint (D-CR1), D-CR3 column probe, and refusals.
#[tokio::test]
async fn memory_writes_carry_scope() {
    let fixture = fixture(|config| {
        config.gateway.targeted_agents = true;
        config.autonomy.level = AutonomyLevel::Full;
        capture_provider_config(config, WRITE_SCOPE_PROVIDER, agent(false, &[]));
    })
    .await;
    crate::providers::test_fixture::register(
        WRITE_SCOPE_PROVIDER,
        Arc::clone(&fixture.provider) as Arc<dyn crate::providers::Provider>,
    );
    let workspace = fixture
        .state
        .config
        .pin()
        .effective
        .workspace_dir
        .to_string_lossy()
        .into_owned();
    let owner = super::super::super::ApiPrincipal::for_tests().owner_id;
    seed_named_x(std::path::Path::new(&workspace), &workspace, &owner, SESSION);
    let conn = brain(&workspace);

    const NINE: [&str; 9] = [
        "message_events",
        "conversation_turns",
        "memory_events",
        "memories",
        "documents",
        "document_chunks",
        "embedding_cache",
        "memory_drafts",
        "memory_trash",
    ];
    let mut dcr3 = Vec::new();
    for table in NINE {
        let columns = table_columns(&conn, table);
        assert!(
            columns.iter().any(|column| column == "ma_scope_id"),
            "{table} has ma_scope_id"
        );
        dcr3.push(format!("{table}: {}", columns.join(",")));
    }
    // D-CR3: no unambiguous FK identifies pre-01b3 targeted rows (target.rs has
    // selector state only). Parent FKs (message_event_id, subject_id) serve D-CR2.
    eprintln!("D-CR3 columns: {}", dcr3.join(" | "));

    let (status, value) = post(
        &fixture,
        SESSION,
        serde_json::json!({"message": "named-x", "agent": "x"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "Named POST: {value}");

    let named_events = scope_rows(
        &conn,
        "SELECT ma_scope_id FROM message_events
         WHERE session_key = ?1 AND role IN ('user', 'assistant')
         ORDER BY id",
        SESSION,
    );
    let named_turns = scope_rows(
        &conn,
        "SELECT ma_scope_id FROM conversation_turns
         WHERE session_key = ?1 AND role IN ('user', 'assistant')
         ORDER BY id",
        SESSION,
    );
    let named_mirrors = scope_rows(
        &conn,
        "SELECT ma_scope_id FROM memory_events
         WHERE session_key = ?1 AND subject_table = 'message_events'
           AND event_type IN ('message', 'message.created')
         ORDER BY id",
        SESSION,
    );
    let named_user: i64 = conn
        .query_row(
            "SELECT count(*) FROM message_events WHERE session_key = ?1 AND role = 'user' AND ma_scope_id = ?2",
            rusqlite::params![SESSION, TARGET_SCOPE],
            |row| row.get(0),
        )
        .expect("test: named user events");
    let named_assistant: i64 = conn
        .query_row(
            "SELECT count(*) FROM message_events WHERE session_key = ?1 AND role = 'assistant' AND ma_scope_id = ?2",
            rusqlite::params![SESSION, TARGET_SCOPE],
            |row| row.get(0),
        )
        .expect("test: named assistant events");
    assert!(named_user >= 1, "Named POST wrote input message_events");
    assert!(named_assistant >= 1, "Named POST wrote assistant message_events");
    assert!(!named_events.is_empty(), "Named POST wrote message_events");
    assert!(
        named_events.iter().all(|scope| scope.as_deref() == Some(TARGET_SCOPE)),
        "message_events row carries X's scope: {named_events:?}"
    );
    assert!(
        named_turns.iter().all(|scope| scope.as_deref() == Some(TARGET_SCOPE)),
        "conversation_turns row carries X's scope: {named_turns:?}"
    );
    assert!(
        named_mirrors.iter().all(|scope| scope.as_deref() == Some(TARGET_SCOPE)),
        "memory_events row carries X's scope: {named_mirrors:?}"
    );
    assert!(
        named_events.iter().all(Option::is_some)
            && named_turns.iter().all(Option::is_some)
            && named_mirrors.iter().all(Option::is_some),
        "Named X wrote zero NULL rows"
    );

    let main_incarnations: i64 = conn
        .query_row(
            "SELECT count(*) FROM agent_incarnations WHERE workspace_id = ?1 AND owner_id = ?2 AND agent_key = 'm'",
            rusqlite::params![&workspace, &owner],
            |row| row.get(0),
        )
        .expect("test: main incarnation count");
    assert_eq!(main_incarnations, 0, "D-CR1: no Main incarnation before the Main POST");

    let (status, value) = post(&fixture, SESSION, serde_json::json!({"message": "main-hi"})).await;
    assert_eq!(status, StatusCode::OK, "Main POST: {value}");

    let minted_u: String = conn
        .query_row(
            "SELECT incarnation FROM agent_incarnations
             WHERE workspace_id = ?1 AND owner_id = ?2 AND agent_key = 'm' AND state = 'active'",
            rusqlite::params![&workspace, &owner],
            |row| row.get(0),
        )
        .expect("test: D-CR1 minted m");
    let main_scopes: Vec<String> = {
        let mut statement = conn
            .prepare(
                "SELECT scope_id FROM agent_scopes
                 WHERE workspace_id = ?1 AND owner_id = ?2 AND session_key = ?3 AND agent_key = 'm'",
            )
            .expect("test: main scopes");
        statement
            .query_map(rusqlite::params![&workspace, &owner, SESSION], |row| row.get(0))
            .expect("test: main scope rows")
            .map(|value| value.expect("test: scope id"))
            .collect()
    };
    assert_eq!(main_scopes.len(), 1, "D-CR1: one Main scope");
    let main_scope = main_scopes.first().expect("test: D-CR1 one Main scope");
    let main_event_scope: Option<String> = conn
        .query_row(
            "SELECT ma_scope_id FROM message_events WHERE session_key = ?1 AND content = 'main-hi'",
            [SESSION],
            |row| row.get(0),
        )
        .expect("test: main event");
    assert_eq!(
        main_event_scope.as_deref(),
        Some(main_scope.as_str()),
        "Main rows carry the minted scope"
    );
    assert_eq!(minted_u.len(), 36, "D-CR1 minted an incarnation UUID");

    let events_before: i64 = conn
        .query_row(
            "SELECT count(*) FROM message_events WHERE session_key = ?1",
            [SESSION],
            |row| row.get(0),
        )
        .expect("test: count");
    fence_scope(&conn, TARGET_SCOPE, FENCE_ERASE);
    let (status, value) = post(
        &fixture,
        SESSION,
        serde_json::json!({"message": "fenced-x", "agent": "x"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "fenced Named: {value}");
    assert_eq!(error_code(&value), Some("agent_scope_refused"));
    let events_after: i64 = conn
        .query_row(
            "SELECT count(*) FROM message_events WHERE session_key = ?1",
            [SESSION],
            |row| row.get(0),
        )
        .expect("test: count after fence");
    assert_eq!(events_after, events_before, "fenced Named wrote zero rows");

    fence_scope(&conn, main_scope, FENCE_MAIN_ERASE);
    let (status, value) = post(&fixture, SESSION, serde_json::json!({"message": "fenced-main"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "fenced Main: {value}");
    assert_eq!(error_code(&value), Some("agent_scope_refused"));
    let fenced_main: i64 = conn
        .query_row(
            "SELECT count(*) FROM message_events WHERE content = 'fenced-main'",
            [],
            |row| row.get(0),
        )
        .expect("test: fenced main rows");
    assert_eq!(fenced_main, 0, "fenced Main wrote zero rows");

    let named = named_partition(&workspace, &owner, "x", TARGET_U);
    let envelope = crate::runtime::envelope::RuntimeEnvelope::console(workspace.clone(), SESSION.to_owned())
        .with_agent_partition(named.clone())
        .with_owner_id(owner.clone());
    let error = append_console_turn(&fixture.state, &envelope, "user", "no-parent", false, false)
        .await
        .expect_err("test: Named missing parent event");
    assert!(
        error.downcast_ref::<AgentScopeRefused>().is_some(),
        "W4 Named + missing parent: {error:#}"
    );

    conn.execute(
        "INSERT INTO agent_incarnations VALUES (?1, ?2, 'n:x', ?3, 'retired', ?4)",
        rusqlite::params![&workspace, &owner, RETIRED_U, TARGET_REV],
    )
    .expect("test: retired incarnation");
    for (label, input) in [
        (
            "retired incarnation",
            write_input(
                &workspace,
                &owner,
                Some(named_partition(&workspace, &owner, "x", RETIRED_U)),
                Some(SESSION),
                "retired",
            ),
        ),
        (
            "wrong U",
            write_input(
                &workspace,
                &owner,
                Some(named_partition(&workspace, &owner, "x", WRONG_U)),
                Some(SESSION),
                "wrong-u",
            ),
        ),
        (
            "missing session_key",
            write_input(&workspace, &owner, Some(named.clone()), None, "no-session"),
        ),
        (
            "workspace mismatch",
            write_input(
                "other-workspace",
                &owner,
                Some(named.clone()),
                Some(SESSION),
                "mismatch",
            ),
        ),
        (
            "owner mismatch",
            write_input(
                &workspace,
                "other-owner",
                Some(named.clone()),
                Some(SESSION),
                "owner-mismatch",
            ),
        ),
    ] {
        let error = fixture.memory.append_message_event(input).await.expect_err(label);
        assert!(
            error.downcast_ref::<AgentScopeRefused>().is_some(),
            "{label}: {error:#}"
        );
    }
    for content in ["retired", "wrong-u", "no-session", "mismatch", "owner-mismatch"] {
        let leaked: i64 = conn
            .query_row(
                "SELECT count(*) FROM message_events WHERE content = ?1",
                [content],
                |row| row.get(0),
            )
            .expect("test: leak");
        assert_eq!(leaked, 0, "{content} wrote zero rows");
    }

    complete_memory_family_scope_stamps().await;
}

#[tokio::test]
async fn none_targeted_scope_rejected() {
    let memory = NoneMemory::new();
    assert!(!memory.supports_agent_scope());
    let principal = history_reader(
        "prx_workspace",
        "session",
        named_partition("prx_workspace", "prx_owner", "x", TARGET_U),
    );
    let turns = memory
        .list_console_conversation_turns(&principal, "session", 10, 0)
        .await
        .expect_err("test: named turns");
    assert!(turns.downcast_ref::<AgentScopeUnsupported>().is_some(), "{turns:#}");
    let events = memory
        .list_message_events_since(&principal, 0, 10)
        .await
        .expect_err("test: named events");
    assert!(events.downcast_ref::<AgentScopeUnsupported>().is_some(), "{events:#}");

    // Job snapshots span all fixtures in this process. Identify only this
    // rejected request rather than another test's legitimately submitted job.
    let session_key = format!("prx-rejected-scope-{}", uuid::Uuid::new_v4());
    let temp = tempfile::tempdir().expect("test: tempdir");
    let mut config = Config::default();
    config.workspace_dir = temp.path().to_owned();
    config.gateway.targeted_agents = true;
    config.agents.insert("x".to_owned(), agent(false, &[]));
    let provider = Arc::new(CapturingProvider::default());
    let mut state = test_app_state(config, provider.clone());
    state.mem = Arc::new(NoneMemory::new());
    let response = post_session_message(
        State(state.clone()),
        Path(session_key.clone()),
        http_request(serde_json::json!({"message": "hi", "agent": "x"})),
    )
    .await
    .into_response();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("test: body");
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("test: json");
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(error_code(&body), Some("agent_backend_unsupported"));
    assert!(provider.requests.lock().is_empty(), "no provider access");
    assert!(state.mem.agent_registry().is_none(), "no registry row");
    let session = state
        .mem
        .get_conversation_session(&session_key)
        .await
        .expect("test: read session");
    assert!(session.is_none(), "no input event");
    let job_label = format!("gateway:session_message:{session_key}");
    assert!(
        crate::gateway::jobs::snapshot_all()
            .iter()
            .all(|job| job.label != job_label),
        "no job"
    );
}

const STORE_SCOPE_PROVIDER: &str = "custom:http://127.0.0.1/prx-ma-01b3-2-store";
const DOC_SCOPE_PROVIDER: &str = "custom:http://127.0.0.1/prx-ma-01b3-3-docs";
const TOPIC_SCOPE_PROVIDER: &str = "custom:http://127.0.0.1/prx-ma-01b3-3-topics";
const SESSION_S2: &str = "prx-target-session-s2";
const STORE_TOOL: &str = "memory_store";
const INGEST_TOOL: &str = "document_ingest";

struct ScopeCountingEmbedder {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl EmbeddingProvider for ScopeCountingEmbedder {
    fn name(&self) -> &str {
        "counting"
    }

    fn dimensions(&self) -> usize {
        4
    }

    fn model(&self) -> &str {
        "counting-v1"
    }

    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        self.calls.fetch_add(texts.len(), Ordering::SeqCst);
        Ok(texts.iter().map(|_| vec![0.1, 0.2, 0.3, 0.4]).collect())
    }
}

async fn fixture_with_scope_embedder(config: impl FnOnce(&mut Config), embed_calls: Arc<AtomicUsize>) -> Fixture {
    let temp = tempfile::tempdir().expect("test: tempdir");
    let workspace = temp.path().join("workspace");
    let private = temp.path().join("private");
    std::fs::create_dir_all(&workspace).expect("test: workspace");
    std::fs::create_dir_all(&private).expect("test: private root");
    let mut base = Config::default();
    base.workspace_dir = workspace.clone();
    base.config_path = private.join("config.toml");
    config(&mut base);
    let provider = Arc::new(CapturingProvider::default());
    let mut state = test_app_state_with_config(base, provider.clone());
    let memory = Arc::new(
        crate::memory::SqliteMemory::with_embedder(
            &workspace,
            Arc::new(ScopeCountingEmbedder { calls: embed_calls }),
            0.7,
            0.3,
            1000,
            None,
        )
        .expect("test: sqlite memory"),
    );
    state.mem = memory.clone();
    memory
        .create_conversation_session(SESSION, "api:local")
        .await
        .expect("test: session");
    memory
        .create_conversation_session(SESSION_S2, "api:local")
        .await
        .expect("test: session s2");
    Fixture {
        state,
        provider,
        memory,
        _temp: temp,
    }
}

fn install_scope_tools(fixture: &mut Fixture) {
    let workspace_dir = fixture.state.config.pin().effective.workspace_dir.clone();
    let security = Arc::new(crate::security::SecurityPolicy::from_config(
        &fixture.state.config.pin().effective.autonomy,
        &workspace_dir,
    ));
    let memory: Arc<dyn crate::memory::Memory> = fixture.memory.clone();
    fixture.state.tools_registry = Arc::new(vec![
        Box::new(MemoryStoreTool::new(Arc::clone(&memory), Arc::clone(&security))) as ToolBox,
        Box::new(DocumentIngestTool::new(workspace_dir, memory, security)) as ToolBox,
    ]);
}

async fn post_scripted(
    fixture: &mut Fixture,
    provider: &str,
    session: &str,
    body: serde_json::Value,
    tool: &'static str,
    args: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let scripted = Arc::new(ScriptedCalls::batches(vec![vec![(tool, args)]]));
    let provider_arc = Arc::clone(&scripted) as Arc<dyn crate::providers::Provider>;
    crate::providers::test_fixture::register(provider, Arc::clone(&provider_arc));
    fixture.state.provider = Arc::clone(&provider_arc);
    let result = post(fixture, session, body).await;
    drop(provider_arc);
    result
}

fn partitioned_write_ctx(workspace: &str, owner: &str, session: &str, partition: AgentPartition) -> MemoryWriteContext {
    MemoryWriteContext {
        workspace_id: Some(workspace.to_owned()),
        owner_id: Some(owner.to_owned()),
        session_key: Some(session.to_owned()),
        agent_partition: Some(partition),
        channel: Some("console".to_owned()),
        ..Default::default()
    }
}

fn configure_stamped_scope(config: &mut Config, endpoint: &str, provider: &str, tools: &[&str]) {
    config.gateway.targeted_agents = true;
    config.autonomy.level = AutonomyLevel::Full;
    config.tool_tiering.always_include = tools.iter().map(|tool| (*tool).to_owned()).collect();
    capture_provider_config(config, provider, agent(true, tools));
    crate::agent::agent::native_decision_fixture::configure(config, endpoint).expect("test: decision backend");
}

fn truncated_content_hash(text: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(text.as_bytes())
        .iter()
        .take(16)
        .fold(String::with_capacity(32), |mut hex, byte| {
            use std::fmt::Write;
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

fn assert_named_store_row(conn: &rusqlite::Connection, key: &str, content: &str) {
    let (scope, topic): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT ma_scope_id, topic_id FROM memories WHERE key = ?1",
            [key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("test: stored memories row");
    assert_eq!(
        scope.as_deref(),
        Some(TARGET_SCOPE),
        "stored memories row carries X's scope"
    );
    assert!(topic.is_none(), "X's memory has topic_id NULL");
    let fts: i64 = conn
        .query_row(
            "SELECT count(*) FROM memories_fts f JOIN memories m ON m.rowid = f.rowid WHERE m.key = ?1",
            [key],
            |row| row.get(0),
        )
        .expect("test: memories_fts");
    assert_eq!(fts, 1, "memories_fts carries X's memory");
    let salted: i64 = conn
        .query_row(
            "SELECT count(*) FROM embedding_cache WHERE ma_scope_id = ?1 AND length(content_hash) = 64",
            [TARGET_SCOPE],
            |row| row.get(0),
        )
        .expect("test: salted cache");
    assert!(salted >= 1, "salted embedding_cache row carries X's scope");
    let plain_hash = truncated_content_hash(content);
    let plain: i64 = conn
        .query_row(
            "SELECT count(*) FROM embedding_cache WHERE content_hash = ?1",
            [plain_hash],
            |row| row.get(0),
        )
        .expect("test: plain hash");
    assert_eq!(plain, 0, "no plain-hash embedding_cache row for the stored text");
}

/// Part 2 completion of `memory_writes_carry_scope`: memory-family writers.
async fn complete_memory_family_scope_stamps() {
    let driver = crate::agent::agent::native_decision_fixture::Driver::start("memory")
        .await
        .expect("test: decision fixture");
    let endpoint = driver.endpoint().to_owned();
    driver
        .run(async move {
            let mut fixture = fixture_with_scope_embedder(
                |config| configure_stamped_scope(config, &endpoint, STORE_SCOPE_PROVIDER, &[STORE_TOOL]),
                Arc::new(AtomicUsize::new(0)),
            )
            .await;
            install_scope_tools(&mut fixture);
            let workspace = fixture
                .state
                .config
                .pin()
                .effective
                .workspace_dir
                .to_string_lossy()
                .into_owned();
            let owner = super::super::super::ApiPrincipal::for_tests().owner_id;
            seed_named_x(std::path::Path::new(&workspace), &workspace, &owner, SESSION);
            let named = named_partition(&workspace, &owner, "x", TARGET_U);
            let main = main_partition(&workspace, &owner);
            let named_ctx = partitioned_write_ctx(&workspace, &owner, SESSION, named.clone());
            let main_s1 = partitioned_write_ctx(&workspace, &owner, SESSION, main.clone());
            let main_s2 = partitioned_write_ctx(&workspace, &owner, SESSION_S2, main.clone());

            let (status, value) = post_scripted(
                &mut fixture,
                STORE_SCOPE_PROVIDER,
                SESSION,
                serde_json::json!({"message": "store-x", "agent": "x"}),
                STORE_TOOL,
                serde_json::json!({"key": "named-core", "content": "hi", "category": "core"}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "Named store POST: {value}");
            let conn = brain(&workspace);
            assert_named_store_row(&conn, "named-core", "hi");

            let (status, value) = post_scripted(
                &mut fixture,
                STORE_SCOPE_PROVIDER,
                SESSION,
                serde_json::json!({"message": "store-main"}),
                STORE_TOOL,
                serde_json::json!({"key": "main-core", "content": "ok", "category": "core"}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "Main store POST: {value}");
            let main_scope: String = conn
                .query_row(
                    "SELECT ma_scope_id FROM memories WHERE key = 'main-core'",
                    [],
                    |row| row.get::<_, Option<String>>(0),
                )
                .expect("test: main store")
                .expect("test: main stamp");

            fixture
                .memory
                .store_with_context("session-key", "s1", MemoryCategory::Core, None, Some(&main_s1))
                .await
                .expect("test: Main S1");
            fixture
                .memory
                .store_with_context("session-key", "s2", MemoryCategory::Core, None, Some(&main_s2))
                .await
                .expect("test: Main S2");
            let s2_scope: String = conn
                .query_row(
                    "SELECT ma_scope_id FROM memories WHERE key = 'session-key'",
                    [],
                    |row| row.get::<_, Option<String>>(0),
                )
                .expect("test: S2 row")
                .expect("test: S2 stamp");
            let s2_id: String = conn
                .query_row(
                    "SELECT scope_id FROM agent_scopes WHERE session_key = ?1 AND agent_key = 'm'",
                    [SESSION_S2],
                    |row| row.get(0),
                )
                .expect("test: S2 scope");
            assert_eq!(s2_scope, s2_id, "Main S2 write carries the S2 scope");

            conn.execute(
                "INSERT INTO memories (id, key, content, category, created_at, updated_at) VALUES ('legacy-1', 'legacy-null', 'old', 'core', 't', 't')",
                [],
            )
            .expect("test: null legacy");
            fixture
                .memory
                .store_with_context("legacy-null", "new", MemoryCategory::Core, None, Some(&main_s1))
                .await
                .expect("test: Main over NULL");
            let stamped: Option<String> = conn
                .query_row("SELECT ma_scope_id FROM memories WHERE key = 'legacy-null'", [], |row| {
                    row.get(0)
                })
                .expect("test: legacy stamp");
            assert!(stamped.is_some(), "Main over a NULL legacy row succeeds");

            conn.execute(
                "INSERT INTO memories (id, key, content, category, created_at, updated_at) VALUES ('null-2', 'named-over-null', 'keep', 'core', 't', 't')",
                [],
            )
            .expect("test: named-over-null seed");
            let cache_before: i64 = conn
                .query_row("SELECT count(*) FROM embedding_cache", [], |row| row.get(0))
                .expect("test: cache before refuse");
            let refused = fixture
                .memory
                .store_with_context(
                    "named-over-null",
                    "take",
                    MemoryCategory::Core,
                    None,
                    Some(&named_ctx),
                )
                .await
                .expect_err("test: Named over NULL");
            assert!(
                refused.downcast_ref::<OwnerScopedStoreRefusal>().is_some(),
                "Named over NULL: {refused:#}"
            );
            let kept: String = conn
                .query_row("SELECT content FROM memories WHERE key = 'named-over-null'", [], |row| {
                    row.get(0)
                })
                .expect("test: original");
            assert_eq!(kept, "keep", "original unchanged");
            let cache_after: i64 = conn
                .query_row("SELECT count(*) FROM embedding_cache", [], |row| row.get(0))
                .expect("test: cache after refuse");
            assert_eq!(cache_after, cache_before, "a refused write leaves no embedding_cache row");

            let foreign = fixture
                .memory
                .store_with_context("foreign-key", "main-owns", MemoryCategory::Core, None, Some(&main_s1))
                .await;
            foreign.expect("test: foreign seed");
            let refused = fixture
                .memory
                .store_with_context("foreign-key", "named-take", MemoryCategory::Core, None, Some(&named_ctx))
                .await
                .expect_err("test: Named over foreign");
            assert!(
                refused.downcast_ref::<OwnerScopedStoreRefusal>().is_some(),
                "Named over foreign: {refused:#}"
            );

            let refused = fixture
                .memory
                .store("named-core", "unstamped", MemoryCategory::Core, None)
                .await
                .expect_err("test: unstamped over Named");
            assert!(
                refused.downcast_ref::<OwnerScopedStoreRefusal>().is_some(),
                "unstamped over Named: {refused:#}"
            );
            fixture
                .memory
                .store("main-core", "ctx-less-main", MemoryCategory::Core, None)
                .await
                .expect("test: ctx-less over Main");
            let kept_main: Option<String> = conn
                .query_row("SELECT ma_scope_id FROM memories WHERE key = 'main-core'", [], |row| {
                    row.get(0)
                })
                .expect("test: ctx-less main stamp");
            assert_eq!(kept_main.as_deref(), Some(main_scope.as_str()), "ctx-less upsert keeps the Main stamp");

            let draft = fixture
                .memory
                .create_memory_draft(MemoryDraftInput {
                    agent_partition: Some(named.clone()),
                    draft_id: Some("draft-named".to_owned()),
                    workspace_id: workspace.clone(),
                    owner_id: Some(owner.clone()),
                    worker_run_id: "run-1".to_owned(),
                    parent_run_id: None,
                    session_key: Some(SESSION.to_owned()),
                    agent_id: None,
                    persona_id: None,
                    key: "main-core".to_owned(),
                    content: "draft-take".to_owned(),
                    category: MemoryCategory::Core,
                    source_event_id: None,
                    visibility: MemoryVisibility::Workspace,
                    payload_json: None,
                })
                .await
                .expect("test: named draft");
            let principal = history_reader(&workspace, SESSION, named.clone());
            let merge_err = fixture
                .memory
                .merge_memory_draft(&principal, &draft.draft_id)
                .await
                .expect_err("test: Named draft merge over Main");
            assert!(
                merge_err.downcast_ref::<OwnerScopedStoreRefusal>().is_some(),
                "Named draft merge over Main: {merge_err:#}"
            );

            fixture
                .memory
                .append_retrieval_trace(RetrievalTraceInput {
                    agent_partition: Some(main.clone()),
                    trace_id: Some("trace-shared".to_owned()),
                    workspace_id: workspace.clone(),
                    owner_id: Some(owner.clone()),
                    session_key: Some(SESSION.to_owned()),
                    agent_id: None,
                    persona_id: None,
                    source: "test".to_owned(),
                    query: "q".to_owned(),
                    candidate_count: 1,
                    selected_count: 1,
                    dropped_count: 0,
                    budget_tokens: None,
                    selected_json: None,
                    dropped_json: None,
                    payload_json: None,
                })
                .await
                .expect("test: main trace");
            let trace_err = fixture
                .memory
                .append_retrieval_trace(RetrievalTraceInput {
                    agent_partition: Some(named.clone()),
                    trace_id: Some("trace-shared".to_owned()),
                    workspace_id: workspace.clone(),
                    owner_id: Some(owner.clone()),
                    session_key: Some(SESSION.to_owned()),
                    agent_id: None,
                    persona_id: None,
                    source: "test".to_owned(),
                    query: "q2".to_owned(),
                    candidate_count: 1,
                    selected_count: 1,
                    dropped_count: 0,
                    budget_tokens: None,
                    selected_json: None,
                    dropped_json: None,
                    payload_json: None,
                })
                .await
                .expect_err("test: trace collision");
            assert!(
                trace_err.downcast_ref::<AgentScopeRefused>().is_some(),
                "trace_id collision: {trace_err:#}"
            );

            fixture
                .memory
                .append_compaction_run(CompactionRunInput {
                    agent_partition: Some(main.clone()),
                    run_id: Some("run-shared".to_owned()),
                    workspace_id: workspace.clone(),
                    owner_id: Some(owner.clone()),
                    session_key: Some(SESSION.to_owned()),
                    agent_id: None,
                    persona_id: None,
                    trigger: "test".to_owned(),
                    mode: "summary".to_owned(),
                    source_message_count: 1,
                    source_token_estimate: 1,
                    summary: "s".to_owned(),
                    summary_memory_key: None,
                    source_event_ids_json: None,
                    source_event_range_json: None,
                    source_document_refs_json: None,
                    fidelity_status: "full".to_owned(),
                    payload_json: None,
                })
                .await
                .expect("test: main compaction");
            let run_err = fixture
                .memory
                .append_compaction_run(CompactionRunInput {
                    agent_partition: Some(named.clone()),
                    run_id: Some("run-shared".to_owned()),
                    workspace_id: workspace.clone(),
                    owner_id: Some(owner.clone()),
                    session_key: Some(SESSION.to_owned()),
                    agent_id: None,
                    persona_id: None,
                    trigger: "test".to_owned(),
                    mode: "summary".to_owned(),
                    source_message_count: 1,
                    source_token_estimate: 1,
                    summary: "other".to_owned(),
                    summary_memory_key: None,
                    source_event_ids_json: None,
                    source_event_range_json: None,
                    source_document_refs_json: None,
                    fidelity_status: "full".to_owned(),
                    payload_json: None,
                })
                .await
                .expect_err("test: run collision");
            assert!(
                run_err.downcast_ref::<AgentScopeRefused>().is_some(),
                "run_id collision: {run_err:#}"
            );

            let fabric = MemoryFabric::new(fixture.memory.clone(), workspace.clone());
            let envelope = crate::runtime::envelope::RuntimeEnvelope::console(workspace.clone(), SESSION.to_owned())
                .with_agent_partition(named.clone())
                .with_owner_id(owner.clone());
            let draft = fabric
                .create_worker_memory_draft(
                    &envelope.message_scope(),
                    "run-fabric",
                    "fabric-draft",
                    "fabric body",
                    MemoryCategory::Core,
                    None,
                    None,
                )
                .await
                .expect("test: fabric draft");
            let draft_scope: Option<String> = conn
                .query_row(
                    "SELECT ma_scope_id FROM memory_drafts WHERE draft_id = ?1",
                    [&draft.draft_id],
                    |row| row.get(0),
                )
                .expect("test: draft stamp");
            assert_eq!(draft_scope.as_deref(), Some(TARGET_SCOPE));
            fixture
                .memory
                .reject_memory_draft(&principal, &draft.draft_id, Some("no"))
                .await
                .expect("test: fabric reject");
            let event_scope: Option<String> = conn
                .query_row(
                    "SELECT ma_scope_id FROM memory_events WHERE subject_id = ?1 AND event_type = 'memory.draft.rejected'",
                    [&draft.draft_id],
                    |row| row.get(0),
                )
                .expect("test: reject event");
            assert_eq!(event_scope.as_deref(), Some(TARGET_SCOPE), "derived draft event equals parent scope");

            fixture
                .memory
                .move_to_trash("named-core", "test", 7)
                .await
                .expect("test: trash");
            let trash_scope: Option<String> = conn
                .query_row(
                    "SELECT ma_scope_id FROM memory_trash WHERE memory_key = 'named-core'",
                    [],
                    |row| row.get(0),
                )
                .expect("test: trash stamp");
            assert_eq!(trash_scope.as_deref(), Some(TARGET_SCOPE));
        })
        .await;
}

#[tokio::test]
async fn documents_writes_carry_scope() {
    let driver = crate::agent::agent::native_decision_fixture::Driver::start("document")
        .await
        .expect("test: decision fixture");
    let endpoint = driver.endpoint().to_owned();
    driver
        .run(async move {
            let mut fixture = fixture_with_scope_embedder(
                |config| configure_stamped_scope(config, &endpoint, DOC_SCOPE_PROVIDER, &[INGEST_TOOL]),
                Arc::new(AtomicUsize::new(0)),
            )
            .await;
            install_scope_tools(&mut fixture);
            let workspace = fixture
                .state
                .config
                .pin()
                .effective
                .workspace_dir
                .to_string_lossy()
                .into_owned();
            let owner = super::super::super::ApiPrincipal::for_tests().owner_id;
            seed_named_x(std::path::Path::new(&workspace), &workspace, &owner, SESSION);
            let named = named_partition(&workspace, &owner, "x", TARGET_U);
            let main = main_partition(&workspace, &owner);
            let body = format!(
                "# First heading\n{}\n\n# Second heading\n{}",
                "alpha ".repeat(400),
                "bravo ".repeat(400)
            );
            let (status, value) = post_scripted(
                &mut fixture,
                DOC_SCOPE_PROVIDER,
                SESSION,
                serde_json::json!({"message": "ingest-x", "agent": "x"}),
                INGEST_TOOL,
                serde_json::json!({"content": body, "title": "named-doc", "document_id": "doc-x"}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "Named ingest POST: {value}");
            let conn = brain(&workspace);
            let stored_docs: Vec<(String, Option<String>)> = conn
                .prepare("SELECT document_id, title FROM documents")
                .expect("test: list documents")
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .expect("test: document rows")
                .map(|row| row.expect("test: document row"))
                .collect();
            let (doc_id, doc_scope): (String, Option<String>) = conn
                .query_row(
                    "SELECT document_id, ma_scope_id FROM documents WHERE title = 'named-doc'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap_or_else(|error| {
                    panic!("test: documents row: {error}; post={value}; stored={stored_docs:?}")
                });
            assert_eq!(
                doc_scope.as_deref(),
                Some(TARGET_SCOPE),
                "documents row carries X's scope"
            );
            let chunks: i64 = conn
                .query_row(
                    "SELECT count(*) FROM document_chunks WHERE document_id = ?1",
                    [&doc_id],
                    |row| row.get(0),
                )
                .expect("test: chunks");
            assert!(chunks > 1, "multi-chunk body");
            let chunk_null: i64 = conn
                .query_row(
                    "SELECT count(*) FROM document_chunks WHERE document_id = ?1 AND (ma_scope_id IS NULL OR ma_scope_id != ?2)",
                    rusqlite::params![&doc_id, TARGET_SCOPE],
                    |row| row.get(0),
                )
                .expect("test: chunk stamps");
            assert_eq!(chunk_null, 0, "every document_chunks row carries X's scope");
            let events: i64 = conn
                .query_row(
                    "SELECT count(*) FROM memory_events WHERE subject_table = 'documents' AND subject_id = ?1 AND ma_scope_id = ?2",
                    rusqlite::params![&doc_id, TARGET_SCOPE],
                    |row| row.get(0),
                )
                .expect("test: document events");
            assert!(events >= 1, "document memory_events carry X's scope");
            let salted: i64 = conn
                .query_row(
                    "SELECT count(*) FROM embedding_cache WHERE ma_scope_id = ?1 AND length(content_hash) = 64",
                    [TARGET_SCOPE],
                    |row| row.get(0),
                )
                .expect("test: chunk cache");
            assert!(salted >= 1, "chunk embeddings are salted cache rows");
            let fts: i64 = conn
                .query_row(
                    "SELECT count(*) FROM document_chunks_fts f JOIN document_chunks c ON c.rowid = f.rowid WHERE c.document_id = ?1",
                    [&doc_id],
                    |row| row.get(0),
                )
                .expect("test: chunks fts");
            assert!(fts > 1, "document_chunks_fts matches");

            let fabric = MemoryFabric::new(fixture.memory.clone(), workspace.clone());
            let marker = format!("[document_ingest_ref]\ndocument_id: {doc_id}\n[/document_ingest_ref]");
            fabric
                .record_semantic_memory_from_event(
                    "semantic-x",
                    &marker,
                    MemoryCategory::Core,
                    Some(SESSION),
                    None,
                    None,
                    None,
                    Some(named.clone()),
                )
                .await
                .expect("test: semantic link");
            let link_scope: Option<String> = conn
                .query_row(
                    "SELECT ma_scope_id FROM memory_links WHERE document_id = ?1",
                    [&doc_id],
                    |row| row.get(0),
                )
                .expect("test: link stamp");
            assert_eq!(link_scope.as_deref(), Some(TARGET_SCOPE), "memory_links row carries X's scope");
            let promoted_scope: Option<String> = conn
                .query_row(
                    "SELECT ma_scope_id FROM memories WHERE key = 'semantic-x'",
                    [],
                    |row| row.get(0),
                )
                .expect("test: promoted memories stamp");
            assert_eq!(
                promoted_scope.as_deref(),
                Some(TARGET_SCOPE),
                "promoted memories row carries X's scope"
            );
            let promoted_event: Option<String> = conn
                .query_row(
                    "SELECT ma_scope_id FROM memory_events WHERE subject_id = 'semantic-x' AND event_type = 'memory.stored'",
                    [],
                    |row| row.get(0),
                )
                .expect("test: promoted memory_events stamp");
            assert_eq!(
                promoted_event.as_deref(),
                Some(TARGET_SCOPE),
                "promoted memory_events row carries X's scope"
            );

            fixture
                .memory
                .ingest_document(crate::memory::DocumentIngestInput {
                    agent_partition: None,
                    document_id: Some("doc-null".to_owned()),
                    workspace_id: workspace.clone(),
                    owner_id: Some(owner.clone()),
                    topic_id: None,
                    task_id: None,
                    source_message_event_id: None,
                    source_kind: "test".to_owned(),
                    source_uri: None,
                    title: Some("null".to_owned()),
                    content: "null document".to_owned(),
                    mime_type: None,
                    visibility: MemoryVisibility::Workspace,
                    metadata_json: None,
                    session_key: Some(SESSION.to_owned()),
                    agent_id: None,
                    persona_id: None,
                })
                .await
                .expect("test: null document");
            fixture
                .memory
                .link_memory_source(crate::memory::MemoryLinkInput {
                    agent_partition: Some(main.clone()),
                    session_key: Some(SESSION.to_owned()),
                    link_id: Some("link-main-null".to_owned()),
                    workspace_id: workspace.clone(),
                    owner_id: Some(owner.clone()),
                    memory_key: Some("semantic-x".to_owned()),
                    memory_event_id: None,
                    message_event_id: None,
                    document_id: "doc-null".to_owned(),
                    chunk_id: None,
                    link_type: "derived_from".to_owned(),
                    payload_json: None,
                })
                .await
                .expect("test: Main may link a NULL document");
            let named_null = fixture
                .memory
                .link_memory_source(crate::memory::MemoryLinkInput {
                    agent_partition: Some(named.clone()),
                    session_key: Some(SESSION.to_owned()),
                    link_id: Some("link-named-null".to_owned()),
                    workspace_id: workspace.clone(),
                    owner_id: Some(owner.clone()),
                    memory_key: Some("semantic-x".to_owned()),
                    memory_event_id: None,
                    message_event_id: None,
                    document_id: "doc-null".to_owned(),
                    chunk_id: None,
                    link_type: "derived_from".to_owned(),
                    payload_json: None,
                })
                .await
                .expect_err("test: Named may not link a NULL document");
            assert!(
                named_null.downcast_ref::<AgentScopeRefused>().is_some(),
                "Named NULL document link: {named_null:#}"
            );

            fixture
                .memory
                .ingest_document(crate::memory::DocumentIngestInput {
                    agent_partition: Some(main.clone()),
                    document_id: Some("doc-collide".to_owned()),
                    workspace_id: workspace.clone(),
                    owner_id: Some(owner.clone()),
                    topic_id: None,
                    task_id: None,
                    source_message_event_id: None,
                    source_kind: "test".to_owned(),
                    source_uri: None,
                    title: Some("main".to_owned()),
                    content: "main document".to_owned(),
                    mime_type: None,
                    visibility: MemoryVisibility::Workspace,
                    metadata_json: None,
                    session_key: Some(SESSION.to_owned()),
                    agent_id: None,
                    persona_id: None,
                })
                .await
                .expect("test: main collide seed");
            let collide = fixture
                .memory
                .ingest_document(crate::memory::DocumentIngestInput {
                    agent_partition: Some(named.clone()),
                    document_id: Some("doc-collide".to_owned()),
                    workspace_id: workspace.clone(),
                    owner_id: Some(owner.clone()),
                    topic_id: None,
                    task_id: None,
                    source_message_event_id: None,
                    source_kind: "test".to_owned(),
                    source_uri: None,
                    title: Some("named".to_owned()),
                    content: "named document".to_owned(),
                    mime_type: None,
                    visibility: MemoryVisibility::Workspace,
                    metadata_json: None,
                    session_key: Some(SESSION.to_owned()),
                    agent_id: None,
                    persona_id: None,
                })
                .await
                .expect_err("test: cross-scope document_id");
            assert!(
                collide.downcast_ref::<AgentScopeRefused>().is_some(),
                "cross-scope document_id: {collide:#}"
            );
            let title: String = conn
                .query_row("SELECT title FROM documents WHERE document_id = 'doc-collide'", [], |row| {
                    row.get(0)
                })
                .expect("test: original title");
            assert_eq!(title, "main", "original unchanged");

            let (status, value) = post_scripted(
                &mut fixture,
                DOC_SCOPE_PROVIDER,
                SESSION,
                serde_json::json!({"message": "ingest-main"}),
                INGEST_TOOL,
                serde_json::json!({"content": "main control document", "title": "main-doc", "document_id": "doc-main"}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "Main ingest POST: {value}");
            let main_doc: Option<String> = conn
                .query_row(
                    "SELECT ma_scope_id FROM documents WHERE title = 'main-doc'",
                    [],
                    |row| row.get(0),
                )
                .expect("test: main document");
            assert!(main_doc.is_some(), "Main control document is stamped");
        })
        .await;
}

#[tokio::test]
async fn topics_writes_carry_scope() {
    let driver = crate::agent::agent::native_decision_fixture::Driver::start("memory")
        .await
        .expect("test: decision fixture");
    let endpoint = driver.endpoint().to_owned();
    driver
        .run(async move {
            let mut fixture = fixture_with_scope_embedder(
                |config| configure_stamped_scope(config, &endpoint, TOPIC_SCOPE_PROVIDER, &[STORE_TOOL]),
                Arc::new(AtomicUsize::new(0)),
            )
            .await;
            install_scope_tools(&mut fixture);
            let workspace = fixture
                .state
                .config
                .pin()
                .effective
                .workspace_dir
                .to_string_lossy()
                .into_owned();
            let owner = super::super::super::ApiPrincipal::for_tests().owner_id;
            seed_named_x(std::path::Path::new(&workspace), &workspace, &owner, SESSION);
            let main = main_partition(&workspace, &owner);
            let shared = "prx shared topic collision body for named isolation";
            let seed_ctx = MemoryWriteContext {
                workspace_id: Some(workspace.clone()),
                owner_id: Some(owner.clone()),
                session_key: Some(SESSION.to_owned()),
                agent_partition: None,
                channel: Some("console".to_owned()),
                ..Default::default()
            };
            fixture
                .memory
                .store_with_context(
                    "shared-topic-seed",
                    shared,
                    MemoryCategory::Core,
                    None,
                    Some(&seed_ctx),
                )
                .await
                .expect("test: shared topic seed");
            let conn = brain(&workspace);
            let (shared_id, shared_updated, shared_scope): (String, String, Option<String>) = conn
                .query_row(
                    "SELECT id, updated_at, ma_scope_id FROM topics ORDER BY id LIMIT 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .expect("test: shared topic");

            let (status, value) = post_scripted(
                &mut fixture,
                TOPIC_SCOPE_PROVIDER,
                SESSION,
                serde_json::json!({"message": "topic-x", "agent": "x"}),
                STORE_TOOL,
                serde_json::json!({"key": "named-shared", "content": shared, "category": "core"}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "Named shared-topic POST: {value}");
            let (updated_at, scope): (String, Option<String>) = conn
                .query_row(
                    "SELECT updated_at, ma_scope_id FROM topics WHERE id = ?1",
                    [&shared_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("test: shared after");
            assert_eq!(updated_at, shared_updated, "shared-topic-unchanged updated_at");
            assert_eq!(scope, shared_scope, "shared-topic-unchanged ma_scope_id");
            let named_topic: Option<String> = conn
                .query_row(
                    "SELECT topic_id FROM memories WHERE key = 'named-shared'",
                    [],
                    |row| row.get(0),
                )
                .expect("test: named topic_id");
            assert!(named_topic.is_none(), "X's memory has topic_id NULL");

            let unique = "prx unique named topic body for isolation test";
            let (status, value) = post_scripted(
                &mut fixture,
                TOPIC_SCOPE_PROVIDER,
                SESSION,
                serde_json::json!({"message": "topic-x-unique", "agent": "x"}),
                STORE_TOOL,
                serde_json::json!({"key": "named-unique", "content": unique, "category": "core"}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "Named unique-topic POST: {value}");
            let x_topics: i64 = conn
                .query_row(
                    "SELECT count(*) FROM topics WHERE ma_scope_id = ?1",
                    [TARGET_SCOPE],
                    |row| row.get(0),
                )
                .expect("test: X topics");
            assert_eq!(x_topics, 1, "X's memory_store creates a topics row stamped X");
            let fts: i64 = conn
                .query_row(
                    "SELECT count(*) FROM topics_fts f JOIN topics t ON t.rowid = f.rowid WHERE t.ma_scope_id = ?1",
                    [TARGET_SCOPE],
                    |row| row.get(0),
                )
                .expect("test: topics_fts");
            assert_eq!(fts, 1, "topics_fts has X's topic only");
            let participants: i64 = conn
                .query_row(
                    "SELECT count(*) FROM topic_participants WHERE topic_id IN (SELECT id FROM topics WHERE ma_scope_id = ?1)",
                    [TARGET_SCOPE],
                    |row| row.get(0),
                )
                .expect("test: participants");
            assert_eq!(participants, 0, "topic_participants have no X rows");
            let aliases: i64 = conn
                .query_row("SELECT count(*) FROM topic_aliases", [], |row| row.get(0))
                .expect("test: aliases");
            assert_eq!(aliases, 0, "topic_aliases have no X rows");

            let main_ctx = partitioned_write_ctx(&workspace, &owner, SESSION, main);
            fixture
                .memory
                .store_with_context("main-unique", "prx main unique topic body for reuse", MemoryCategory::Core, None, Some(&main_ctx))
                .await
                .expect("test: Main unique topic");
            fixture
                .memory
                .store_with_context("main-shared", shared, MemoryCategory::Core, None, Some(&main_ctx))
                .await
                .expect("test: Main reuses NULL/Main topic");
            let main_reuse: Option<String> = conn
                .query_row(
                    "SELECT topic_id FROM memories WHERE key = 'main-shared'",
                    [],
                    |row| row.get(0),
                )
                .expect("test: main reuse");
            assert_eq!(main_reuse.as_deref(), Some(shared_id.as_str()), "Main reuses a NULL/Main topic as today");
        })
        .await;
}
