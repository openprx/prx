#![cfg(test)]
//! Real authenticated API routing for two independent owner-approved writes.
use super::*;
use axum::response::IntoResponse;
use tower::ServiceExt;

const OWNERS: [(&str, &str, &str); 2] = [
    ("prx-owner-a-token", "prx-owner-a-session", "prx owner a content"),
    ("prx-owner-b-token", "prx-owner-b-session", "prx owner b content"),
];
const CONTEXT_SENTINELS: [&str; 2] = ["PRX_OWNER_A_CONTEXT_SENTINEL", "PRX_OWNER_B_CONTEXT_SENTINEL"];

struct OwnerCollectionProvider {
    calls: AtomicUsize,
    context_calls: AtomicUsize,
}
#[async_trait]
impl Provider for OwnerCollectionProvider {
    async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
        anyhow::bail!("fixture requires retained structured collection trace")
    }
    fn supports_native_tools(&self) -> bool {
        true
    }
    async fn chat_traced(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<crate::providers::traits::ChatTrace> {
        let input = &request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == "user")
            .context("actual collection input")?
            .content;
        // Proposal-only requests encode prior messages as context data. Owner
        // intent must come from the explicit current request, not old probes.
        let proposal_context = serde_json::from_str::<serde_json::Value>(input).ok();
        let input = proposal_context
            .as_ref()
            .and_then(|value| value.get("current_request"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or(input);
        let matching = OWNERS
            .iter()
            .filter(|owner| input.contains(owner.2))
            .collect::<Vec<_>>();
        anyhow::ensure!(
            matching.len() == 1,
            "collection input must identify exactly one fixture payload"
        );
        let current = matching.first().context("owner payload")?;
        let owner_index = OWNERS
            .iter()
            .position(|owner| owner == *current)
            .context("owner fixture index")?;
        if input.contains("context isolation probe") {
            let expected_context = CONTEXT_SENTINELS.get(owner_index).context("owner context sentinel")?;
            let transcript = request
                .messages
                .iter()
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::ensure!(
                transcript.contains(expected_context),
                "authenticated Console turn did not recall its own owner-scoped context page"
            );
            for (index, sentinel) in CONTEXT_SENTINELS.iter().enumerate() {
                if index != owner_index {
                    anyhow::ensure!(
                        !transcript.contains(sentinel),
                        "authenticated Console turn recalled another owner's workspace-visible page"
                    );
                }
            }
            self.context_calls.fetch_add(1, Ordering::SeqCst);
            let now = chrono::Utc::now();
            return Ok(crate::providers::traits::ChatTrace {
                response: ChatResponse {
                    text: Some("Owner-isolated context recalled".to_string()),
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                },
                attempts: vec![crate::llm::route_decision::ProviderAttempt {
                    seq: 1,
                    provider: "ollama".to_string(),
                    model: model.to_string(),
                    started_at: now,
                    finished_at: now,
                    status: crate::llm::route_decision::AttemptStatus::Success,
                    error_class: None,
                    error_message: None,
                }],
                final_provider: "ollama".to_string(),
                final_model: model.to_string(),
                served_member: None,
                tokens_used: crate::llm::route_decision::TokenUsage {
                    prompt_tokens: Some(12),
                    completion_tokens: Some(8),
                    total_tokens: Some(20),
                    source: crate::llm::route_decision::TokenUsageSource::Reported,
                    ..crate::llm::route_decision::TokenUsage::default()
                },
            });
        }
        let content = current.2;
        self.calls.fetch_add(1, Ordering::SeqCst);
        // Reuse only the existing controlled main-call trace constructor. Owner
        // payload is selected from the actual raw input, never global ordinal.
        let fixture = CollectionProvider {
            calls: AtomicUsize::new(0),
            multiple: false,
            abstain: false,
        };
        let mut trace = fixture.chat_traced(request, model, temperature).await?;
        let call = trace
            .response
            .tool_calls
            .first_mut()
            .context("one exact collection proposal")?;
        call.arguments = serde_json::json!({"path":TARGET,"content":content}).to_string();
        Ok(trace)
    }
}

async fn routed(
    app: &axum::Router,
    method: axum::http::Method,
    uri: &str,
    token: Option<&str>,
    payload: serde_json::Value,
) -> anyhow::Result<(StatusCode, serde_json::Value)> {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    // No ApiPrincipal insertion: the real middleware validates this bearer.
    body(
        app.clone()
            .oneshot(request.body(Body::from(payload.to_string()))?)
            .await?,
    )
    .await
}

/// A response may precede its queued terminal events. Wait for the last input's
/// durable outcome before asserting that a refused request adds no events.
async fn settled_owner_events(
    state: &AppState,
    scope: &crate::memory::MemoryPrincipal,
) -> anyhow::Result<Vec<crate::memory::MessageEvent>> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let events = state.mem.list_message_events_since(scope, 0, 128).await?;
            let last_run = events
                .iter()
                .rev()
                .find(|event| event.role == "user" && event.event_type == "message.created")
                .and_then(|event| event.run_id.as_deref())
                .context("owner input run")?;
            if events
                .iter()
                .any(|event| event.run_id.as_deref() == Some(last_run) && event.event_type == "causal.outcome.recorded")
            {
                let runtime = state.pin_turn_runtime();
                let service = runtime.decision_slot.publish(&runtime, state).await?;
                anyhow::ensure!(
                    service.drain_writes(Instant::now() + Duration::from_secs(5)).await,
                    "owner event queues did not drain"
                );
                return state.mem.list_message_events_since(scope, 0, 128).await;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("owner terminal events did not settle")?
}

async fn exercise_owners(
    state: &AppState,
    provider: &OwnerCollectionProvider,
    writes: &AtomicUsize,
    workspace: &std::path::Path,
) -> anyhow::Result<()> {
    use axum::http::Method;
    let app = crate::gateway::api::router(state.clone()).with_state(state.clone());
    let config = state.config.pin();
    let config_parent = config.effective.config_path.parent().context("fixture config root")?;
    let mut private_paths = Vec::new();
    let mut decision_ids: Vec<String> = Vec::new();
    for (index, (token, session, content)) in OWNERS.iter().enumerate() {
        let principal = crate::gateway::api::ApiPrincipal::of_token(token);
        let scope = console_runtime_envelope_for_workspace(
            workspace.to_string_lossy().as_ref(),
            session,
            "console",
            "prx_user",
        )
        .with_owner_id(principal.owner_id.clone())
        .with_owner_isolation()
        .memory_principal();
        let filesystem = crate::tools::owner_filesystem::OwnerFilesystem::open(
            config_parent,
            &scope.workspace_id,
            &principal.owner_id,
            workspace,
        )?;
        let target = filesystem.resolve(TARGET)?.operation_target().to_path_buf();
        assert!(!target.exists());
        let uri = format!("/sessions/{session}/message");
        let (status, value) = routed(
            &app,
            Method::POST,
            &uri,
            Some(token),
            serde_json::json!({"message":format!("context isolation probe for {content}")}),
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{value}");
        assert_eq!(
            value.get("reply").and_then(serde_json::Value::as_str),
            Some("Owner-isolated context recalled")
        );
        assert_eq!(provider.context_calls.load(Ordering::SeqCst), index + 1);
        let input = serde_json::json!({"message":format!("file_write: Create {TARGET} containing {content}.")});
        let before = settled_owner_events(state, &scope).await?;
        for unauthorized in [None, Some("prx-unknown-token")] {
            let (status, _) = routed(&app, Method::POST, &uri, unauthorized, input.clone()).await?;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }
        assert_eq!(
            state.mem.list_message_events_since(&scope, 0, 128).await?.len(),
            before.len()
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), index);
        assert_eq!(writes.load(Ordering::SeqCst), index);
        let (status, value) = routed(&app, Method::POST, &uri, Some(token), input).await?;
        assert_eq!(status, StatusCode::OK, "{value}");
        assert_eq!(
            value.get("status").and_then(serde_json::Value::as_str),
            Some("awaiting_approval")
        );
        let id = value
            .get("decision_id")
            .and_then(serde_json::Value::as_str)
            .context("actual owner collection ID")?
            .to_owned();
        let foreign = OWNERS
            .iter()
            .find(|(other, _, _)| other != token)
            .context("other paired owner")?
            .0;
        let (inbox_status, inbox) = routed(
            &app,
            Method::GET,
            "/sessions/approvals?limit=20",
            Some(token),
            serde_json::json!({}),
        )
        .await?;
        assert_eq!(inbox_status, StatusCode::OK, "{inbox}");
        let own_rows = inbox
            .get("approvals")
            .and_then(serde_json::Value::as_array)
            .context("approval inbox")?;
        assert!(
            own_rows.iter().any(|item| {
                item.get("decision_id").and_then(serde_json::Value::as_str) == Some(id.as_str())
                    && item.get("session_id").and_then(serde_json::Value::as_str) == Some(*session)
            }),
            "owner inbox must list this pending approval: {inbox}"
        );
        assert!(
            own_rows.iter().all(|item| {
                decision_ids.iter().all(|previous| {
                    item.get("decision_id").and_then(serde_json::Value::as_str) != Some(previous.as_str())
                })
            }),
            "resolved approvals from earlier owners stay out of this inbox: {inbox}"
        );
        let (foreign_status, foreign_inbox) = routed(
            &app,
            Method::GET,
            "/sessions/approvals?limit=20",
            Some(foreign),
            serde_json::json!({}),
        )
        .await?;
        assert_eq!(foreign_status, StatusCode::OK, "{foreign_inbox}");
        let foreign_rows = foreign_inbox
            .get("approvals")
            .and_then(serde_json::Value::as_array)
            .context("foreign approval inbox")?;
        assert!(
            foreign_rows
                .iter()
                .all(|item| { item.get("decision_id").and_then(serde_json::Value::as_str) != Some(id.as_str()) }),
            "foreign owner must not see this pending approval: {foreign_inbox}"
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), index + 1);
        assert_eq!(writes.load(Ordering::SeqCst), index);
        assert!(!target.exists());
        let approval = format!("/sessions/{session}/approvals/{id}");
        let resume = format!("{approval}/resume");
        for (method, path, payload) in [
            (Method::GET, approval.as_str(), serde_json::json!({})),
            (Method::POST, approval.as_str(), serde_json::json!({"approved":true})),
            (Method::POST, resume.as_str(), serde_json::json!({})),
        ] {
            let (status, _) = routed(&app, method, path, Some(foreign), payload).await?;
            assert_eq!(
                status,
                StatusCode::CONFLICT,
                "authenticated foreign owner cannot control this grant"
            );
        }
        assert_eq!(writes.load(Ordering::SeqCst), index);
        let before_unreviewed = settled_owner_events(state, &scope).await?;
        let (status, _) = routed(&app, Method::POST, &resume, Some(token), serde_json::json!({})).await?;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "unreviewed grant cannot create resume ingress"
        );
        assert_eq!(
            state.mem.list_message_events_since(&scope, 0, 128).await?.len(),
            before_unreviewed.len()
        );
        assert_eq!(writes.load(Ordering::SeqCst), index);
        let (status, _) = routed(&app, Method::GET, &approval, Some(token), serde_json::json!({})).await?;
        assert_eq!(status, StatusCode::OK);
        let (status, value) = routed(
            &app,
            Method::POST,
            &approval,
            Some(token),
            serde_json::json!({"approved":true}),
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{value}");
        assert_eq!(writes.load(Ordering::SeqCst), index);
        let (status, value) = routed(&app, Method::POST, &resume, Some(token), serde_json::json!({})).await?;
        assert_eq!(status, StatusCode::OK, "{value}");
        assert_eq!(
            value.get("execution_status").and_then(serde_json::Value::as_str),
            Some("succeeded")
        );
        assert_eq!(writes.load(Ordering::SeqCst), index + 1);
        assert_eq!(std::fs::read_to_string(&target)?, *content);
        assert!(!workspace.join(TARGET).exists());
        let before_repeat = settled_owner_events(state, &scope).await?;
        let (status, _) = routed(&app, Method::POST, &resume, Some(token), serde_json::json!({})).await?;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(writes.load(Ordering::SeqCst), index + 1);
        let events = state.mem.list_message_events_since(&scope, 0, 128).await?;
        assert_eq!(
            events.len(),
            before_repeat.len(),
            "repeat cannot create another source or execution"
        );
        for kind in [
            "approval.decision.pending",
            "approval.decision.reviewed",
            "approval.decision.resolved",
            "tool.execution.reserved",
            "tool.execution.finalized",
        ] {
            let rows = events
                .iter()
                .filter(|event| event.event_type == kind)
                .collect::<Vec<_>>();
            assert_eq!(rows.len(), 1, "one {kind} receipt for actual owner operation");
            let event = rows.first().context("actual receipt")?;
            assert_eq!(event.owner_id, scope.owner_id);
            assert_eq!(event.workspace_id, scope.workspace_id);
            assert_eq!(event.session_key, scope.session_key);
        }
        let resolved = events
            .iter()
            .find(|event| event.event_type == "approval.decision.resolved")
            .context("claimed grant")?;
        let claim: serde_json::Value = serde_json::from_str(
            resolved
                .raw_payload_json
                .as_deref()
                .context("actual grant claim payload")?,
        )?;
        assert_eq!(
            claim.get("disposition").and_then(serde_json::Value::as_str),
            Some("consumed")
        );
        let reserved = events
            .iter()
            .find(|event| event.event_type == "tool.execution.reserved")
            .context("execution reservation")?;
        let finalized = events
            .iter()
            .find(|event| event.event_type == "tool.execution.finalized")
            .context("execution final")?;
        assert_eq!(reserved.run_id, finalized.run_id);
        let reservation: serde_json::Value =
            serde_json::from_str(reserved.raw_payload_json.as_deref().context("reserved payload")?)?;
        let terminal: serde_json::Value =
            serde_json::from_str(finalized.raw_payload_json.as_deref().context("final payload")?)?;
        assert_eq!(
            reservation.get("operation_id").and_then(serde_json::Value::as_str),
            Some(format!("decision-resume:{id}").as_str())
        );
        assert_eq!(
            reservation.get("capability").and_then(serde_json::Value::as_str),
            Some("file_write")
        );
        assert_eq!(terminal.get("reservation"), Some(&reservation));
        assert_eq!(
            terminal.pointer("/outcome/status").and_then(serde_json::Value::as_str),
            Some("succeeded")
        );
        let finalized_turns = events
            .iter()
            .filter(|event| event.event_type == "turn.finalized")
            .collect::<Vec<_>>();
        assert_eq!(
            finalized_turns.len(),
            3,
            "context probe, collection and fresh resume finalize independently"
        );
        let resume_terminal = finalized_turns
            .iter()
            .find(|event| event.run_id == reserved.run_id)
            .context("actual resumed terminal")?;
        let resume_source = events
            .iter()
            .find(|event| {
                event.role == "user" && event.event_type == "message.created" && event.run_id == reserved.run_id
            })
            .context("actual fresh resume ingress")?;
        use crate::causal_tree::store::{CausalDecisionStore, DecisionScope, TimeWindow};
        let store = CausalDecisionStore::existing_shared_writer(workspace).context("actual owner decision store")?;
        let decision_scope = DecisionScope::Owner {
            workspace_id: store.workspace_id().context("actual decision store identity")?,
            owner_id: Arc::from(principal.owner_id.as_str()),
        };
        let decisions = store.reader().list(
            &decision_scope,
            TimeWindow {
                from_ms: 0,
                to_ms: i64::MAX,
            },
            16,
        )?;
        assert_eq!(decisions.len(), 3);
        let direct = decisions
            .iter()
            .find(|row| row.directive_kind == "answer")
            .context("owner-isolated direct-answer decision")?;
        assert_eq!(direct.session_key.as_deref(), Some(*session));
        let collection = decisions
            .iter()
            .find(|row| row.decision_id.to_string() == id)
            .context("owner collection decision")?;
        assert_eq!(collection.directive_kind, "collect_approval_arguments");
        let resumed = decisions
            .iter()
            .find(|row| row.directive_kind == "use_tool")
            .context("owner resume decision")?;
        assert_eq!(resumed.backend_kind, "compatible_http");
        assert_eq!(resumed.directive_kind, "use_tool");
        assert_eq!(resumed.session_key.as_deref(), Some(*session));
        assert_eq!(resumed.run_id, resume_source.run_id);
        assert_ne!(collection.run_id, resumed.run_id);
        let replay = store
            .reader()
            .replay_record(&decision_scope, &resumed.decision_id)?
            .context("actual resume replay binding")?;
        assert_eq!(replay.source_event_id.as_deref(), Some(resume_source.event_id.as_str()));
        assert!(resume_source.id < reserved.id && reserved.id < finalized.id && finalized.id < resume_terminal.id);
        let pending = events
            .iter()
            .find(|event| event.event_type == "approval.decision.pending")
            .context("pending collection")?;
        assert_ne!(pending.run_id, resume_source.run_id);
        private_paths.push(target);
        decision_ids.push(id);
    }
    assert_eq!(private_paths.len(), 2);
    assert_ne!(private_paths.first(), private_paths.get(1));
    assert_ne!(decision_ids.first(), decision_ids.get(1));
    for (path, (_, _, content)) in private_paths.iter().zip(OWNERS.iter()) {
        assert_eq!(
            std::fs::read_to_string(path)?,
            *content,
            "second owner must not overwrite first owner bytes"
        );
    }
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert_eq!(provider.context_calls.load(Ordering::SeqCst), 2);
    assert_eq!(writes.load(Ordering::SeqCst), 2);
    Ok(())
}

