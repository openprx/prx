//! Exact terminal proposals retained across cancellation and future drop.
#[cfg(feature = "llm-router")]
mod automix;
use crate::runtime::turn_decision::TurnDecisionService;
#[cfg(feature = "llm-router")]
pub(crate) use automix::SettledAutomixContinuation;
use std::sync::Arc;

/// Observer-owned projection of confirmed segment usage, bounded by this task's
/// completed segments rather than the workspace event catalog.
pub(crate) struct TerminalUsageCollector {
    workspace: String,
    owner: String,
    task_id: String,
    require_durable: bool,
    journal: Option<ProcessSegmentJournal>,
    previous_observation:
        parking_lot::Mutex<Option<Arc<super::turn_decision::segment_observation::SegmentObservation>>>,
    records: parking_lot::Mutex<std::collections::BTreeMap<String, CollectedSegmentTerminal>>,
}

enum ProcessSegmentJournal {
    Ordinary(crate::runtime::child_source::ordinary_process::journal::OrdinarySegmentJournal),
    Operator(crate::runtime::child_source::operator_process::journal::OperatorSegmentJournal),
}

struct CollectedSegmentTerminal {
    reference: Option<crate::session_worker::protocol::WorkerSegmentTerminal>,
    payload_digest: Option<[u8; 32]>,
    usage: Option<crate::llm::route_decision::MeteredTokenUsageRecord>,
}

impl std::fmt::Debug for TerminalUsageCollector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TerminalUsageCollector")
    }
}

