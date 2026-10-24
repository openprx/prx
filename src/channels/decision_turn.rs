//! Decision and terminal ownership for an admitted channel turn.
use crate::agent::terminal::{TurnTerminalDraft, TurnTerminalReceipt};
use crate::runtime::turn_decision::{TurnDecisionContext, TurnDecisionService, TurnDisposition};
use crate::runtime::turn_source::TurnSourceProof;
use std::sync::Arc;

pub(super) struct ChannelDecisionTurn {
    service: Arc<TurnDecisionService>,
    proof: super::decision_source::ChannelTurnProof,
    context: Arc<TurnDecisionContext>,
    terminal: crate::runtime::turn_terminal::TurnTerminalGuard,
    envelope: crate::runtime::envelope::RuntimeEnvelope,
    config: Arc<crate::config::Config>,
    generation: Arc<crate::config::ConfigGeneration>,
    trace: Arc<crate::agent::turn_contracts::RetainedProviderTrace>,
    pub(super) route: crate::llm::route_decision::RouteDecision,
}

pub(super) struct SmartGroupClassifierCall {
    accounting: Arc<crate::runtime::auxiliary_call::AuxiliaryAccounting>,
    observation: Arc<crate::runtime::auxiliary_call::AuxiliaryProviderCall>,
    route: crate::llm::route_decision::RouteDecision,
    context: Arc<TurnDecisionContext>,
}

impl SmartGroupClassifierCall {
    pub(super) fn provider(&self) -> Arc<dyn crate::providers::Provider> {
        self.accounting.provider()
    }
    pub(super) fn model(&self) -> &str {
        self.accounting.selected_model().model()
    }
    pub(super) fn accounting(&self) -> Arc<dyn crate::providers::attempt_accounting::AttemptAccounting> {
        self.accounting.clone()
    }
    pub(super) fn begin(&self) -> anyhow::Result<crate::tools::capability::execution_trace::PendingProvider> {
        self.observation.begin()?;
        self.accounting.begin_provider_observation()
    }
    pub(super) fn complete(
        &self,
        pending: crate::tools::capability::execution_trace::PendingProvider,
        trace: &crate::providers::traits::ChatTrace,
    ) -> anyhow::Result<()> {
        pending.finish(Some(&trace.attempts));
        self.observation.complete(trace)
    }
    pub(super) fn fail(&self, pending: crate::tools::capability::execution_trace::PendingProvider, cancelled: bool) {
        if cancelled {
            pending.cancelled();
        } else {
            pending.failed();
        }
    }
    pub(super) fn authorize_silence(&self) -> anyhow::Result<()> {
        self.accounting.authorize_silence()?;
        self.context.terminal_execution_trace();
        Ok(())
    }
    pub(super) fn provider_outcome(
        &self,
        trace: &crate::providers::traits::ChatTrace,
        started_at: chrono::DateTime<chrono::Utc>,
    ) -> crate::llm::route_decision::ProviderExecutionOutcome {
        crate::llm::route_decision::ProviderExecutionOutcome::from_trace(
            &self.route,
            trace.attempts.clone(),
            trace.final_provider.clone(),
            trace.final_model.clone(),
            started_at,
            chrono::Utc::now(),
            false,
        )
        .with_prepared_parameters(self.accounting.prepared_parameters())
    }
}

impl ChannelDecisionTurn {
    pub(super) async fn admit(
        ctx: &super::ChannelRuntimeContext,
        source: &super::decision_source::ChannelSourceReceipt,
        generation: &Arc<crate::config::ConfigGeneration>,
        security: &Arc<crate::security::SecurityPolicy>,
        route: &super::ChannelRouteSelection,
        cancellation: &tokio_util::sync::CancellationToken,
        admission: &mut super::decision_source::ChannelAdmissionGuard,
    ) -> anyhow::Result<Self> {
        let service = ctx.decisions.publish(ctx, generation, security).await?;
        let proof = source.bind_model(&route.provider, &route.model)?;
        let model = service.resolve_agent_model(&proof)?;
        let route = crate::llm::route_decision::RouteDecision::single_candidate_for_context(
            model.provider_label().to_owned(),
            model.model().to_owned(),
            proof.owner_id().to_owned(),
            proof.session_key().to_owned(),
            proof.source_event_id().map(str::to_owned),
            None,
            "channel_reply",
            u32::try_from(proof.routing_input().chars().count() / 4).unwrap_or(u32::MAX),
            service.model_can_call_tools(&proof),
            true,
        );
        let context = service
            .decide_agent_initial(
                &proof,
                crate::runtime::turn_decision::AgentTurnKind::SingleShot,
                cancellation,
            )
            .await?;
        let envelope = source.envelope().clone();
        let trace = Arc::new(crate::agent::turn_contracts::RetainedProviderTrace::default());
        let terminal = crate::runtime::turn_terminal::TurnTerminalGuard::new(
            admission.take_terminal_ticket()?,
            Arc::clone(&service),
            Arc::clone(&context),
            crate::memory::MemoryFabric::new(Arc::clone(&ctx.memory), envelope.workspace_id.clone())
                .with_event_recording(generation.effective.memory.event_recording_config()),
            Arc::clone(&generation.effective),
            envelope.clone(),
            None,
        )?
        .with_provider_trace(Arc::clone(&trace), route.clone());
        Ok(Self {
            service,
            proof,
            context,
            terminal,
            envelope,
            config: Arc::clone(&generation.effective),
            generation: Arc::clone(generation),
            trace,
            route,
        })
    }