async fn run_two_owner_work(endpoint: &str, calls: &AtomicUsize, scratch: &std::path::Path) -> anyhow::Result<()> {
    let _keyring = crate::acl::approval_grant::WitnessKeyring::global_for_tests();
    let workspace = scratch.join("workspace");
    std::fs::create_dir(&workspace)?;
    let mut config = Config::default();
    config.workspace_dir = workspace.clone();
    config.config_path = scratch.join("config.toml");
    config.causal_tree.enabled = true;
    config.autonomy.level = crate::security::AutonomyLevel::Supervised;
    config.agent.compaction.mode = crate::config::AgentCompactionMode::Safeguard;
    config.default_provider = Some("ollama".into());
    config.default_model = Some(MODEL.into());
    config.providers.models.push(toml::from_str(&format!(
        "provider = \"ollama\"\nmodel = \"{MODEL}\"\noperations = [\"chat\"]\nnative_tools = true\ncontext_window = 65536\n"
    ))?);
    config.causal_tree.backend.kind =
        crate::causal_tree::config::BackendSelection::Fixed(crate::decision::BackendKind::CompatibleHttp);
    config.causal_tree.backend.endpoint = endpoint.to_owned();
    config.causal_tree.backend.model = DECISION_MODEL.to_owned();
    let priced_provider = format!("custom:{endpoint}");
    config.providers.models.push(serde_json::from_value(serde_json::json!({
        "provider":priced_provider,"model":DECISION_MODEL,"operations":["decision_endpoint"]
    }))?);
    config.cost.prices.insert(
        format!("{priced_provider}/{DECISION_MODEL}"),
        crate::config::schema::ModelPricing {
            input: 0.1,
            output: 0.2,
            cache_write: 0.3,
            cache_read: 0.1,
        },
    );
    assert_eq!(config.causal_tree.policy.extra_latency_budget_ms, 500);
    let provider = Arc::new(OwnerCollectionProvider {
        calls: AtomicUsize::new(0),
        context_calls: AtomicUsize::new(0),
    });
    let writes = Arc::new(AtomicUsize::new(0));
    let mut state = test_app_state(config.clone(), provider.clone());
    state.config = crate::config::new_shared(config.clone());
    state.model = MODEL.into();
    state.mem = Arc::new(crate::memory::SqliteMemory::new(&workspace)?);
    state.pairing = Arc::new(PairingGuard::new(
        true,
        &OWNERS
            .iter()
            .map(|(token, _, _)| (*token).to_owned())
            .collect::<Vec<_>>(),
    ));
    state.tools_registry = Arc::new(vec![Box::new(CountedWrite {
        inner: crate::tools::file_write::FileWriteTool::new(Arc::new(crate::security::SecurityPolicy::from_config(
            &config.autonomy,
            &workspace,
        ))),
        calls: Arc::clone(&writes),
    })]);
    for (index, ((token, session, content), sentinel)) in OWNERS.iter().zip(CONTEXT_SENTINELS).enumerate() {
        let principal = crate::gateway::api::ApiPrincipal::of_token(token);
        state
            .mem
            .ingest_document(crate::memory::DocumentIngestInput {
                document_id: Some(format!("context-page:console-owner-{index}")),
                workspace_id: workspace.to_string_lossy().into_owned(),
                owner_id: Some(principal.owner_id),
                topic_id: None,
                task_id: None,
                source_message_event_id: None,
                source_kind: "context_page".to_string(),
                source_uri: None,
                title: None,
                content: format!("{content} {sentinel}"),
                mime_type: Some("text/plain".to_string()),
                visibility: crate::memory::MemoryVisibility::Workspace,
                metadata_json: None,
                session_key: Some((*session).to_string()),
                agent_id: None,
                persona_id: None,
            })
            .await?;
    }
    let mut runtime = state.pin_turn_runtime();
    let resolved = crate::providers::selection::check(
        &config,
        "ollama",
        MODEL,
        &crate::providers::catalog::Requirement::chat().with(crate::providers::catalog::Feature::NativeTools),
        crate::providers::catalog::UnknownPolicy::Invocation,
    )?;
    assert_eq!(
        resolved.verification(),
        &crate::providers::catalog::Verification::Verified
    );
    let binding = crate::gateway::decision_source::GatewayProviderBinding::new(Arc::clone(&runtime.provider), resolved);
    let mutable = Arc::get_mut(&mut runtime).context("unpublished fixture runtime")?;
    mutable.provider_binding = Some(Arc::new(binding));
    mutable.decision_slot = Arc::new(crate::gateway::decision_runtime::GatewayDecisionSlot::new(
        Arc::new(crate::security::SecurityPolicy::from_config(
            &config.autonomy,
            &workspace,
        )),
        0,
    ));
    // Conversation metadata only. These actual stored anchors carry no
    // decision, pending approval, grant, selected capability or execution.
    for (token, session, _) in OWNERS {
        let owner = crate::gateway::api::ApiPrincipal::of_token(token);
        let envelope = console_runtime_envelope_for_workspace(
            workspace.to_string_lossy().as_ref(),
            session,
            "console",
            "prx_user",
        )
        .with_run_id(uuid::Uuid::new_v4().to_string())
        .with_config_generation(&runtime.config_generation)
        .with_owner_id(owner.owner_id)
        .with_owner_isolation();
        append_console_turn(
            &state,
            &envelope,
            "user",
            "Console fixture metadata anchor",
            true,
            false,
        )
        .await?;
    }
    let current = Arc::try_unwrap(runtime).map_err(|_| anyhow::anyhow!("fixture runtime unexpectedly shared"))?;
    state.turn_runtime = Some(Arc::new(crate::gateway::GatewayTurnRuntimeOwner::new(
        current,
        Arc::clone(&state.config),
        Arc::clone(&state.mem),
        None,
        None,
        None,
        None,
        Arc::clone(&state.hooks),
        #[cfg(feature = "wasm-plugins")]
        None,
    )));
    let runtime = state.pin_turn_runtime();
    let service = Arc::clone(runtime.decision_slot.publish(&runtime, &state).await?);
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        std::panic::AssertUnwindSafe(Box::pin(exercise_owners(&state, &provider, &writes, &workspace))).catch_unwind(),
    )
    .await;
    let drained = runtime
        .decision_slot
        .cleanup
        .drain(tokio::time::Instant::now() + Duration::from_secs(5))
        .await;
    let writer_drained = service.drain_writes(Instant::now() + Duration::from_secs(5)).await;
    anyhow::ensure!(
        matches!(&result, Ok(Ok(Ok(())))) && drained.is_ok() && writer_drained,
        "two-owner fixture result={result:?}; cleanup={drained:?}; writer_drained={writer_drained}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        4,
        "one controlled decision request per direct probe and fresh approved resume"
    );
    // All six whole turns remain durable after owned cleanup drains.
    for (token, session, _) in OWNERS {
        let scope = console_runtime_envelope_for_workspace(
            workspace.to_string_lossy().as_ref(),
            session,
            "console",
            "prx_user",
        )
        .with_owner_id(crate::gateway::api::ApiPrincipal::of_token(token).owner_id)
        .with_owner_isolation()
        .memory_principal();
        let events = state.mem.list_message_events_since(&scope, 0, 128).await?;
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "turn.finalized")
                .count(),
            3
        );
    }
    Ok(())
}

