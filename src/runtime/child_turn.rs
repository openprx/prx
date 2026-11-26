//! Source-bound decision and terminal ownership for one child execution segment.

pub(in crate::runtime) mod segment_observation;

use super::turn_source::{OwnedTurnSourceProof, TurnSourceProof};
use crate::agent::terminal::{
    TurnDeliveryIntent, TurnTerminalDraft, TurnTerminalReceipt, TurnTerminalStatus, TurnTerminalTelemetry,
};
use crate::agent::turn_contracts::{ToolLoopOutcome, ToolLoopRuntimeAdapter, ToolLoopTrace};
use crate::runtime::turn_context::OpenedTurn;
use crate::runtime::turn_publication::TurnRuntimeService;
use anyhow::Context;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(crate) struct ChildSegmentTurn {
    service: Arc<TurnRuntimeService>,
    proof: Arc<OwnedTurnSourceProof>,
    decision: Arc<OpenedTurn>,
    /// The segment turn's execution-owned runtime context (R26); the service
    /// and decision context stay until Part 2.
    runtime: Arc<crate::runtime::turn_context::TurnRuntimeContext>,
    execution: crate::tools::ToolExecutionContext,
    terminal: super::turn_terminal::TurnTerminalGuard,
    binding: crate::agent::terminal::TerminalBinding,
    provider_trace: Arc<crate::agent::turn_contracts::RetainedProviderTrace>,
    started_at: chrono::DateTime<chrono::Utc>,
    route: crate::llm::route_decision::RouteDecision,
    history_commit_len: Option<usize>,
    /// The memory the replayed-session recall of each admitted input reads.
    recall_memory: Arc<dyn crate::memory::Memory>,
    memory_config: crate::config::MemoryConfig,
}

// Resources consumed by the synchronous post-decision terminal admission.
struct ChildTerminalAdmission {
    fabric: crate::memory::MemoryFabric,
    config: Arc<crate::config::Config>,
    usage_collector: Option<Arc<super::turn_terminal::TerminalUsageCollector>>,
    route: crate::llm::route_decision::RouteDecision,
    recall_memory: Arc<dyn crate::memory::Memory>,
    memory_config: crate::config::MemoryConfig,
}

impl ChildSegmentTurn {
    pub(crate) async fn admit(
        service: Arc<TurnRuntimeService>,
        source: (Arc<OwnedTurnSourceProof>, crate::tools::ToolExecutionContext),
        fabric: crate::memory::MemoryFabric,
        config: Arc<crate::config::Config>,
        owner: &Arc<super::turn_cleanup::CleanupTracker>,
        cancellation: &CancellationToken,
        usage_collector: Option<Arc<super::turn_terminal::TerminalUsageCollector>>,
    ) -> anyhow::Result<Self> {
        let (proof, execution) = source;
        let previous_observation = match usage_collector.as_ref() {
            Some(collector) => {
                if proof.source_event_id().is_some() {
                    segment_observation::validate_envelope(&execution.envelope)?;
                }
                collector.previous_observation(&proof, &execution.envelope)?
            }
            None => {
                anyhow::ensure!(
                    proof.previous_segment_input().is_none(),
                    "segment predecessor has no terminal owner"
                );
                None
            }
        };
        let model = service.resolve_agent_model(proof.as_ref())?;
        let route = crate::llm::route_decision::RouteDecision::single_candidate_for_context(
            model.provider_label().to_owned(),
            model.model().to_owned(),
            proof.owner_id().to_owned(),
            proof.session_key().to_owned(),
            proof.source_event_id().map(str::to_owned),
            None,
            "child_segment",
            u32::try_from(proof.routing_input().chars().count() / 4).unwrap_or(u32::MAX),
            service.model_can_call_tools(proof.as_ref()),
            false,
        );
        let decision = service
            .open_issued_segment_turn_context(proof.as_ref(), previous_observation.as_deref(), cancellation)
            .await?;
        let recall_memory = fabric.memory();
        let memory_config = config.memory.clone();
        Self::finish_admission(
            service,
            proof,
            decision,
            execution,
            ChildTerminalAdmission {
                fabric,
                config,
                usage_collector,
                route,
                recall_memory,
                memory_config,
            },
            owner,
            cancellation,
        )
    }