    pub(super) fn with_child_cleanup(
        mut self,
        acknowledgement: tokio::sync::oneshot::Receiver<Result<(), String>>,
    ) -> Self {
        self.terminal = self.terminal.with_child_cleanup(acknowledgement);
        self
    }

    pub(super) fn vision_accounting(&self) -> anyhow::Result<Arc<crate::runtime::auxiliary_call::AuxiliaryAccounting>> {
        self.service.auxiliary_accounting(
            &self.proof,
            &self.context,
            crate::runtime::auxiliary_call::AuxiliaryPurpose::SignalVision,
        )
    }

    pub(super) fn smart_group_classifier(&mut self) -> anyhow::Result<SmartGroupClassifierCall> {
        let config = &self.generation.effective.smart_group;
        let accounting = self.service.smart_group_accounting(
            &self.proof,
            &self.context,
            &config.classifier_provider,
            &config.classifier_model,
        )?;
        let selected = accounting.selected_model();
        let route = crate::llm::route_decision::RouteDecision::single_candidate_for_context(
            selected.provider_label().to_owned(),
            selected.model().to_owned(),
            self.proof.owner_id().to_owned(),
            self.proof.session_key().to_owned(),
            self.proof.source_event_id().map(str::to_owned),
            None,
            "smart_group_classifier",
            u32::try_from(self.proof.routing_input().chars().count() / 4).unwrap_or(u32::MAX),
            false,
            false,
        );
        let observation = crate::runtime::auxiliary_call::AuxiliaryProviderCall::new_accounted(
            crate::runtime::auxiliary_call::AuxiliaryPurpose::SmartGroupClassifier,
            route.clone(),
            accounting.as_ref(),
        );
        self.terminal.register_auxiliary_call(Arc::clone(&observation))?;
        Ok(SmartGroupClassifierCall {
            accounting,
            observation,
            route,
            context: Arc::clone(&self.context),
        })
    }

    pub(super) fn register_vision_observation(
        &mut self,
        accounting: &crate::runtime::auxiliary_call::AuxiliaryAccounting,
    ) -> anyhow::Result<Arc<crate::runtime::auxiliary_call::AuxiliaryProviderCall>> {
        let observation = crate::runtime::auxiliary_call::AuxiliaryProviderCall::new_accounted(
            crate::runtime::auxiliary_call::AuxiliaryPurpose::SignalVision,
            self.route.clone(),
            accounting,
        );
        self.terminal.register_auxiliary_call(Arc::clone(&observation))?;
        Ok(observation)
    }

    pub(super) fn build_provider(
        &self,
    ) -> anyhow::Result<(super::ChannelRouteSelection, Arc<dyn crate::providers::Provider>)> {
        let resolved = self.service.resolve_agent_model(&self.proof)?;
        let catalog = crate::providers::selection::catalog_for(&self.config);
        let provider = crate::providers::create_provider_for(
            &resolved,
            &catalog,
            &self.config.reliability,
            &self.config.cost,
            &crate::providers::provider_runtime_options_from_config(&self.config),
        )?;
        Ok((
            super::ChannelRouteSelection {
                provider: resolved.provider_label().to_owned(),
                model: resolved.model().to_owned(),
            },
            Arc::from(provider),
        ))
    }

    pub(super) fn close_reply(&self) -> anyhow::Result<Option<String>> {
        Ok(match self.context.disposition()? {
            TurnDisposition::Close { reply } => Some(reply.into_owned()),
            TurnDisposition::Run => None,
            TurnDisposition::CollectApprovalArguments => {
                anyhow::bail!("channel has no approval collection adapter")
            }
        })
    }

