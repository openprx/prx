#![cfg(test)]
//! Actual Console source, pending owner review and fresh exact-call resume.
use super::*;
use anyhow::Context;
use futures_util::FutureExt;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Case {
    Controlled,
    Builtin,
    Multiple,
}
const DECISION_MODEL: &str = "prx-console-decision";
const MODEL: &str = "prx-local-model";
const SESSION: &str = "prx-console-approval";
const TARGET: &str = "prx-approved.txt";
const CONTENT: &str = "approved console content";

struct CountedWrite {
    inner: crate::tools::file_write::FileWriteTool,
    calls: Arc<AtomicUsize>,
}
#[async_trait]
impl crate::tools::Tool for CountedWrite {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        self.inner.parameters_schema()
    }
    fn tier(&self) -> crate::tools::traits::ToolTier {
        self.inner.tier()
    }
    fn categories(&self) -> &'static [crate::tools::traits::ToolCategory] {
        self.inner.categories()
    }
    fn supports_owner_filesystem(&self) -> bool {
        self.inner.supports_owner_filesystem()
    }
    async fn execute_with_cancellation(
        &self,
        args: serde_json::Value,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> anyhow::Result<crate::tools::ToolResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.execute_with_cancellation(args, cancellation).await
    }
    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.execute(args).await
    }
}
struct CollectionProvider {
    calls: AtomicUsize,
    multiple: bool,
}
#[async_trait]
impl Provider for CollectionProvider {
    async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
        anyhow::bail!("fixture requires actual retained traced main call")
    }
    fn supports_native_tools(&self) -> bool {
        true
    }
    async fn chat_traced(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        _: f64,
    ) -> anyhow::Result<crate::providers::traits::ChatTrace> {
        use crate::llm::route_decision::{AttemptStatus, ProviderAttempt, TokenUsage, TokenUsageSource};
        let ordinal = self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(model, MODEL);
        let calls = if ordinal == 0 {
            let tools = request.tools.context("collection proposal schema missing")?;
            assert_eq!(tools.len(), 1);
            assert_eq!(tools.first().unwrap().name, "file_write");
            (0..if self.multiple { 2 } else { 1 })
                .map(|index| crate::providers::ToolCall {
                    id: format!("prx-collection-{index}"),
                    name: "file_write".into(),
                    arguments: serde_json::json!({"path":TARGET,"content":CONTENT}).to_string(),
                })
                .collect()
        } else {
            Vec::new()
        };
        let now = Utc::now();
        Ok(crate::providers::traits::ChatTrace {
            response: ChatResponse {
                text: Some("Approval fixture response".into()),
                tool_calls: calls,
                reasoning_content: None,
            },
            attempts: vec![ProviderAttempt {
                seq: 1,
                provider: "ollama".into(),
                model: model.into(),
                started_at: now,
                finished_at: now,
                status: AttemptStatus::Success,
                error_class: None,
                error_message: None,
            }],
            final_provider: "ollama".into(),
            final_model: model.into(),
            served_member: None,
            tokens_used: TokenUsage {
                prompt_tokens: Some(12),
                completion_tokens: Some(8),
                total_tokens: Some(20),
                source: TokenUsageSource::Reported,
                ..TokenUsage::default()
            },
        })
    }
}
fn request(body: serde_json::Value, token: Option<&str>) -> Request {
    let mut request = Request::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    if let Some(token) = token {
        request
            .extensions_mut()
            .insert(super::super::super::ApiPrincipal::of_token(token));
    }
    request
}
async fn body(response: Response) -> anyhow::Result<(StatusCode, serde_json::Value)> {
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024).await?;
    Ok((status, serde_json::from_slice(&bytes)?))
}