#[tokio::test]
async fn console_two_authenticated_owners_write_same_logical_path_with_independent_one_use_approvals()
-> anyhow::Result<()> {
    Box::pin(run_two_owner_case()).await
}
async fn run_two_owner_case() -> anyhow::Result<()> {
    let scratch = tempfile::tempdir()?;
    let calls = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let stop = tokio_util::sync::CancellationToken::new();
    let server_stop = stop.clone();
    let app = axum::Router::new()
        .route("/v1/systemone", axum::routing::post(two_owner_decision_reply))
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
        .with_state(Arc::clone(&calls));
    let mut server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(server_stop.cancelled_owned())
            .await
    });
    let result = std::panic::AssertUnwindSafe(Box::pin(run_two_owner_work(&endpoint, &calls, scratch.path())))
        .catch_unwind()
        .await;
    stop.cancel();
    let joined = match tokio::time::timeout(Duration::from_secs(5), &mut server).await {
        Ok(result) => result.is_ok_and(|served| served.is_ok()),
        Err(_) => {
            server.abort();
            let _ = tokio::time::timeout(Duration::from_secs(5), &mut server).await;
            false
        }
    };
    if !joined || !matches!(&result, Ok(Ok(()))) {
        let retained = scratch.keep();
        anyhow::bail!(
            "two-owner Console result={result:?}; listener_joined={joined}; retained={}",
            retained.display()
        );
    }
    Ok(())
}

async fn two_owner_decision_reply(
    axum::extract::State(calls): axum::extract::State<Arc<AtomicUsize>>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> axum::response::Response {
    calls.fetch_add(1, Ordering::SeqCst);
    decision_answers(&body).map_or_else(
        |_| axum::http::StatusCode::BAD_REQUEST.into_response(),
        |response| axum::Json(response).into_response(),
    )
}

mod browser_fixture;