    // Construct the terminal only after segment work has returned. Keep its
    // intermediate guards outside the earlier admission poll frame.
    #[inline(never)]
    fn finish_admission(
        service: Arc<TurnRuntimeService>,
        proof: Arc<OwnedTurnSourceProof>,
        decision: Arc<OpenedTurn>,
        execution: crate::tools::ToolExecutionContext,
        admission: ChildTerminalAdmission,
        owner: &Arc<super::turn_cleanup::CleanupTracker>,
        cancellation: &CancellationToken,
    ) -> anyhow::Result<Self> {
        let ChildTerminalAdmission {
            fabric,
            config,
            usage_collector,
            route,
            recall_memory,
            memory_config,
        } = admission;
        let mut execution = execution;
        execution.call_budget = Some(
            crate::agent::send_admission::call_token_budget(&service, &decision, proof.as_ref())
                .context("segment call budget")?,
        );
        // This owner is outside the segment's child-drain scope. Registering
        // inside that scope would make it wait for its own terminal guard.
        let provider_trace = Arc::new(crate::agent::turn_contracts::RetainedProviderTrace::default());
        let binding = service.terminal_binding(&decision)?;
        let terminal = super::turn_terminal::TurnTerminalGuard::new(
            owner.register(cancellation.clone())?,
            Arc::clone(&service),
            Arc::clone(&decision),
            fabric,
            config,
            execution.envelope.clone(),
            None,
        )?
        .with_provider_trace(Arc::clone(&provider_trace), route.clone())
        .with_usage_collector(usage_collector);
        let runtime = Arc::clone(decision.runtime());
        Ok(Self {
            service,
            proof,
            decision,
            runtime,
            execution,
            terminal,
            binding,
            provider_trace,
            started_at: chrono::Utc::now(),
            route,
            history_commit_len: None,
            recall_memory,
            memory_config,
        })
    }

    pub(crate) fn with_child_cleanup(
        mut self,
        acknowledgement: tokio::sync::oneshot::Receiver<Result<(), String>>,
    ) -> Self {
        self.terminal = self.terminal.with_child_cleanup(acknowledgement);
        self
    }

    /// A synchronous child owns its own assistant projection; task segments
    /// leave this unset because their logical task owns the projection.
    pub(crate) const fn project_history(&mut self, history_commit_len: usize) {
        self.history_commit_len = Some(history_commit_len);
    }

    /// The segment turn's runtime context (R26).
    pub(crate) const fn runtime(&self) -> &Arc<crate::runtime::turn_context::TurnRuntimeContext> {
        &self.runtime
    }

