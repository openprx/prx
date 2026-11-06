//! Exact terminal proposals retained across cancellation and future drop.
use super::turn_decision::VerifiedDirectiveApplication;
use crate::causal_tree::outcome::{OutcomeCorrelator, TerminalFacts};
use crate::causal_tree::state::TurnSurfaceKind;
use crate::causal_tree::store::{DecisionScope, Enqueue};
use crate::runtime::turn_decision::TurnDecisionService;
use anyhow::Context;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
        // A failed foreground caller may have lost its return trace after a
        // real provider reply. Use the same observed evidence as abort_commit;
        // the single terminal writer still owns settlement and replay.
        if proposal.provider_outcome.is_none()
            && matches!(
                proposal.status,
                crate::agent::terminal::TurnTerminalStatus::Failed
                    | crate::agent::terminal::TurnTerminalStatus::Cancelled
            )
            && let Some((retained, route)) = &self.provider_trace
        {
            proposal.provider_outcome = retained
                .snapshot()
                .0
                .map(|trace| crate::agent::terminal::provider_outcome_from_trace(route, self.started_at, trace));
        }
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
        Box::pin(self.flush()).await
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
        persist_auxiliary_replies(&self.auxiliary_calls, &self.fabric, &self.config, &mut proposal).await?;
        // Settling a completed auxiliary call does not assert parent or child
        // completion. Preserve its known cost even when descendant drain fails.
        if let Some(cleanup) = &self.child_cleanup {
            cleanup.wait().await?;
        }
        // Keep the canonical writer in its existing owner, with a bounded
        // heap await state rather than adding another terminal task owner.
        let receipt = Box::pin(finalize_and_rejoin(&self.service, &self.fabric, &self.config, proposal)).await?;
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
        Box::pin(self.flush()).await.map(|_| ())
    }
}

type AuxiliaryReceipt = (
    Arc<super::auxiliary_call::AuxiliaryProviderCall>,
    Option<crate::memory::MessageEvent>,
);

/// Persist every registered call independently: a failure on one must not skip
/// the receipt of another, so all are attempted. Returns the persisted
/// receipts and the combined failures.
async fn persist_auxiliary_events(
    calls: &[Arc<super::auxiliary_call::AuxiliaryProviderCall>],
    fabric: &crate::memory::MemoryFabric,
    config: &crate::config::Config,
    proposal: &mut crate::agent::terminal::TurnTerminalDraft,
) -> (Vec<AuxiliaryReceipt>, Vec<String>) {
    let mut receipts = Vec::new();
    let mut failures = Vec::new();
    for call in calls {
        match call.persist(fabric, config, proposal).await {
            Ok(event) => receipts.push((Arc::clone(call), event)),
            Err(error) => failures.push(format!("{}: {error:#}", call.purpose_label())),
        }
    }
    (receipts, failures)
}