impl TerminalUsageCollector {
    pub(crate) const fn new(workspace: String, owner: String, task_id: String) -> Self {
        Self {
            workspace,
            owner,
            task_id,
            require_durable: true,
            journal: None,
            previous_observation: parking_lot::Mutex::new(None),
            records: parking_lot::Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    /// Ordinary completion can be confirmed locally while message recording
    /// is disabled. Such a receipt never supplies a durable event reference.
    pub(crate) fn new_ordinary(
        workspace: String,
        owner: String,
        task_id: String,
        recording: crate::memory::MemoryEventRecording,
    ) -> Self {
        Self {
            require_durable: recording.enabled,
            ..Self::new(workspace, owner, task_id)
        }
    }

    pub(crate) fn with_ordinary_journal(
        mut self,
        journal: crate::runtime::child_source::ordinary_process::journal::OrdinarySegmentJournal,
    ) -> Self {
        self.journal = Some(ProcessSegmentJournal::Ordinary(journal));
        self
    }

    pub(crate) fn with_operator_journal(
        mut self,
        journal: crate::runtime::child_source::operator_process::journal::OperatorSegmentJournal,
    ) -> Self {
        self.journal = Some(ProcessSegmentJournal::Operator(journal));
        self
    }

    async fn record_with_recovery(
        &self,
        receipt: &crate::agent::terminal::TurnTerminalReceipt,
        context: &crate::runtime::turn_decision::TurnDecisionContext,
        envelope: &crate::runtime::envelope::RuntimeEnvelope,
    ) -> anyhow::Result<()> {
        self.record(receipt)?;
        match &self.journal {
            Some(ProcessSegmentJournal::Ordinary(journal)) => journal.terminal(receipt).await?,
            Some(ProcessSegmentJournal::Operator(journal)) => journal.terminal(receipt).await?,
            None => {}
        }
        if self.require_durable && envelope.source_message_event_id.is_some() {
            let observation =
                super::turn_decision::segment_observation::SegmentObservation::confirmed(ConfirmedSegmentTerminal {
                    receipt,
                    context,
                    envelope,
                })?;
            let mut previous = self.previous_observation.lock();
            if let Some(existing) = previous.as_ref() {
                observation.validate_successor(existing)?;
            }
            *previous = Some(Arc::new(observation));
        }
        Ok(())
    }

    pub(super) fn previous_observation(
        &self,
        proof: &super::turn_source::OwnedTurnSourceProof,
        envelope: &crate::runtime::envelope::RuntimeEnvelope,
    ) -> anyhow::Result<Option<Arc<super::turn_decision::segment_observation::SegmentObservation>>> {
        use super::turn_source::TurnSourceProof;
        anyhow::ensure!(
            proof.workspace_id() == self.workspace
                && proof.owner_id() == self.owner
                && proof.task_id() == Some(self.task_id.as_str()),
            "segment observation collector differs from current task"
        );
        if proof.previous_segment_input().is_none() {
            return Ok(None);
        }
        anyhow::ensure!(self.require_durable, "segment predecessor has no durable collector");
        let previous = self.previous_observation.lock();
        let observation = previous
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("previous segment terminal is not confirmed"))?;
        observation.matches_next(proof, envelope)?;
        Ok(Some(Arc::clone(observation)))
    }

    fn record(&self, receipt: &crate::agent::terminal::TurnTerminalReceipt) -> anyhow::Result<()> {
        let event = &receipt.terminal_event;
        anyhow::ensure!(
            (if self.require_durable {
                event.id > 0
            } else {
                event.id == 0
            }) && event.workspace_id == self.workspace
                && event.owner_id.as_deref() == Some(self.owner.as_str())
                && event.subject == Some(crate::memory::MessageEventSubject::Task(self.task_id.clone()))
                && event.run_id.as_deref() == Some(receipt.terminal_id.as_str())
                && receipt.terminal_id != self.task_id
                && event.event_type == "turn.finalized",
            "segment usage receipt belongs to another task"
        );
        if let Some(usage) = &receipt.usage_settlement {
            anyhow::ensure!(
                usage.settlement_id.as_deref() == Some(receipt.terminal_id.as_str()),
                "segment usage settlement identity differs from terminal"
            );
        }
        use sha2::Digest;
        let reference = if self.require_durable {
            Some(crate::session_worker::protocol::WorkerSegmentTerminal {
                terminal_id: receipt.terminal_id.clone(),
                event_id: event.event_id.clone(),
                event_digest: format!("{:x}", sha2::Sha256::digest(serde_json::to_vec(event)?)),
            })
        } else {
            None
        };
        // Synthetic event IDs/timestamps may differ across retries; the exact
        // terminal proposal and settlement must still agree.
        let payload_digest = if self.require_durable {
            // The durable reference already hashes the complete event.
            None
        } else {
            Some(
                sha2::Sha256::digest(serde_json::to_vec(&(
                    &event.content,
                    &event.raw_payload_json,
                    &receipt.usage_settlement,
                ))?)
                .into(),
            )
        };
        let mut records = self.records.lock();
        if let Some(existing) = records.get(&receipt.terminal_id) {
            anyhow::ensure!(
                existing.reference == reference
                    && existing.payload_digest == payload_digest
                    && existing.usage == receipt.usage_settlement,
                "segment terminal conflicts with its prior receipt"
            );
        } else {
            records.insert(
                receipt.terminal_id.clone(),
                CollectedSegmentTerminal {
                    reference,
                    payload_digest,
                    usage: receipt.usage_settlement.clone(),
                },
            );
        }
        Ok(())
    }

    pub(crate) fn snapshot(&self) -> Vec<crate::llm::route_decision::MeteredTokenUsageRecord> {
        self.records
            .lock()
            .values()
            .filter_map(|record| record.usage.clone())
            .collect()
    }

    pub(crate) fn confirmed_count(&self) -> usize {
        self.records.lock().len()
    }

    pub(crate) fn confirmed_terminal_ids(&self) -> Vec<String> {
        self.records.lock().keys().cloned().collect()
    }

    pub(crate) fn terminal_references(&self) -> Vec<crate::session_worker::protocol::WorkerSegmentTerminal> {
        self.records
            .lock()
            .values()
            .filter_map(|record| record.reference.clone())
            .collect()
    }
}

/// Retains the same acknowledgement across cancellation of a flush. A
/// dropped waiter must not consume the only proof of descendant cleanup.
struct ChildCleanupBarrier(tokio::sync::Mutex<ChildCleanupState>);

/// Only terminal ownership can construct this witness, after finalization,
/// outcome rejoin and the existing task recovery journal have all succeeded.
pub(super) struct ConfirmedSegmentTerminal<'a> {
    receipt: &'a crate::agent::terminal::TurnTerminalReceipt,
    context: &'a crate::runtime::turn_decision::TurnDecisionContext,
    envelope: &'a crate::runtime::envelope::RuntimeEnvelope,
}
impl<'a> ConfirmedSegmentTerminal<'a> {
    pub(super) const fn parts(
        self,
    ) -> (
        &'a crate::agent::terminal::TurnTerminalReceipt,
        &'a crate::runtime::turn_decision::TurnDecisionContext,
        &'a crate::runtime::envelope::RuntimeEnvelope,
    ) {
        (self.receipt, self.context, self.envelope)
    }
}