    pub(crate) fn routing(&self) -> crate::agent::turn_request::TurnDecision<'_> {
        crate::agent::turn_request::TurnDecision {
            service: &self.service,
            context: &self.decision,
            runtime: &self.runtime,
            proof: self.proof.as_ref(),
        }
    }

    pub(crate) fn prompt_skills(&self) -> anyhow::Result<Vec<crate::skills::Skill>> {
        self.service.agent_prompt_skills(self.proof.as_ref(), &self.decision)
    }

    pub(crate) fn system_prompt(&self, config: &crate::config::Config, delegated: &str) -> anyhow::Result<String> {
        let skills = self.prompt_skills()?;
        let model = self.service.resolve_agent_model(self.proof.as_ref())?;
        let mut child_config = config.clone();
        child_config.workspace_dir = std::path::PathBuf::from(self.proof.workspace_id());
        child_config.autonomy.level = self.execution.child_scope.as_ref().map_or_else(
            || self.service.security().autonomy,
            |scope| scope.gate_inputs().0.authority.level,
        );
        Ok(crate::agent::runtime_prompt::build_delegated_system_prompt(
            &child_config,
            model.model(),
            &skills,
            self.service.model_can_call_tools(self.proof.as_ref()),
            delegated,
        ))
    }

    pub(crate) fn compaction(&self) -> &crate::config::AgentCompactionConfig {
        self.decision.main_compaction()
    }

    /// Prefixes the replayed-session recall and appends the date tail to the
    /// last message of `history`, which must be the admitted input. The recall
    /// is read here, once per input, for steering inputs and delegated turns.
    pub(crate) async fn enrich_input(&self, history: &mut [crate::providers::ChatMessage]) -> anyhow::Result<()> {
        use anyhow::Context;
        let input = history
            .last_mut()
            .filter(|message| message.role == "user")
            .context("child segment input is missing from history")?;
        anyhow::ensure!(
            input.content == self.proof.routing_input(),
            "child segment history differs from its admitted input"
        );
        let scope = crate::agent::context_recall::RecallScope::from_envelope(
            &self.execution.envelope,
            &self.memory_config,
            self.proof.policy_chat_type(),
        );
        let recall_budget = crate::context::api::open::replayed_recall_budget(self.compaction().max_context_tokens);
        let mut enriched = crate::agent::context_recall::replayed_session_recall(
            self.recall_memory.as_ref(),
            &scope,
            self.proof.routing_input(),
            recall_budget,
        )
        .await
        .preamble;
        enriched.push_str(&input.content);
        crate::agent::turn_contracts::push_current_time_tail(&mut enriched);
        *input = crate::agent::turn_contracts::inbound_user_message(&enriched);
        Ok(())
    }

    /// Opens the first input of a task segment through the shared turn
    /// opening: the earlier turns of the child are replayed with their tool
    /// rounds and attachments, fitted to the budget, the recall of the session
    /// is read, and the input's attachment row is stored. Returns the whole
    /// model history: the system message, the replayed turns and the request.
    pub(crate) async fn open_first_input(
        &self,
        previous: Vec<crate::memory::child_sessions::ChildTurn>,
        system_prompt: String,
    ) -> anyhow::Result<Vec<crate::providers::ChatMessage>> {
        let previous_turns = previous
            .into_iter()
            .map(|turn| crate::memory::ConversationTurn {
                id: 0,
                session_key: self.execution.envelope.session_key.clone(),
                role: turn.role,
                content: turn.content,
                timestamp: turn.timestamp.to_rfc3339(),
                message_id: turn.event_id.clone(),
                message_event_id: turn.event_id,
            })
            .collect();
        let model = self.service.resolve_agent_model(self.proof.as_ref())?;
        let opening = crate::context::api::open::open_turn(crate::context::api::open::OpenInput {
            memory: &self.recall_memory,
            envelope: &self.execution.envelope,
            memory_config: &self.memory_config,
            chat_type: self.proof.policy_chat_type(),
            visible_message: self.proof.routing_input(),
            card: None,
            previous_turns,
            replay: crate::context::api::open::ReplayMode::FitToBudget,
            recall: crate::context::api::open::OpenRecall::ReplayedSession,
            current_input_key: None,
            excluded_session: None,
            system_prompt,
            hooks: None,
            service: &self.service,
            context: &self.decision,
            provider_label: model.provider_label(),
            model: model.model(),
        })
        .await;
        Ok(opening.history)
    }

    pub(crate) fn adapter(&self) -> ToolLoopRuntimeAdapter {
        let live = self.execution.live_turn.clone();
        let mut context = self.execution.clone();
        if live.is_some() {
            context.approval_ledger = self.service.live_approval_ledger();
        }
        ToolLoopRuntimeAdapter {
            live_frames: live.map(|live| {
                live.0.frames(
                    &self.execution.envelope,
                    &crate::agent::sanitize::known_tool_names(self.service.registry.as_ref()),
                )
            }),
            guarded_delivery: false,
            provider_trace: Some(Arc::clone(&self.provider_trace)),
            events: None,
            stream_provider: false,
            tool_execution_service: None,
            tool_execution_context: context,
            allowed_tool_names: None,
        }
    }

    /// A task segment's adapter (G5b ruling 1): its published execution under
    /// its run's approval strategy, the production strategy a root turn uses,
    /// so a Supervised `Ask` reaches the same resolver as the root's.
    pub(crate) fn adapter_with_approval(
        &self,
        approval: Arc<dyn crate::tools::ApprovalStrategy>,
    ) -> ToolLoopRuntimeAdapter {
        let mut adapter = self.adapter();
        adapter.tool_execution_service = Some(Arc::new(self.service.execution.with_approval(approval)));
        adapter
    }

    fn proposal(
        &self,
        status: TurnTerminalStatus,
        summary: String,
        provider_outcome: Option<crate::llm::route_decision::ProviderExecutionOutcome>,
    ) -> TurnTerminalDraft {
        TurnTerminalDraft {
            binding: Some(self.binding.clone()),
            terminal_id: self.proof.run_id().to_owned(),
            scope: self.execution.envelope.message_scope(),
            status,
            history: self
                .history_commit_len
                .filter(|_| status == TurnTerminalStatus::Completed)
                .map(|history_commit_len| crate::agent::terminal::TurnHistoryProjection {
                    assistant_content: summary.clone(),
                    history_commit_len,
                }),
            history_scope: None,
            provider_outcome,
            telemetry: TurnTerminalTelemetry {
                summary,
                started_at: self.started_at,
                finished_at: chrono::Utc::now(),
            },
            delivery_intent: TurnDeliveryIntent::ReturnToCaller,
        }
    }

    pub(crate) async fn complete(
        &self,
        outcome: &ToolLoopOutcome,
        trace: &ToolLoopTrace,
    ) -> anyhow::Result<TurnTerminalReceipt> {
        let (status, summary) = match outcome {
            ToolLoopOutcome::Text(text) | ToolLoopOutcome::AwaitingApproval { reply: text } => {
                (TurnTerminalStatus::Completed, text.clone())
            }
            ToolLoopOutcome::Silent { reason } => (TurnTerminalStatus::Silent, reason.clone()),
        };
        let retained = self.provider_trace.snapshot().0.unwrap_or_else(|| trace.clone());
        let provider = crate::agent::terminal::provider_outcome_from_trace(&self.route, self.started_at, retained);
        self.terminal
            .commit(self.proposal(status, summary, Some(provider)))
            .await
            .map_err(|error| error.context(super::turn_cleanup::CleanupIncomplete))
    }

    pub(crate) async fn fail(&self, error: &anyhow::Error, cancelled: bool) -> anyhow::Result<()> {
        self.terminal
            .finish_failure(error, cancelled)
            .await
            .map_err(|error| error.context(super::turn_cleanup::CleanupIncomplete))
    }
}
