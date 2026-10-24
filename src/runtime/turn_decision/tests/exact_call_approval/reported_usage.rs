#![cfg(test)]
//! A deterministic reported-usage provider drives the real approval and terminal paths.
use super::*;
use crate::agent::turn_contracts::{RetainedProviderTrace, ToolLoopOutcome, ToolLoopRuntimeAdapter};
use crate::agent::turn_request::{ToolLoopMemory, TurnDecision, TurnDelivery, TurnRequest, TurnSchedule, TurnServices};
use crate::llm::route_decision::{AttemptStatus, ProviderAttempt, RouteDecision, TokenUsage};
use crate::providers::traits::ChatTrace;
use crate::providers::{ChatMessage, ChatRequest, ChatResponse, Provider};
use futures_util::FutureExt as _;

pub(super) struct Fixture<'a> {
    pub service: &'a Arc<TurnDecisionService>,
    pub context: &'a Arc<TurnDecisionContext>,
    pub proof: &'a dyn crate::runtime::turn_source::TurnSourceProof,
    pub envelope: &'a crate::runtime::envelope::RuntimeEnvelope,
    pub config: &'a crate::config::Config,
    pub memory: &'a Arc<dyn Memory>,
    pub registry: &'a Arc<Vec<Box<dyn Tool>>>,
    pub tool_name: &'a str,
    pub arguments: &'a serde_json::Value,
    pub cleanup_confirmed: &'a AtomicBool,
}

struct ReportedApprovalProvider<'a> {
    tool_name: &'a str,
    arguments: &'a serde_json::Value,
    calls: AtomicUsize,
}

#[async_trait]
impl Provider for ReportedApprovalProvider<'_> {
    async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
        anyhow::bail!("the approval regression requires the traced provider entry")
    }

    async fn chat_traced(&self, _: ChatRequest<'_>, model: &str, _: f64) -> anyhow::Result<ChatTrace> {
        anyhow::ensure!(
            self.calls.fetch_add(1, Ordering::SeqCst) == 0,
            "approval must stop the loop"
        );
        let now = chrono::Utc::now();
        Ok(ChatTrace {
            response: ChatResponse {
                text: None,
                tool_calls: vec![crate::providers::ToolCall {
                    id: "prx_approval_usage_call".to_owned(),
                    name: self.tool_name.to_owned(),
                    arguments: self.arguments.to_string(),
                }],
                reasoning_content: None,
            },
            attempts: vec![ProviderAttempt {
                seq: 1,
                provider: "ollama".to_owned(),
                model: model.to_owned(),
                started_at: now,
                finished_at: now,
                status: AttemptStatus::Success,
                error_class: None,
                error_message: None,
            }],
            final_provider: "ollama".to_owned(),
            final_model: model.to_owned(),
            served_member: None,
            tokens_used: TokenUsage::reported(Some(31), Some(7), Some(38)),
        })
    }
}