enum ChildCleanupState {
    Pending(tokio::sync::oneshot::Receiver<Result<(), String>>),
    Finished(Result<(), String>),
}

impl ChildCleanupBarrier {
    async fn wait(&self) -> anyhow::Result<()> {
        let mut state = self.0.lock().await;
        let result = match &mut *state {
            ChildCleanupState::Pending(receiver) => receiver
                .await
                .unwrap_or_else(|_| Err("child cleanup acknowledgement disappeared".to_owned())),
            ChildCleanupState::Finished(result) => result.clone(),
        };
        *state = ChildCleanupState::Finished(result.clone());
        result.map_err(|reason| anyhow::anyhow!(reason).context(crate::runtime::turn_cleanup::CleanupIncomplete))
    }
}

/// Owns an exact terminal proposal across interruption of the caller future.
/// Once writing starts, cleanup retries that proposal, never a different status.
pub(crate) struct TurnTerminalGuard {
    child_cleanup: Option<Arc<ChildCleanupBarrier>>,
    cleanup_ticket: Option<crate::runtime::turn_cleanup::CleanupTicket>,
    usage_collector: Option<Arc<TerminalUsageCollector>>,
    cleanup_ack: Option<tokio::sync::oneshot::Sender<Result<(), String>>>,
    service: Arc<TurnDecisionService>,
    context: Arc<crate::runtime::turn_decision::TurnDecisionContext>,
    settlement: crate::agent::terminal::DecisionSettlementRef,
    fabric: crate::memory::MemoryFabric,
    config: Arc<crate::config::Config>,
    envelope: crate::runtime::envelope::RuntimeEnvelope,
    started_at: chrono::DateTime<chrono::Utc>,
    auxiliary_calls: Vec<Arc<super::auxiliary_call::AuxiliaryProviderCall>>,
    provider_trace: Option<(
        Arc<crate::agent::turn_contracts::RetainedProviderTrace>,
        crate::llm::route_decision::RouteDecision,
    )>,
    state: parking_lot::Mutex<TerminalState>,
}

#[derive(Default)]
struct TerminalState {
    #[cfg(feature = "llm-router")]
    automix_claimed: bool,
    pending: Option<crate::agent::terminal::TurnTerminalDraft>,
    finished: bool,
}

impl TurnTerminalGuard {
    pub(crate) fn new(
        cleanup_ticket: crate::runtime::turn_cleanup::CleanupTicket,
        service: Arc<TurnDecisionService>,
        context: Arc<crate::runtime::turn_decision::TurnDecisionContext>,
        fabric: crate::memory::MemoryFabric,
        config: Arc<crate::config::Config>,
        envelope: crate::runtime::envelope::RuntimeEnvelope,
        cleanup_ack: Option<tokio::sync::oneshot::Sender<Result<(), String>>>,
    ) -> anyhow::Result<Self> {
        let settlement = service.terminal_settlement_ref(Arc::clone(&context))?;
        Ok(Self {
            child_cleanup: None,
            cleanup_ticket: Some(cleanup_ticket),
            usage_collector: None,
            cleanup_ack,
            service,
            context,
            settlement,
            fabric,
            config,
            envelope,
            started_at: chrono::Utc::now(),
            auxiliary_calls: Vec::new(),
            provider_trace: None,
            state: parking_lot::Mutex::new(TerminalState::default()),
        })
    }

    /// Installed by the admitted auxiliary consumer before preparing or polling
    /// its request so terminal cancellation retains its observation owner.
    /// Each purpose is persisted under its own idempotency key, so the
    /// terminal keeps every registered call; registering a second call of the
    /// same purpose would overwrite its receipt and is refused.
    pub(crate) fn register_auxiliary_call(
        &mut self,
        call: Arc<super::auxiliary_call::AuxiliaryProviderCall>,
    ) -> anyhow::Result<()> {
        ensure_purpose_unclaimed(&self.auxiliary_calls, &call)?;
        self.auxiliary_calls.push(call);
        Ok(())
    }