    pub(super) async fn close(
        &self,
        ctx: &super::ChannelRuntimeContext,
        security: &crate::security::SecurityPolicy,
        message: &super::traits::ChannelMessage,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        let ports = ChannelClose {
            turn: self,
            ctx,
            security,
            message,
        };
        self.service
            .close_agent_directive_turn(&self.proof, Arc::clone(&self.context), &ports, cancellation)
            .await?;
        Ok(())
    }

    pub(super) fn routing(&self) -> crate::agent::turn_request::TurnDecision<'_> {
        crate::agent::turn_request::TurnDecision {
            service: &self.service,
            context: &self.context,
            proof: &self.proof,
        }
    }
    pub(super) fn compaction(&self) -> &crate::config::AgentCompactionConfig {
        self.context.main_compaction()
    }
    pub(super) fn prompt_skills(&self) -> anyhow::Result<Vec<crate::skills::Skill>> {
        self.service.agent_prompt_skills(&self.proof, &self.context)
    }
    pub(super) fn memory_context(&self) -> anyhow::Result<String> {
        crate::agent::memory_loader::format_selected_context(None, self.context.retrieved()?)
    }
    pub(super) fn adapter(&self) -> crate::agent::turn_contracts::ToolLoopRuntimeAdapter {
        let mut execution =
            crate::tools::ToolExecutionContext::new(self.envelope.clone(), self.proof.policy_chat_type());
        execution.chat_id = self.envelope.recipient.clone().unwrap_or_default();
        crate::agent::turn_contracts::ToolLoopRuntimeAdapter {
            verify_unexecuted_reply: false,
            provider_trace: Some(Arc::clone(&self.trace)),
            events: None,
            stream_provider: false,
            tool_execution_service: None,
            tool_execution_context: execution,
            allowed_tool_names: None,
        }
    }
    pub(super) async fn fail(&self, error: &anyhow::Error, cancelled: bool) -> anyhow::Result<()> {
        self.terminal.finish_failure(error, cancelled).await
    }

    pub(super) async fn commit(&self, mut proposal: TurnTerminalDraft) -> anyhow::Result<TurnTerminalReceipt> {
        proposal.decision = Some(self.service.terminal_settlement_ref(Arc::clone(&self.context))?);
        // Terminal provenance belongs to the admitted sender; assistant history
        // has a separate projection scope and cannot replace source authority.
        if proposal.history.is_some() {
            proposal.history_scope = Some(proposal.scope.clone().with_sender("prx"));
        }
        proposal.scope = self.envelope.message_scope();
        self.terminal.commit(proposal).await
    }
}

struct ChannelClose<'a> {
    turn: &'a ChannelDecisionTurn,
    ctx: &'a super::ChannelRuntimeContext,
    security: &'a crate::security::SecurityPolicy,
    message: &'a super::traits::ChannelMessage,
}
#[async_trait::async_trait]
impl crate::runtime::turn_decision::CloseTurnPorts for ChannelClose<'_> {
    type Receipt = TurnTerminalReceipt;
    async fn finalize_closed_turn(
        &self,
        context: Arc<TurnDecisionContext>,
        reply: &str,
    ) -> anyhow::Result<Self::Receipt> {
        anyhow::ensure!(
            Arc::ptr_eq(&context, &self.turn.context),
            "channel close decision changed"
        );
        super::authorize_channel_outbound(self.ctx, self.security, &self.message.channel)
            .map_err(|reason| anyhow::anyhow!("channel decision outbound denied: {reason}"))?;
        let channel = self.ctx.channels_by_name.get(&self.message.channel);
        anyhow::ensure!(
            super::ensure_notice_before_channel_output(self.ctx, &self.turn.generation, self.message, channel).await,
            "channel decision identity notice was not acknowledged"
        );
        self.turn
            .commit(TurnTerminalDraft {
                decision: None,
                terminal_id: self.turn.proof.run_id().to_owned(),
                scope: self.turn.envelope.message_scope(),
                status: crate::agent::terminal::TurnTerminalStatus::Completed,
                history: Some(crate::agent::terminal::TurnHistoryProjection {
                    assistant_content: reply.to_owned(),
                    history_commit_len: 2,
                }),
                history_scope: None,
                provider_outcome: None,
                telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                    summary: "channel decision closed".to_owned(),
                    started_at: chrono::Utc::now(),
                    finished_at: chrono::Utc::now(),
                },
                delivery_intent: crate::agent::terminal::TurnDeliveryIntent::Reply {
                    target: self.message.reply_target.clone(),
                },
            })
            .await
    }
}