pub(super) async fn exercise(fixture: Fixture<'_>) {
    let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new()).unwrap();
    // Catch inside the owned scope: all terminal guards drop before its drain,
    // including assertion failures. The outer fixture retains its workspace on
    // writer-drain failure and resumes this panic only after that barrier.
    let result = cleanup
        .run(async {
            Ok(std::panic::AssertUnwindSafe(Box::pin(exercise_owned(&fixture)))
                .catch_unwind()
                .await)
        })
        .await;
    if result.is_err() {
        fixture.cleanup_confirmed.store(false, Ordering::SeqCst);
    }
    let result = result.expect("approval usage fixture cleanup must be confirmed");
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn exercise_owned(f: &Fixture<'_>) {
    let fabric = crate::memory::MemoryFabric::new(Arc::clone(f.memory), f.config.workspace_dir.to_string_lossy())
        .with_event_recording(f.config.memory.event_recording_config());
    let owner = crate::runtime::turn_cleanup::current_tracker().unwrap();
    let retained = Arc::new(RetainedProviderTrace::default());
    let route = RouteDecision::single_candidate_for_context(
        "ollama",
        "prx-local-model".to_owned(),
        f.proof.owner_id(),
        f.proof.session_key(),
        f.proof.source_event_id().map(str::to_owned),
        None,
        "approval_usage_regression",
        8,
        true,
        false,
    );
    let guard = crate::runtime::turn_terminal::TurnTerminalGuard::new(
        owner.register(CancellationToken::new()).unwrap(),
        Arc::clone(f.service),
        Arc::clone(f.context),
        fabric.clone(),
        Arc::new(f.config.clone()),
        f.envelope.clone(),
        None,
    )
    .unwrap()
    .with_provider_trace(Arc::clone(&retained), route.clone());
    let provider = ReportedApprovalProvider {
        tool_name: f.tool_name,
        arguments: f.arguments,
        calls: AtomicUsize::new(0),
    };
    let hooks = crate::hooks::HookManager::new(f.config.workspace_dir.clone());
    let mut history = vec![
        ChatMessage::system("Inspect project context"),
        ChatMessage::user("Inspect project context"),
    ];
    let started_at = chrono::Utc::now();
    let (outcome, trace) = Box::pin(crate::agent::turn_engine::run_turn(
        &mut history,
        TurnRequest {
            channel_name: f.proof.policy_channel(),
            scope: None,
            mode: ChatMode::Edit,
            silent: true,
            expose_stay_silent: false,
            multimodal: &f.config.multimodal,
            schedule: TurnSchedule {
                read_only_tool_concurrency_window: 1,
                priority_scheduling_enabled: false,
                low_priority_tool_names: Vec::new(),
            },
            compaction: None,
            decision: TurnDecision {
                service: f.service,
                context: f.context,
                proof: f.proof,
            },
        },
        TurnServices {
            provider: &provider,
            provider_name: "ollama",
            model: "prx-local-model",
            temperature: 0.0,
            tools_registry: Arc::clone(f.registry),
            observer: &crate::observability::NoopObserver,
            hooks: &hooks,
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(ToolLoopRuntimeAdapter {
                verify_chat_completion: false,
                provider_trace: Some(retained),
                events: None,
                stream_provider: false,
                tool_execution_service: None,
                tool_execution_context: ToolExecutionContext::new(f.envelope.clone(), f.proof.policy_chat_type()),
                allowed_tool_names: None,
            }),
            memory: ToolLoopMemory::new(f.memory, &f.config.workspace_dir, None),
        },
        TurnDelivery {
            cancellation: Some(CancellationToken::new()),
            on_delta: None,
            on_tool_call: None,
        },
    ))
    .await
    .unwrap();
    assert!(matches!(outcome, ToolLoopOutcome::AwaitingApproval { .. }));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(trace.tokens_used, TokenUsage::reported(Some(31), Some(7), Some(38)));
    assert_eq!(
        trace.execution_trace.as_ref(),
        Some(&f.context.terminal_execution_trace())
    );
    let outcome = crate::agent::terminal::provider_outcome_from_trace(&route, started_at, trace);
    let receipt = guard
        .commit(crate::agent::terminal::TurnTerminalDraft {
            decision: Some(f.service.terminal_settlement_ref(Arc::clone(f.context)).unwrap()),
            terminal_id: f.envelope.run_id.clone().unwrap(),
            scope: f.envelope.message_scope(),
            status: crate::agent::terminal::TurnTerminalStatus::Completed,
            history: None,
            history_scope: None,
            provider_outcome: Some(outcome),
            telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                summary: "approval wait returned".to_owned(),
                started_at,
                finished_at: chrono::Utc::now(),
            },
            delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
        })
        .await
        .unwrap();
    let usage = receipt
        .usage_settlement
        .as_ref()
        .expect("normal terminal retains reported provider usage");
    assert_eq!(
        (usage.prompt_tokens, usage.completion_tokens, usage.total_tokens),
        (31, 7, 38)
    );
    let usage_event = f
        .memory
        .find_owned_message_event_by_idempotency_key(
            f.proof.workspace_id(),
            f.proof.owner_id(),
            &format!("turn:{}:usage", receipt.terminal_id),
        )
        .await
        .unwrap()
        .expect("usage settlement is durable");
    let persisted: crate::llm::route_decision::MeteredTokenUsageRecord =
        serde_json::from_str(usage_event.raw_payload_json.as_deref().unwrap()).unwrap();
    assert_eq!(&persisted, usage);
    assert_eq!(persisted.settlement_id.as_deref(), Some(receipt.terminal_id.as_str()));
}