    /// Persist the existing main aggregate settlement before an additional
    /// paid call. This does not reinterpret the main trace as per-attempt usage.
    #[cfg(feature = "llm-router")]
    pub(crate) async fn settle_main_before_auxiliary(
        &self,
        outcome: &crate::llm::route_decision::ProviderExecutionOutcome,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<Option<SettledAutomixContinuation>> {
        use crate::llm::route_decision::TokenUsageSource;
        if cancellation.is_cancelled() || !self.claim_automix_continuation(outcome)? {
            return Ok(None);
        }
        let usage = &outcome.tokens_used;
        if !matches!(usage.source, TokenUsageSource::Reported)
            || usage.prompt_tokens.is_none()
            || usage.completion_tokens.is_none()
            || self
                .provider_trace
                .as_ref()
                .is_some_and(|(trace, _)| trace.has_unknown_usage())
        {
            return Ok(None);
        }
        let terminal_id = self
            .envelope
            .run_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("main settlement has no issued run"))?;
        let Some(record) = crate::agent::terminal::usage_settlement(terminal_id, outcome, &self.config.cost)
            .filter(|record| record.cost_usd.is_some())
        else {
            return Ok(None);
        };
        let deadline = self.context.auxiliary_deadline();
        if std::time::Instant::now() >= deadline {
            return Ok(None);
        }
        let owner = self
            .cleanup_ticket
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("main settlement cleanup owner missing"))?
            .owner();
        let ticket = owner.register(tokio_util::sync::CancellationToken::new())?;
        let fabric = self.fabric.clone();
        let scope = self.envelope.message_scope();
        let cost = self.config.cost.clone();
        let workspace = self.config.workspace_dir.clone();
        let terminal_id = terminal_id.to_owned();
        let job = tokio::spawn(async move {
            let result = async {
                let payload = serde_json::to_string(&record)?;
                let event = fabric
                    .record_runtime_event_idempotent(
                        scope,
                        "usage.settled",
                        format!("settlement_id={terminal_id}"),
                        Some(payload.clone()),
                        format!("turn:{terminal_id}:usage"),
                    )
                    .await?;
                anyhow::ensure!(
                    event.id <= 0 || event.raw_payload_json.as_deref() == Some(payload.as_str()),
                    "main usage settlement conflicts"
                );
                let settled = crate::runtime::blocking::spawn_blocking(move || {
                    crate::cost::tracker::CostTracker::for_workspace(cost, &workspace)?.settle_metered(&record)
                })
                .await??;
                anyhow::ensure!(
                    matches!(
                        settled,
                        crate::cost::types::CostSettlement::Recorded { .. }
                            | crate::cost::types::CostSettlement::Replayed
                    ),
                    "main cost settlement is not confirmed"
                );
                Ok::<_, anyhow::Error>(())
            }
            .await;
            ticket.finish(result.is_ok());
            result
        });
        match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), job).await {
            Ok(joined) => {
                joined??;
                if cancellation.is_cancelled() || std::time::Instant::now() >= deadline {
                    return Ok(None);
                }
                Ok(Some(self.settled_automix_continuation(cancellation.clone())))
            }
            // The owned task and ticket remain alive until real I/O finishes.
            Err(_) => Ok(None),
        }
    }

    pub(crate) fn with_child_cleanup(
        mut self,
        acknowledgement: tokio::sync::oneshot::Receiver<Result<(), String>>,
    ) -> Self {
        self.child_cleanup = Some(Arc::new(ChildCleanupBarrier(tokio::sync::Mutex::new(
            ChildCleanupState::Pending(acknowledgement),
        ))));
        self
    }

    pub(crate) fn with_usage_collector(mut self, collector: Option<Arc<TerminalUsageCollector>>) -> Self {
        self.usage_collector = collector;
        self
    }

    pub(crate) fn with_provider_trace(
        mut self,
        trace: Arc<crate::agent::turn_contracts::RetainedProviderTrace>,
        route: crate::llm::route_decision::RouteDecision,
    ) -> Self {
        self.provider_trace = Some((trace, route));
        self
    }

    fn abort_commit(
        &self,
        status: crate::agent::terminal::TurnTerminalStatus,
        summary: String,
    ) -> crate::agent::terminal::TurnTerminalDraft {
        let mut summary = summary;
        let provider_outcome = self.provider_trace.as_ref().and_then(|(retained, route)| {
            let (trace, unknown) = retained.snapshot();
            if unknown {
                summary.push_str("; provider usage is incomplete: unresolved or unreported calls");
            }
            trace.map(|trace| crate::agent::terminal::provider_outcome_from_trace(route, self.started_at, trace))
        });
        crate::agent::terminal::TurnTerminalDraft {
            decision: Some(self.settlement.clone()),
            terminal_id: self.envelope.run_id.clone().unwrap_or_default(),
            scope: self.envelope.message_scope(),
            status,
            history: None,
            history_scope: None,
            provider_outcome,
            telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                summary,
                started_at: self.started_at,
                finished_at: chrono::Utc::now(),
            },
            delivery_intent: crate::agent::terminal::TurnDeliveryIntent::Suppress {
                reason: "runtime turn did not return a committed reply".to_owned(),
            },
        }
    }

    pub(crate) async fn commit(
        &self,
        mut proposal: crate::agent::terminal::TurnTerminalDraft,
    ) -> anyhow::Result<crate::agent::terminal::TurnTerminalReceipt> {
        bind_terminal_decision(&mut proposal, &self.settlement)?;
        if self
            .provider_trace
            .as_ref()
            .is_some_and(|(trace, _)| trace.has_unknown_usage())
        {
            proposal
                .telemetry
                .summary
                .push_str("; provider usage is incomplete: unresolved or unreported calls");
        }
        {
            let mut state = self.state.lock();
            anyhow::ensure!(
                !state.finished && state.pending.is_none(),
                "runtime terminal proposal already exists"
            );
            state.pending = Some(proposal);
        }
        self.flush().await
    }

    async fn flush(&self) -> anyhow::Result<crate::agent::terminal::TurnTerminalReceipt> {
        #[cfg(test)]
        let stage = super::turn_cleanup::diagnostic::Stage::ticket(
            self.cleanup_ticket.as_ref(),
            super::turn_cleanup::diagnostic::StageName::TerminalFlush,
        );
        let mut proposal = self
            .state
            .lock()
            .pending
            .clone()
            .ok_or_else(|| anyhow::anyhow!("runtime terminal proposal missing"))?;
        for call in &self.auxiliary_calls {
            persist_auxiliary_reply(call, &self.fabric, &self.config, &self.context, &mut proposal).await?;
        }
        // Settling a completed auxiliary call does not assert parent or child
        // completion. Preserve its known cost even when descendant drain fails.
        if let Some(cleanup) = &self.child_cleanup {
            cleanup.wait().await?;
        }
        let receipt = finalize_and_rejoin(&self.service, &self.fabric, &self.config, proposal).await?;
        if let Some(collector) = &self.usage_collector {
            collector
                .record_with_recovery(&receipt, &self.context, &self.envelope)
                .await?;
        }
        self.state.lock().finished = true;
        #[cfg(test)]
        stage.finish();
        Ok(receipt)
    }

    pub(crate) async fn finish_error(&self, error: &anyhow::Error) -> anyhow::Result<()> {
        self.finish_failure(error, false).await
    }

    pub(crate) async fn finish_failure(&self, error: &anyhow::Error, cancelled: bool) -> anyhow::Result<()> {
        {
            let mut state = self.state.lock();
            if state.finished {
                return Ok(());
            }
            if state.pending.is_none() {
                state.pending = Some(self.abort_commit(
                    if cancelled {
                        crate::agent::terminal::TurnTerminalStatus::Cancelled
                    } else {
                        crate::agent::terminal::TurnTerminalStatus::Failed
                    },
                    error.to_string(),
                ));
            }
        }
        self.flush().await.map(|_| ())
    }
}

