#![cfg(test)]
//! Real main loop collection and durable signed pending terminal; no tool dispatch.
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

    async fn chat_traced(&self, request: ChatRequest<'_>, model: &str, _: f64) -> anyhow::Result<ChatTrace> {
        anyhow::ensure!(
            self.calls.fetch_add(1, Ordering::SeqCst) == 0,
            "approval must stop the loop"
        );
        let specs = request.tools.expect("collection singleton schema");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs.first().unwrap().name, self.tool_name);
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
        ChatMessage::system("Collect exact file arguments for human review"),
        ChatMessage::user(f.proof.routing_input()),
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
                provider_trace: Some(retained),
                events: None,
                stream_provider: false,
                tool_execution_service: None,
                tool_execution_context: ToolExecutionContext::new(f.envelope.clone(), f.proof.policy_chat_type()),
                allowed_tool_names: None,
            }),
            memory: ToolLoopMemory::new(f.memory, &f.config.workspace_dir, None)
                .with_event_fabric(fabric.clone())
                .with_request_event_scope(f.envelope.message_scope()),
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
    let physical = trace
        .execution_trace
        .as_ref()
        .expect("terminal keeps the actual empty trace");
    assert_eq!(physical.started_attempts, 0);
    assert!(physical.calls.is_empty());
    assert!(f.context.execution_refs().is_empty());
    assert!(f.context.collected_pending_event().is_some());
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
    let replay = crate::agent::terminal::verify_terminal_replay(
        f.memory.as_ref(),
        &f.service.store.reader(),
        &f.service.state_key,
        f.service.store.workspace_id().unwrap().as_ref(),
        f.proof.workspace_id(),
        f.proof.owner_id(),
        &receipt.terminal_id,
        &receipt.terminal_event,
    )
    .await
    .expect("actual terminal signature and pending receipt");
    assert!(replay.collected_pending());
    let facts = crate::causal_tree::outcome::TerminalFacts::from_persisted_marker(
        &receipt.terminal_event,
        replay.decision(),
        f.proof.owner_id(),
        f.proof.workspace_id(),
    )
    .unwrap()
    .with_verified_resume(&replay);
    let outcome = crate::causal_tree::outcome::OutcomeCorrelator::persisted_record(replay.decision(), &facts).unwrap();
    let scope = crate::causal_tree::store::DecisionScope::Owner {
        workspace_id: f.service.store.workspace_id().unwrap(),
        owner_id: Arc::from(f.proof.owner_id()),
    };
    let reader = f.service.store.reader();
    assert!(reader.outcome_matches(&scope, &outcome).unwrap());
    let decision = reader
        .get(&scope, &replay.decision().decision_id)
        .unwrap()
        .expect("signed decision and actual terminal outcome are durable");
    let recorded = decision
        .outcomes
        .iter()
        .find(|record| record.settlement_id == receipt.terminal_id)
        .expect("actual terminal settlement is present");
    assert_eq!(recorded.classification, "awaiting_approval");
    assert!(recorded.executed_action.is_none() && recorded.executed_target.is_none());
    let mut tampered = receipt.terminal_event.clone();
    let mut payload: serde_json::Value = serde_json::from_str(tampered.raw_payload_json.as_deref().unwrap()).unwrap();
    payload
        .as_object_mut()
        .unwrap()
        .insert("status".to_owned(), serde_json::json!("failed"));
    tampered.raw_payload_json = Some(payload.to_string());
    assert!(
        crate::agent::terminal::verify_terminal_replay(
            f.memory.as_ref(),
            &f.service.store.reader(),
            &f.service.state_key,
            f.service.store.workspace_id().unwrap().as_ref(),
            f.proof.workspace_id(),
            f.proof.owner_id(),
            &receipt.terminal_id,
            &tampered,
        )
        .await
        .is_err(),
        "mutated terminal cannot retain signed pending evidence"
    );
}

struct RejectedProposalProvider<'a> {
    base: ReportedApprovalProvider<'a>,
    multiple: bool,
    stall: bool,
    entered: tokio::sync::Notify,
}
#[async_trait]
impl Provider for RejectedProposalProvider<'_> {
    async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
        anyhow::bail!("traced entry required")
    }
    async fn chat_traced(&self, request: ChatRequest<'_>, model: &str, temperature: f64) -> anyhow::Result<ChatTrace> {
        if self.stall {
            self.base.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            return std::future::pending().await;
        }
        let mut response = self.base.chat_traced(request, model, temperature).await?;
        if self.multiple {
            response.response.tool_calls.push(crate::providers::ToolCall {
                id: "prx_second_proposal".into(),
                name: self.base.tool_name.to_owned(),
                arguments: self.base.arguments.to_string(),
            });
        }
        Ok(response)
    }
}

/// The production collector is driven by a controlled traced response. This
/// proves rejection/Drop retention, not an HTTP transport or successful terminal.
pub(super) async fn reject(f: Fixture<'_>, mode: u8) {
    let mut arguments = f.arguments.clone();
    if mode == 0 {
        arguments
            .as_object_mut()
            .unwrap()
            .insert("_zc_scope".into(), serde_json::json!({"owner_id":"prx_foreign"}));
    }
    let provider = RejectedProposalProvider {
        base: ReportedApprovalProvider {
            tool_name: f.tool_name,
            arguments: &arguments,
            calls: AtomicUsize::new(0),
        },
        multiple: mode == 1,
        stall: mode >= 2,
        entered: tokio::sync::Notify::new(),
    };
    let retained = RetainedProviderTrace::default();
    let cancellation = CancellationToken::new();
    let history = [ChatMessage::user(f.proof.routing_input())];
    let lease = f.service.begin_approval_collection(f.proof, f.context).unwrap();
    let mut future = Box::pin(crate::agent::approval_collection::collect_approval_arguments(
        crate::agent::approval_collection::CollectionProviderRequest {
            provider: &provider,
            messages: &history,
            selected_provider: "ollama",
            model: "prx-local-model",
            temperature: 0.0,
            retained: &retained,
            cancellation: &cancellation,
        },
        lease,
    ));
    if mode >= 2 {
        tokio::select! {
            () = provider.entered.notified() => {},
            _ = &mut future => panic!("controlled provider must remain in flight"),
        }
        if mode == 2 {
            drop(future);
        } else {
            cancellation.cancel();
            assert!(future.await.is_err());
        }
        let (trace, unknown) = retained.snapshot();
        assert!(trace.is_none() && unknown, "Drop preserves unknown usage");
    } else {
        let result = future.await.unwrap();
        assert!(result.pending.is_err());
        assert_eq!(
            result.trace.tokens_used,
            TokenUsage::reported(Some(31), Some(7), Some(38))
        );
        let (trace, unknown) = retained.snapshot();
        assert!(
            trace.is_some() && !unknown,
            "rejected arguments retain actual known usage"
        );
    }
    assert_eq!(provider.base.calls.load(Ordering::SeqCst), 1);
    assert!(f.context.collected_pending_event().is_none());
    assert!(f.service.begin_approval_collection(f.proof, f.context).is_err());
    assert!(
        f.service
            .inspect_pending_decision_approval(f.proof.owner_id(), f.proof.session_key(), f.context.decision_id())
            .await
            .is_err()
    );
}