/// Persist every auxiliary receipt and only then report
/// any failure so one bad call never hides another's receipt.
async fn persist_auxiliary_replies(
    calls: &[Arc<super::auxiliary_call::AuxiliaryProviderCall>],
    fabric: &crate::memory::MemoryFabric,
    config: &crate::config::Config,
    proposal: &mut crate::agent::terminal::TurnTerminalDraft,
) -> anyhow::Result<()> {
    let (_, failures) = persist_auxiliary_events(calls, fabric, config, proposal).await;
    anyhow::ensure!(
        failures.is_empty(),
        "auxiliary receipt persistence failed: {}",
        failures.join("; ")
    );
    Ok(())
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
        AuxiliaryProviderCall::new_for(purpose, route(purpose), "0".repeat(64))
    }

    fn route(purpose: AuxiliaryPurpose) -> crate::llm::route_decision::RouteDecision {
        crate::llm::route_decision::RouteDecision::single_candidate_for_context(
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
        )
    }

    fn draft(scope: crate::memory::MessageEventScope) -> crate::agent::terminal::TurnTerminalDraft {
        let now = chrono::Utc::now();
        crate::agent::terminal::TurnTerminalDraft {
            decision: None,
            terminal_id: "prx_aux_terminal".to_owned(),
            scope,
            status: crate::agent::terminal::TurnTerminalStatus::Failed,
            history: None,
            history_scope: None,
            provider_outcome: None,
            telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                summary: "aux".to_owned(),
                started_at: now,
                finished_at: now,
            },
            delivery_intent: crate::agent::terminal::TurnDeliveryIntent::None,
        }
    }

    #[tokio::test]
    async fn a_failing_first_call_does_not_skip_the_second_receipt() {
        let temp = tempfile::tempdir().expect("test: workspace");
        let mut config = crate::config::Config::default();
        config.workspace_dir = temp.path().to_path_buf();
        let memory: Arc<dyn crate::memory::Memory> =
            Arc::new(crate::memory::SqliteMemory::new(temp.path()).expect("test: sqlite"));
        let fabric = crate::memory::MemoryFabric::new(Arc::clone(&memory), temp.path().to_string_lossy())
            .with_event_recording(config.memory.event_recording_config());
        // An invalid accounting scope makes the first call's persistence fail.
        let route = route(AuxiliaryPurpose::SmartGroupClassifier);
        let broken = AuxiliaryProviderCall::new_for(AuxiliaryPurpose::SmartGroupClassifier, route, "bad".to_owned());
        let healthy = call(AuxiliaryPurpose::SignalVision);
        for call in [&broken, &healthy] {
            call.begin().expect("test: begin");
        }
        let scope = crate::memory::MessageEventScope::new("test", crate::memory::MemoryVisibility::Workspace)
            .with_owner_id("prx_aux_owner")
            .with_run_id("prx_aux_run")
            .with_session_key("prx_aux_session");
        let mut proposal = draft(scope);
        let (receipts, failures) = persist_auxiliary_events(&[broken, healthy], &fabric, &config, &mut proposal).await;
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(
            failures
                .iter()
                .all(|failure| failure.contains("smart_group_classifier")),
            "{failures:?}"
        );
        assert_eq!(receipts.len(), 1, "the second call still persisted");
        assert!(receipts.iter().all(|(call, _)| call.purpose_label() == "signal_vision"));
    }

    #[tokio::test]
    async fn classifier_and_vision_on_one_terminal_persist_two_settled_events() {
        let temp = tempfile::tempdir().expect("test: workspace");
        let mut config = crate::config::Config::default();
        config.workspace_dir = temp.path().to_path_buf();
        let memory: Arc<dyn crate::memory::Memory> =
            Arc::new(crate::memory::SqliteMemory::new(temp.path()).expect("test: sqlite"));
        let fabric = crate::memory::MemoryFabric::new(Arc::clone(&memory), temp.path().to_string_lossy())
            .with_event_recording(config.memory.event_recording_config());
        let calls = [
            call(AuxiliaryPurpose::SmartGroupClassifier),
            call(AuxiliaryPurpose::SignalVision),
        ];
        for call in &calls {
            call.begin().expect("test: begin");
        }
        let scope = crate::memory::MessageEventScope::new("test", crate::memory::MemoryVisibility::Workspace)
            .with_owner_id("prx_aux_owner")
            .with_run_id("prx_aux_run")
            .with_session_key("prx_aux_session");
        let mut proposal = draft(scope);
        let (receipts, failures) = persist_auxiliary_events(&calls, &fabric, &config, &mut proposal).await;
        assert!(failures.is_empty(), "{failures:?}");
        let events: Vec<_> = receipts.into_iter().filter_map(|(_, event)| event).collect();
        assert_eq!(events.len(), 2, "one settled event per registered call");
        assert!(
            events
                .iter()
                .all(|event| event.event_type == "provider.auxiliary.settled")
        );
        let ids: std::collections::HashSet<_> = events.iter().map(|event| &event.event_id).collect();
        assert_eq!(ids.len(), 2, "distinct events");
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

/// Commit through the shared finalizer before rejoining the decision journal.
async fn finalize_and_rejoin(
    service: &TurnDecisionService,
    fabric: &crate::memory::MemoryFabric,
    config: &crate::config::Config,
    proposal: crate::agent::terminal::TurnTerminalDraft,
) -> anyhow::Result<crate::agent::terminal::TurnTerminalReceipt> {
    #[cfg(test)]
    let mut stage =
        super::turn_cleanup::diagnostic::Stage::current(super::turn_cleanup::diagnostic::StageName::TerminalFinalize);
    let recorded = proposal.decision.as_ref().is_some_and(|link| link.is_recorded());
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
    if recorded {
        service
            .recovery_scope()
            .rejoin_terminal(&receipt.terminal_id, owner)
            .await?;
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
                        persist_auxiliary_replies(&auxiliary_calls, &fabric, &config, &mut proposal).await?;
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

// Authenticated historical decision evidence remains readable during migration.
impl super::ingress_recovery::RecoveryScope {
    pub(super) async fn rejoin_terminal_inner(
        &self,
        terminal_id: &str,
        owner_id: &str,
        bounded: Option<(
            &crate::memory::ingress_recovery::IngressRecoveryScope,
            Instant,
            &Arc<super::turn_cleanup::CleanupTracker>,
        )>,
    ) -> anyhow::Result<Enqueue> {
        #[cfg(test)]
        let mut stage = super::turn_cleanup::diagnostic::Stage::current(
            super::turn_cleanup::diagnostic::StageName::RejoinPreconditions,
        );
        let workspace_id = self.workspace.to_string_lossy();
        anyhow::ensure!(
            matches!(
                self.surface,
                TurnSurfaceKind::AgentLoop
                    | TurnSurfaceKind::Interactive
                    | TurnSurfaceKind::Server
                    | TurnSurfaceKind::Cron
                    | TurnSurfaceKind::Xin
                    | TurnSurfaceKind::Worker
                    | TurnSurfaceKind::Channel
            ) && self.recordable
                && self.durable_key,
            "terminal rejoin requires an enabled pinned ingress store"
        );
        anyhow::ensure!(
            !terminal_id.is_empty() && !owner_id.is_empty(),
            "terminal rejoin scope is empty"
        );
        #[cfg(test)]
        stage.next(super::turn_cleanup::diagnostic::StageName::RejoinLookup);
        let final_key = format!("turn:{terminal_id}:final");
        let event = if let Some((scope, deadline, _)) = bounded {
            self.memory
                .read_ingress_event(
                    scope,
                    &crate::memory::ingress_recovery::IngressEventLookup::IdempotencyKey(final_key.clone()),
                    deadline,
                )
                .await?
        } else {
            self.memory
                .find_message_event_by_idempotency_key(&workspace_id, &final_key)
                .await?
        }
        .context("committed terminal marker is missing")?;
        anyhow::ensure!(
            event.idempotency_key.as_deref() == Some(final_key.as_str()),
            "terminal lookup returned a different marker"
        );
        #[cfg(test)]
        stage.next(super::turn_cleanup::diagnostic::StageName::RejoinReplay);
        anyhow::ensure!(
            event.workspace_id == workspace_id && event.owner_id.as_deref() == Some(owner_id),
            "terminal replay scope does not match its owner"
        );
        let store = self
            .store
            .as_ref()
            .context("historical terminal evidence store is unavailable")?;
        let store_workspace_id = store.workspace_id().context("store workspace scope is missing")?;
        let replay = if let Some((_, deadline, owner)) = bounded {
            crate::agent::terminal::verify_terminal_replay_until(
                self.memory.as_ref(),
                &store.reader(),
                &self.state_key,
                &store_workspace_id,
                &workspace_id,
                owner_id,
                terminal_id,
                &event,
                deadline,
                owner,
            )
            .await?
        } else {
            crate::agent::terminal::verify_terminal_replay(
                self.memory.as_ref(),
                &store.reader(),
                &self.state_key,
                &store_workspace_id,
                &workspace_id,
                owner_id,
                terminal_id,
                &event,
            )
            .await?
        };
        #[cfg(test)]
        stage.next(super::turn_cleanup::diagnostic::StageName::RejoinReplayScope);
        anyhow::ensure!(
            super::turn_source::ingress_event_surface(
                &replay.source().source,
                replay.source().channel.as_deref(),
                &replay.source().visibility
            ) == Some(self.surface),
            "terminal replay belongs to another ingress surface"
        );
        anyhow::ensure!(
            replay
                .parent_decision_id()
                .map(crate::tools::capability::child::ParentAuthorityId::from)
                == self.parent_authority_id,
            "terminal replay belongs to another parent decision"
        );
        #[cfg(test)]
        stage.next(super::turn_cleanup::diagnostic::StageName::RejoinFacts);
        let decision = replay.decision();
        let facts = TerminalFacts::from_persisted_marker(&event, decision, owner_id, &workspace_id)?
            .with_verified_resume(&replay);
        let writer = store.writer().context("terminal rejoin writer is missing")?;
        #[cfg(test)]
        stage.next(super::turn_cleanup::diagnostic::StageName::RejoinCorrelator);
        let expected = OutcomeCorrelator::persisted_record(decision, &facts)?;
        #[cfg(test)]
        stage.next(super::turn_cleanup::diagnostic::StageName::RejoinRecord);
        let queued = writer.record_outcome(expected.clone());
        anyhow::ensure!(queued == Enqueue::Queued, "rejoined outcome could not be queued");
        #[cfg(test)]
        stage.next(super::turn_cleanup::diagnostic::StageName::RejoinDrain);
        anyhow::ensure!(
            writer
                .drain(bounded.map_or_else(|| Instant::now() + Duration::from_secs(5), |(_, deadline, _)| deadline))
                .await,
            "rejoined outcome persistence timed out"
        );
        #[cfg(test)]
        stage.next(super::turn_cleanup::diagnostic::StageName::RejoinConfirmAndRepairs);
        let scope = DecisionScope::Owner {
            workspace_id: store_workspace_id,
            owner_id: Arc::from(owner_id),
        };
        anyhow::ensure!(
            if let Some((_, deadline, owner)) = bounded {
                let reader = store.reader();
                let scope = scope.clone();
                let expected = expected.clone();
                super::ingress_recovery::owned_store_read(owner, move || {
                    Ok(reader.outcome_matches_until(&scope, &expected, deadline)?)
                })
                .await?
            } else {
                store.reader().outcome_matches(&scope, &expected)?
            },
            "rejoined outcome is absent or conflicts with the authenticated terminal"
        );
        let (terminal_status, awaiting_approval, verified) = facts.recovered_signals(&decision.directive_kind);
        crate::skills::lifecycle::spawn_settle_task_terminal(
            self.workspace.clone(),
            Some(owner_id.to_owned()),
            crate::skills::lifecycle::task_key(
                super::turn_source::subject_task_id(event.subject.as_ref()),
                event.run_id.as_deref(),
                event.parent_run_id.as_deref(),
            ),
            terminal_id.to_owned(),
            crate::skills::lifecycle::TerminalObservation {
                recovered: true,
                root: event.parent_run_id.is_none(),
                ..terminal_status.skill_observation(awaiting_approval, verified)
            },
        );
        if replay.parent_decision_id().is_some() {
            let link = crate::causal_tree::store::ChildOutcomeLink::from_replay(&replay, &expected)
                .context("child outcome lineage is inconsistent")?;
            anyhow::ensure!(
                writer.record_child_outcome(link) == Enqueue::Queued,
                "child outcome link could not be queued"
            );
            anyhow::ensure!(
                writer
                    .drain(bounded.map_or_else(|| Instant::now() + Duration::from_secs(5), |(_, deadline, _)| deadline))
                    .await,
                "child outcome link persistence timed out"
            );
            let expected_link = crate::causal_tree::store::ChildOutcomeLink::from_replay(&replay, &expected)
                .context("child outcome lineage changed")?;
            anyhow::ensure!(
                if let Some((_, deadline, owner)) = bounded {
                    let reader = store.reader();
                    let scope = scope.clone();
                    super::ingress_recovery::owned_store_read(owner, move || {
                        Ok(reader.child_outcome_matches_until(&scope, &expected_link, deadline)?)
                    })
                    .await?
                } else {
                    store.reader().child_outcome_matches(&scope, &expected_link)?
                },
                "child outcome link is absent or conflicts with authenticated facts"
            );
        }
        if replay.fixed_close()
            || facts.has_completed_run_evidence(decision, replay.run_capabilities())
            || (decision
                .selected()
                .is_some_and(|candidate| candidate.action == "use_tool")
                && facts.has_completed_resume_evidence(replay.run_capabilities()))
        {
            anyhow::ensure!(
                !replay.fixed_close() || facts.is_completed_local_close(),
                "replay is not a completed local close"
            );
            let application = VerifiedDirectiveApplication::from_replay(&replay)?;
            let sink = self
                .durable_events
                .as_ref()
                .context("applied replay sink is unavailable")?;
            if let Some((_, deadline, _)) = bounded {
                sink.repair_applied_until(application, deadline).await?;
            } else {
                anyhow::ensure!(
                    sink.repair_applied(application, Instant::now() + Duration::from_secs(5))
                        .await,
                    "rejoined applied event is absent or conflicts with the authenticated terminal"
                );
            }
        }
        if replay.source().source == crate::memory::MessageEventSource::Console {
            // Only authenticated replay reaches this projection. Each event is
            // rechecked in the backend transaction; replay cannot duplicate it.
            if let Some((_, deadline, _)) = bounded {
                self.memory
                    .project_console_message_until(replay.source(), deadline)
                    .await?;
                if let Some(assistant) = replay.assistant() {
                    self.memory.project_console_message_until(assistant, deadline).await?;
                }
            } else {
                self.memory.project_console_message(replay.source()).await?;
                if let Some(assistant) = replay.assistant() {
                    self.memory.project_console_message(assistant).await?;
                }
            }
        }
        #[cfg(test)]
        stage.finish();
        Ok(queued)
    }
}