fn ensure_purpose_unclaimed(
    registered: &[Arc<super::auxiliary_call::AuxiliaryProviderCall>],
    call: &super::auxiliary_call::AuxiliaryProviderCall,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        registered
            .iter()
            .all(|existing| existing.purpose_label() != call.purpose_label()),
        "terminal already owns a {} auxiliary provider call",
        call.purpose_label()
    );
    Ok(())
}

#[cfg(test)]
mod auxiliary_registration_tests {
    use super::*;
    use crate::runtime::auxiliary_call::{AuxiliaryProviderCall, AuxiliaryPurpose};

    fn call(purpose: AuxiliaryPurpose) -> Arc<AuxiliaryProviderCall> {
        let route = crate::llm::route_decision::RouteDecision::single_candidate_for_context(
            "prx_provider".to_owned(),
            "prx_model".to_owned(),
            "owner".to_owned(),
            "session".to_owned(),
            None,
            None,
            purpose.label(),
            1,
            false,
            false,
        );
        AuxiliaryProviderCall::new_for(purpose, route, "0".repeat(64))
    }

    #[test]
    fn distinct_purposes_coexist_but_a_repeat_would_overwrite_the_first_receipt() {
        let classifier = call(AuxiliaryPurpose::SmartGroupClassifier);
        let mut registered = vec![Arc::clone(&classifier)];
        let vision = call(AuxiliaryPurpose::SignalVision);
        ensure_purpose_unclaimed(&registered, &vision).expect("test: a different purpose is independent");
        registered.push(vision);
        let repeat = call(AuxiliaryPurpose::SmartGroupClassifier);
        let error = ensure_purpose_unclaimed(&registered, &repeat).expect_err("test: same purpose refused");
        assert!(error.to_string().contains("smart_group_classifier"));
    }
}