async fn exercise(
    state: AppState,
    provider: Arc<CollectionProvider>,
    writes: Arc<AtomicUsize>,
    path: PathBuf,
    scope: crate::memory::MemoryPrincipal,
    case: Case,
) -> anyhow::Result<()> {
    let config = state.config.pin();
    let config_parent = config.effective.config_path.parent().context("fixture config root")?;
    let filesystem = crate::tools::owner_filesystem::OwnerFilesystem::open(
        config_parent,
        &scope.workspace_id,
        scope.owner_id.as_deref().context("actual Console owner")?,
        &path,
    )?;
    let private_target = filesystem.resolve(TARGET)?.operation_target().to_path_buf();
    assert!(!private_target.exists());
    assert!(!config_parent.join("owner-files").exists());
    let response = post_session_message(
        State(state.clone()),
        Path(SESSION.into()),
        request(
            serde_json::json!({"message":"file_write: Create prx-approved.txt containing approved console content."}),
            Some("prx-test-token"),
        ),
    )
    .await
    .into_response();
    let (status, value) = body(response).await?;
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    assert!(!path.join(TARGET).exists());
    assert!(!private_target.exists());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let events = state.mem.list_message_events_since(&scope, 0, 64).await?;
    let pending = events
        .iter()
        .filter(|event| event.event_type == "approval.decision.pending")
        .count();
    assert_eq!(pending, usize::from(case != Case::Multiple));
    let terminal = events
        .iter()
        .find(|event| event.event_type == "turn.finalized")
        .context("actual Console terminal missing")?;
    let payload: serde_json::Value = serde_json::from_str(
        terminal
            .raw_payload_json
            .as_deref()
            .context("terminal payload missing")?,
    )?;
    let usage = payload
        .get("usage_settlement")
        .context("terminal usage settlement missing")?;
    assert_eq!(usage.get("prompt_tokens").and_then(serde_json::Value::as_u64), Some(12));
    assert_eq!(
        usage.get("completion_tokens").and_then(serde_json::Value::as_u64),
        Some(8)
    );
    assert_eq!(usage.get("total_tokens").and_then(serde_json::Value::as_u64), Some(20));
    assert_eq!(
        usage.get("source").and_then(serde_json::Value::as_str),
        Some("reported")
    );
    if case == Case::Multiple {
        assert!(
            !status.is_success(),
            "multiple calls must not create an approval: {value}"
        );
        return Ok(());
    }
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(
        value.get("status").and_then(serde_json::Value::as_str),
        Some("awaiting_approval")
    );
    let id = value
        .get("decision_id")
        .and_then(serde_json::Value::as_str)
        .context("actual pending decision missing")?
        .to_owned();
    let unauthorized = post_decision_resume(
        State(state.clone()),
        Path((SESSION.into(), id.clone())),
        request(serde_json::json!({}), None),
    )
    .await;
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    let foreign = change_decision_approval(
        state.clone(),
        SESSION.into(),
        id.clone(),
        super::super::super::ApiPrincipal::of_token("prx-foreign-token"),
        DecisionApprovalChange::Review(true),
    )
    .await;
    assert_eq!(foreign.status(), StatusCode::CONFLICT);
    // A separately pinned ReadOnly generation must refuse before publication,
    // pending mutation, input recording or job dispatch, even for the true owner.
    let runtime = state.pin_turn_runtime();
    let service = runtime.decision_slot.publish(&runtime, &state).await?;
    anyhow::ensure!(
        service.drain_writes(Instant::now() + Duration::from_secs(5)).await,
        "pending collection events must settle before checking readonly mutation"
    );
    let before_readonly = state.mem.list_message_events_since(&scope, 0, 64).await?;
    let mut readonly = state.clone();
    let mut readonly_config = (*state.config.pin().effective).clone();
    readonly_config.autonomy.level = crate::security::policy::AutonomyLevel::ReadOnly;
    readonly.config = crate::config::new_shared(readonly_config);
    readonly.turn_runtime = None;
    for change in [
        DecisionApprovalChange::Review(true),
        DecisionApprovalChange::Review(false),
        DecisionApprovalChange::Revoke,
    ] {
        let refused = change_decision_approval(
            readonly.clone(),
            SESSION.into(),
            id.clone(),
            super::super::super::ApiPrincipal::of_token("prx-test-token"),
            change,
        )
        .await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    }
    let refused = post_decision_resume(
        State(readonly),
        Path((SESSION.into(), id.clone())),
        request(serde_json::json!({}), Some("prx-test-token")),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    let after_readonly = state.mem.list_message_events_since(&scope, 0, 64).await?;
    assert_eq!(after_readonly.len(), before_readonly.len());
    assert!(
        after_readonly
            .iter()
            .zip(&before_readonly)
            .all(|(after, before)| after.event_id == before.event_id
                && after.raw_payload_json == before.raw_payload_json)
    );
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert!(!path.join(TARGET).exists());
    assert!(!private_target.exists());
    let principal = super::super::super::ApiPrincipal::of_token("prx-test-token");
    let inspected = get_decision_approval(
        State(state.clone()),
        Path((SESSION.into(), id.clone())),
        axum::Extension(principal.clone()),
    )
    .await
    .into_response();
    assert_eq!(inspected.status(), StatusCode::OK);
    let approved = change_decision_approval(
        state.clone(),
        SESSION.into(),
        id.clone(),
        principal,
        DecisionApprovalChange::Review(true),
    )
    .await;
    assert_eq!(approved.status(), StatusCode::OK);
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    let resumed = post_decision_resume(
        State(state.clone()),
        Path((SESSION.into(), id.clone())),
        request(serde_json::json!({}), Some("prx-test-token")),
    )
    .await;
    let (status, value) = body(resumed).await?;
    assert_eq!(status, StatusCode::OK, "{value}");
    // Read the actual owner-scoped stored decision, never infer its backend
    // from the endpoint configuration or the HTTP success status.
    use crate::causal_tree::store::{CausalDecisionStore, DecisionScope, TimeWindow};
    let store = CausalDecisionStore::existing_shared_writer(&path).context("actual publication store")?;
    let decision_scope = DecisionScope::Owner {
        workspace_id: store.workspace_id().context("actual decision store identity")?,
        owner_id: Arc::from(scope.owner_id.as_deref().context("actual owner")?),
    };
    let rows = store.reader().list(
        &decision_scope,
        TimeWindow {
            from_ms: 0,
            to_ms: i64::MAX,
        },
        16,
    )?;
    assert_eq!(rows.len(), 2, "exactly the collection and fresh resume decisions");
    let collection_row = rows
        .iter()
        .find(|row| row.decision_id.to_string() == id)
        .context("actual pending collection decision")?;
    assert_eq!(collection_row.directive_kind, "collect_approval_arguments");
    let resumed_row = rows
        .iter()
        .find(|row| row.decision_id.to_string() != id)
        .context("actual fresh resume decision")?;
    assert!(resumed_row.has_owner);
    assert_eq!(resumed_row.session_key.as_deref(), Some(SESSION));
    assert_eq!(collection_row.session_key, resumed_row.session_key);
    assert!(resumed_row.run_id.is_some());
    assert_ne!(collection_row.run_id, resumed_row.run_id);
    assert!(
        resumed_row.prior_decision_id.is_none(),
        "fresh ingress, not an invented hop"
    );
    let replay = store
        .reader()
        .replay_record(&decision_scope, &resumed_row.decision_id)?
        .context("actual resume source binding")?;
    let resume_events = state.mem.list_message_events_since(&scope, 0, 64).await?;
    let source = resume_events
        .iter()
        .find(|event| Some(event.event_id.as_str()) == replay.source_event_id.as_deref())
        .context("actual persisted resume ingress")?;
    assert_eq!(source.event_type, "message.created");
    assert_eq!(source.owner_id, scope.owner_id);
    assert_eq!(source.session_key.as_deref(), Some(SESSION));
    assert_eq!(source.run_id, resumed_row.run_id);
    if case == Case::Builtin {
        assert_eq!(resumed_row.backend_kind, "builtin");
        assert_eq!(resumed_row.directive_kind, "use_tool");
        assert!(resumed_row.abstention_reason.is_none());
        assert!(
            resumed_row.attempts.is_empty(),
            "builtin selection makes no decision HTTP call"
        );
    } else {
        assert_eq!(resumed_row.backend_kind, "compatible_http");
        assert_eq!(resumed_row.configured_model, DECISION_MODEL);
        assert_eq!(resumed_row.directive_kind, "use_tool");
        assert_eq!(resumed_row.attempts.len(), 1);
        let attempt = resumed_row.attempts.first().context("real HTTP attempt")?;
        assert_eq!(attempt.backend, "compatible_http");
        assert_eq!(attempt.outcome, "served");
        assert!(attempt.error_class.is_none());
    }
    assert_eq!(
        value.get("execution_status").and_then(serde_json::Value::as_str),
        Some("succeeded")
    );
    assert_eq!(std::fs::read_to_string(&private_target)?, CONTENT);
    assert!(
        !path.join(TARGET).exists(),
        "Console must not write the shared workspace"
    );
    let foreign_filesystem = crate::tools::owner_filesystem::OwnerFilesystem::open(
        config_parent,
        &scope.workspace_id,
        "prx_other_owner",
        &path,
    )?;
    assert!(foreign_filesystem.resolve_existing(TARGET).is_err());
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    // The actual publication must use this same scoped memory for execution,
    // not merely for the decision and pending-approval records.
    let executed_events = state.mem.list_message_events_since(&scope, 0, 64).await?;
    let mut reservations = executed_events
        .iter()
        .filter(|event| event.event_type == "tool.execution.reserved");
    let reserved = reservations.next().context("actual execution reservation")?;
    assert!(reservations.next().is_none());
    let mut terminals = executed_events
        .iter()
        .filter(|event| event.event_type == "tool.execution.finalized");
    let finalized = terminals.next().context("actual execution terminal")?;
    assert!(terminals.next().is_none());
    for event in [reserved, finalized] {
        assert_eq!(event.workspace_id, scope.workspace_id);
        assert_eq!(event.owner_id, scope.owner_id);
        assert_eq!(event.session_key, scope.session_key);
    }
    let reservation: serde_json::Value =
        serde_json::from_str(reserved.raw_payload_json.as_deref().context("reservation payload")?)?;
    let terminal: serde_json::Value =
        serde_json::from_str(finalized.raw_payload_json.as_deref().context("terminal payload")?)?;
    let operation_id = format!("decision-resume:{id}");
    assert_eq!(
        reservation.get("operation_id").and_then(serde_json::Value::as_str),
        Some(operation_id.as_str())
    );
    assert_eq!(
        reservation.get("capability").and_then(serde_json::Value::as_str),
        Some("file_write")
    );
    assert_eq!(terminal.get("reservation"), Some(&reservation));
    assert_eq!(
        terminal
            .get("outcome")
            .and_then(|outcome| outcome.get("status"))
            .and_then(serde_json::Value::as_str),
        Some("succeeded")
    );
    let repeated = post_decision_resume(
        State(state),
        Path((SESSION.into(), id)),
        request(serde_json::json!({}), Some("prx-test-token")),
    )
    .await;
    assert!(!repeated.status().is_success());
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    Ok(())
}

fn run_case(case: Case) -> std::pin::Pin<Box<impl std::future::Future<Output = anyhow::Result<()>>>> {
    Box::pin(run_owned_case(case))
}
async fn run_case_work(
    case: Case,
    endpoint: Option<&str>,
    calls: &AtomicUsize,
    scratch: &std::path::Path,
) -> anyhow::Result<()> {
    let _keyring = crate::acl::approval_grant::WitnessKeyring::global_for_tests();
    let mut config = Config::default();
    let workspace = scratch.join("workspace");
    std::fs::create_dir(&workspace)?;
    config.workspace_dir = workspace.clone();
    config.config_path = scratch.join("config.toml");
    config.causal_tree.enabled = true;
    config.autonomy.level = crate::security::policy::AutonomyLevel::Supervised;
    config.default_provider = Some("ollama".into());
    config.default_model = Some(MODEL.into());
    config.providers.models.push(toml::from_str(&format!(
        "provider = \"ollama\"\nmodel = \"{MODEL}\"\noperations = [\"chat\"]\nnative_tools = true\ncontext_window = 65536\n"
    ))?);
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
    assert_eq!(config.causal_tree.policy.extra_latency_budget_ms, 500);
    if let Some(endpoint) = endpoint {
        config.causal_tree.backend.kind =
            crate::causal_tree::config::BackendSelection::Fixed(crate::decision::BackendKind::CompatibleHttp);
        config.causal_tree.backend.endpoint = endpoint.to_owned();
        config.causal_tree.backend.model = DECISION_MODEL.to_owned();
        let priced_provider = format!("custom:{endpoint}");
        config.providers.models.push(serde_json::from_value(serde_json::json!({
            "provider":priced_provider, "model":DECISION_MODEL, "operations":["decision_endpoint"]
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
    }
    let provider = Arc::new(CollectionProvider {
        calls: AtomicUsize::new(0),
        multiple: case == Case::Multiple,
    });
    let writes = Arc::new(AtomicUsize::new(0));
    let mut state = test_app_state(config.clone(), provider.clone());
    // The old ordinary Console helper intentionally forces causal off. Install
    // the actual enabled generation before any source/publication is created.
    state.config = crate::config::new_shared(config.clone());
    state.model = MODEL.into();
    state.tools_registry = Arc::new(vec![Box::new(CountedWrite {
        inner: crate::tools::file_write::FileWriteTool::new(Arc::new(crate::security::SecurityPolicy::from_config(
            &config.autonomy,
            &config.workspace_dir,
        ))),
        calls: Arc::clone(&writes),
    })]);
    let envelope = console_runtime_envelope_for_workspace(
        config.workspace_dir.to_string_lossy().as_ref(),
        SESSION,
        "console",
        "prx_user",
    );
    // Existing actual source helper creates the conversation metadata. This is
    // an ingress anchor, not a model turn, pending approval or authority grant.
    let (runtime, anchor_envelope, _, anchor) =
        console_test_source(&mut state, envelope, "Console fixture scope anchor").await;
    let scope = anchor_envelope.memory_principal();
    drop(anchor);
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
    // Retain the actual publication so its writer can be explicitly drained
    // before removing scratch, while the runtime still owns its decision slot.
    let service = Arc::clone(runtime.decision_slot.publish(&runtime, &state).await?);
    let operation = std::panic::AssertUnwindSafe(async {
        exercise(state, provider, writes, workspace.clone(), scope, case).await?;
        if case == Case::Controlled {
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            check_decision_accounting(&workspace)?;
        } else {
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        }
        Ok::<_, anyhow::Error>(())
    })
    .catch_unwind();
    let result = tokio::time::timeout(Duration::from_secs(5), Box::pin(operation)).await;
    let drained = runtime
        .decision_slot
        .cleanup
        .drain(tokio::time::Instant::now() + Duration::from_secs(5))
        .await;
    let writer_drained = service.drain_writes(Instant::now() + Duration::from_secs(5)).await;
    if !matches!(&result, Ok(Ok(Ok(())))) || drained.is_err() || !writer_drained {
        anyhow::bail!(
            "Console fixture failed; actual result={result:?}; cleanup={drained:?}; writer_drained={writer_drained}; retained={}",
            scratch.display()
        );
    }
    Ok(())
}

#[tokio::test]
async fn console_controlled_decision_file_write_requires_owner_review_and_fresh_resume() -> anyhow::Result<()> {
    run_case(Case::Controlled).await
}
#[tokio::test]
async fn console_multiple_collection_calls_fail_without_pending_or_file_write() -> anyhow::Result<()> {
    run_case(Case::Multiple).await
}

#[tokio::test]
async fn console_builtin_approved_resume_selects_and_executes_file_write() -> anyhow::Result<()> {
    run_case(Case::Builtin).await
}

// This endpoint controls predictions, not source, schema, approval or execution
// authority. Every response key and option comes from the actual compiler wire.
fn decision_answers(body: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
    use serde_json::{Value, json};
    assert_eq!(body.get("model").and_then(Value::as_str), Some(DECISION_MODEL));
    let candidates = body
        .pointer("/state/candidates")
        .and_then(Value::as_array)
        .context("actual candidate projection")?;
    let (target_index, selected_action, selected_target) = candidates
        .iter()
        .position(|candidate| {
            candidate.get("action").and_then(Value::as_str) == Some("use_tool")
                && candidate.get("target").and_then(Value::as_str) == Some("filesystem")
        })
        .map(|index| (index, "use_tool", "filesystem"))
        .or_else(|| {
            candidates
                .iter()
                .position(|candidate| candidate.get("action").and_then(Value::as_str) == Some("answer"))
                .map(|index| (index, "answer", "none"))
        })
        .context("actual filesystem use-tool or terminal answer candidate")?;
    let target_path = format!("$.candidates[{target_index}]");
    let questions = body
        .get("questions")
        .and_then(Value::as_object)
        .context("actual salted wire questions")?;
    anyhow::ensure!(!questions.is_empty(), "empty decision request");
    let mut answers = serde_json::Map::new();
    for (id, question) in questions {
        let instruction = question
            .get("instructions")
            .and_then(Value::as_str)
            .context("actual question instructions")?;
        let kind = question
            .get("type")
            .and_then(Value::as_str)
            .context("actual question type")?;
        let target_question = instruction.contains(&target_path);
        let candidate_question = candidates.iter().enumerate().any(|(index, _)| {
            let path = format!("$.candidates[{index}]");
            instruction == format!("Does {path} apply to this state?")
                || instruction == format!("Would {path} succeed?")
                || instruction == format!("Rate the risk of {path}.")
                || instruction == format!("Rate the task gain from {path}.")
        });
        let answer = match kind {
            "choice" => {
                let chosen = match instruction {
                    "Choose the best legal next action." => selected_action,
                    "Choose the capability family needed by the next action." => selected_target,
                    _ => anyhow::bail!("unexpected choice question"),
                };
                let options = question
                    .get("criteria")
                    .and_then(Value::as_object)
                    .context("choice criteria")?;
                anyhow::ensure!(options.contains_key(chosen), "required actual choice absent");
                let probabilities: serde_json::Map<String, Value> = options
                    .keys()
                    .map(|option| (option.clone(), json!(if option == chosen { 1.0 } else { 0.0 })))
                    .collect();
                json!({"type":"choice","choice":chosen,"probabilities":probabilities,"confidence":1.0})
            }
            "noul" => {
                let yes = match instruction {
                    "Is the observed evidence sufficient for the next action?" => true,
                    "Could the observed artifacts be stale?" => false,
                    _ if candidate_question
                        && (instruction.starts_with("Does ") || instruction.starts_with("Would ")) =>
                    {
                        target_question
                    }
                    _ => anyhow::bail!("unexpected noul question"),
                };
                json!({"type":"noul","noul":if yes {1.0} else {0.0}})
            }
            "score" => {
                let high = match instruction {
                    "Rate the expected task gain." => true,
                    "Rate the risk of the next action." => false,
                    _ if candidate_question && instruction.starts_with("Rate the risk of ") => false,
                    _ if candidate_question && instruction.starts_with("Rate the task gain from ") => target_question,
                    _ => anyhow::bail!("unexpected score question"),
                };
                let levels = question
                    .get("criteria")
                    .and_then(Value::as_array)
                    .context("score criteria")?;
                let last = levels.len().checked_sub(1).context("nonempty score legend")?;
                let selected = if high { last } else { 0 };
                let legend: serde_json::Map<String, Value> = levels
                    .iter()
                    .enumerate()
                    .map(|(index, level)| (index.to_string(), level.clone()))
                    .collect();
                let probabilities: serde_json::Map<String, Value> = levels
                    .iter()
                    .enumerate()
                    .map(|(index, _)| (index.to_string(), json!(if index == selected { 1.0 } else { 0.0 })))
                    .collect();
                json!({"type":"score","score":selected,"legend":legend,"probabilities":probabilities,"confidence":1.0})
            }
            _ => anyhow::bail!("unexpected decision primitive"),
        };
        answers.insert(id.clone(), answer);
    }
    Ok(json!({"model":DECISION_MODEL,"answers":answers,"usage":{"input_tokens":20,"output_tokens":10}}))
}

async fn decision_reply(
    axum::extract::State(calls): axum::extract::State<Arc<AtomicUsize>>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Response {
    calls.fetch_add(1, Ordering::SeqCst);
    let filesystem = body
        .pointer("/state/candidates")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|candidates| {
            candidates.iter().any(|candidate| {
                candidate.get("action").and_then(serde_json::Value::as_str) == Some("use_tool")
                    && candidate.get("target").and_then(serde_json::Value::as_str) == Some("filesystem")
            })
        });
    assert!(
        filesystem,
        "the counted decision request must take the filesystem candidate branch"
    );
    decision_answers(&body).map_or_else(
        |_| StatusCode::BAD_REQUEST.into_response(),
        |response| axum::Json(response).into_response(),
    )
}

async fn run_owned_case(case: Case) -> anyhow::Result<()> {
    let scratch = tempfile::tempdir()?;
    let calls = Arc::new(AtomicUsize::new(0));
    let mut server = None;
    let stop = tokio_util::sync::CancellationToken::new();
    let endpoint = if case == Case::Controlled {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let server_stop = stop.clone();
        let app = axum::Router::new()
            .route("/v1/systemone", axum::routing::post(decision_reply))
            .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
            .with_state(Arc::clone(&calls));
        server = Some(tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(server_stop.cancelled_owned())
                .await
        }));
        Some(endpoint)
    } else {
        None
    };
    // All setup/publication/exercise errors and panics after spawning converge
    // on this teardown. Scratch outlives both the service and listener owner.
    let result = std::panic::AssertUnwindSafe(Box::pin(run_case_work(
        case,
        endpoint.as_deref(),
        &calls,
        scratch.path(),
    )))
    .catch_unwind()
    .await;
    stop.cancel();
    let joined = if let Some(mut server) = server {
        match tokio::time::timeout(Duration::from_secs(5), &mut server).await {
            Ok(result) => result.is_ok_and(|served| served.is_ok()),
            Err(_) => {
                server.abort();
                let aborted = tokio::time::timeout(Duration::from_secs(5), &mut server).await;
                // A watchdog-triggered teardown is a test failure even when
                // the abort is confirmed; an unknown join also retains scratch.
                drop(aborted);
                false
            }
        }
    } else {
        true
    };
    if !joined || !matches!(&result, Ok(Ok(()))) {
        let retained = scratch.keep();
        anyhow::bail!(
            "Console fixture result={result:?}; listener_joined={joined}; retained={}",
            retained.display()
        );
    }
    Ok(())
}

fn check_decision_accounting(path: &std::path::Path) -> anyhow::Result<()> {
    use serde_json::Value;
    let journal = std::fs::read_to_string(path.join("state/costs.reservations.jsonl"))?;
    let rows: Vec<Value> = journal.lines().map(serde_json::from_str).collect::<Result<_, _>>()?;
    let matching = |event: &str| {
        rows.iter()
            .filter(|row| row.get("event").and_then(Value::as_str) == Some(event))
            .collect::<Vec<_>>()
    };
    let reserved = matching("estimated_reserved_v1");
    let dispatched = matching("dispatched");
    let settled = matching("estimated_settled_v1");
    assert_eq!((reserved.len(), dispatched.len(), settled.len()), (1, 1, 1));
    let attempt = reserved
        .first()
        .and_then(|row| row.pointer("/request/reservation/attempt_id"))
        .and_then(Value::as_str)
        .context("actual decision reservation")?;
    assert!(attempt.starts_with("decision:"));
    for row in [dispatched.first(), settled.first()] {
        assert_eq!(
            row.and_then(|row| row.get("attempt_id")).and_then(Value::as_str),
            Some(attempt)
        );
    }
    assert_eq!(
        settled
            .first()
            .and_then(|row| row.get("actual_tokens"))
            .and_then(Value::as_str),
        Some("30")
    );
    Ok(())
}

mod two_owner;