/// Both normal commit and interrupted pending-commit recovery use the same
/// real receipt binding. A successful auxiliary observation never turns a
/// Failed/Cancelled parent into a Completed reply.
async fn persist_auxiliary_reply(
    call: &super::auxiliary_call::AuxiliaryProviderCall,
    fabric: &crate::memory::MemoryFabric,
    config: &crate::config::Config,
    context: &crate::runtime::turn_decision::TurnDecisionContext,
    proposal: &mut crate::agent::terminal::TurnTerminalDraft,
) -> anyhow::Result<()> {
    let event = call.persist(fabric, config, proposal).await?;
    if proposal.status == crate::agent::terminal::TurnTerminalStatus::Completed && context.served_auxiliary().is_none()
    {
        if let Some(link) =
            crate::agent::terminal::served_auxiliary::ServedAuxiliaryLink::from_call(call, proposal, event)?
        {
            context.bind_served_auxiliary(link)?;
        }
    }
    Ok(())
}

async fn finalize_and_rejoin(
    service: &TurnDecisionService,
    fabric: &crate::memory::MemoryFabric,
    config: &crate::config::Config,
    proposal: crate::agent::terminal::TurnTerminalDraft,
) -> anyhow::Result<crate::agent::terminal::TurnTerminalReceipt> {
    #[cfg(test)]
    let mut stage =
        super::turn_cleanup::diagnostic::Stage::current(super::turn_cleanup::diagnostic::StageName::TerminalFinalize);
    let receipt = crate::agent::terminal::finalize_turn(fabric, proposal, &config.cost, &config.workspace_dir).await?;
    #[cfg(test)]
    stage.next(super::turn_cleanup::diagnostic::StageName::TerminalReceiptOwner);
    let owner = receipt
        .terminal_event
        .owner_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("runtime terminal owner missing"))?;
    #[cfg(test)]
    stage.next(super::turn_cleanup::diagnostic::StageName::TerminalRejoin);
    if config.causal_tree.enabled {
        service.rejoin_terminal(&receipt.terminal_id, owner).await?;
    }
    #[cfg(test)]
    stage.finish();
    Ok(receipt)
}

impl Drop for TurnTerminalGuard {
    fn drop(&mut self) {
        let state = self.state.get_mut();
        if state.finished {
            if let Some(ticket) = self.cleanup_ticket.take() {
                ticket.finish(true);
            }
            if let Some(ack) = self.cleanup_ack.take() {
                let _ = ack.send(Ok(()));
            }
            return;
        }
        let pending = state.pending.take();
        let mut proposal = pending.unwrap_or_else(|| {
            self.abort_commit(
                if std::thread::panicking() {
                    crate::agent::terminal::TurnTerminalStatus::Failed
                } else {
                    crate::agent::terminal::TurnTerminalStatus::Cancelled
                },
                "runtime turn future ended before terminal acknowledgement".to_owned(),
            )
        });
        let service = Arc::clone(&self.service);
        let fabric = self.fabric.clone();
        let config = Arc::clone(&self.config);
        let cleanup_ack = self.cleanup_ack.take();
        let cleanup_ticket = self.cleanup_ticket.take();
        #[cfg(test)]
        let diagnostic_binding = super::turn_cleanup::diagnostic::terminal_binding(cleanup_ticket.as_ref());
        let usage_collector = self.usage_collector.take();
        let context = Arc::clone(&self.context);
        let observation_envelope = usage_collector.as_ref().map(|_| self.envelope.clone());
        let child_cleanup = self.child_cleanup.take();
        let auxiliary_calls = std::mem::take(&mut self.auxiliary_calls);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let work = async move {
                    #[cfg(test)]
                    let stage = super::turn_cleanup::diagnostic::Stage::ticket(
                        cleanup_ticket.as_ref(),
                        super::turn_cleanup::diagnostic::StageName::TerminalDropFinalize,
                    );
                    let result = async {
                        for call in &auxiliary_calls {
                            persist_auxiliary_reply(call, &fabric, &config, &context, &mut proposal).await?;
                        }
                        if let Some(cleanup) = child_cleanup {
                            cleanup.wait().await?;
                        }
                        let receipt = finalize_and_rejoin(&service, &fabric, &config, proposal).await?;
                        if let Some(collector) = &usage_collector {
                            let envelope = observation_envelope
                                .as_ref()
                                .ok_or_else(|| anyhow::anyhow!("segment observation envelope missing"))?;
                            collector.record_with_recovery(&receipt, &context, envelope).await?;
                        }
                        Ok::<_, anyhow::Error>(receipt)
                    }
                    .await;
                    #[cfg(test)]
                    if result.is_ok() {
                        stage.finish();
                    } else {
                        drop(stage);
                    }
                    if let Err(error) = &result {
                        tracing::error!(error = %error, "Runtime interrupted terminal cleanup remains incomplete");
                    }
                    if let Some(ticket) = cleanup_ticket {
                        ticket.finish(result.is_ok());
                    }
                    if let Some(ack) = cleanup_ack {
                        let _ = ack.send(
                            result
                                .map(|_| ())
                                .map_err(|_| "terminal cleanup persistence failed".to_owned()),
                        );
                    }
                };
                #[cfg(test)]
                let work = super::turn_cleanup::diagnostic::terminal_scope(diagnostic_binding, work);
                handle.spawn(work);
            }
            Err(_) => {
                tracing::error!("Runtime terminal cleanup has no runtime; completion is unconfirmed");
                if let Some(ack) = cleanup_ack {
                    let _ = ack.send(Err("terminal cleanup runtime is unavailable".to_owned()));
                }
            }
        }
    }
}

/// The guard owns the terminal decision link. Draft omission cannot bypass
/// its settlement, and a supplied foreign link fails before retaining a draft.
pub(crate) fn bind_terminal_decision(
    proposal: &mut crate::agent::terminal::TurnTerminalDraft,
    owned: &crate::agent::terminal::DecisionSettlementRef,
) -> anyhow::Result<()> {
    if let Some(supplied) = proposal.decision.as_ref() {
        anyhow::ensure!(
            owned.same_context(supplied),
            "terminal proposal decision differs from its guard"
        );
    }
    proposal.decision = Some(owned.clone());
    Ok(())
}

/// All ordinary and interruption paths share the same exact proposal owner.
pub(crate) async fn commit_terminal(
    guard: Option<&TurnTerminalGuard>,
    fabric: &crate::memory::MemoryFabric,
    proposal: crate::agent::terminal::TurnTerminalDraft,
    config: &crate::config::Config,
) -> anyhow::Result<crate::agent::terminal::TurnTerminalReceipt> {
    if let Some(guard) = guard {
        guard.commit(proposal).await
    } else {
        crate::agent::terminal::finalize_turn(fabric, proposal, &config.cost, &config.workspace_dir).await
    }
}
