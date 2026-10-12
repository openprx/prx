//! One purpose-bound history hop inside the original Agent decision chain.
//! No constructor here can be reached through backend predictions or wire data.
mod embedding;

use super::accounting;
use super::router_replay_facts::{RouterReadFacts, provider_binding};

use super::{
    AgentDecisionError, AgentTurnKind, Arc, BTreeMap, BTreeSet, CancellationToken, CapabilityRef, CapabilityTarget,
    CausalDecisionEngine, CausalEventKind, CausalState, Context, DecisionId, DecisionKey, DecisionRecordBuilder,
    DecisionScope, DecisionTokenReservation, DerivedState, DirectiveContext, DiscoveredRefs, EngineDecision,
    EngineInputs, Enqueue, GateInputs, HopLineage, Instant, KeyDomain, Memory, NonZeroU8, ObservedState, ObservedStep,
    ObservedStepKind, ObservedStepStatus, PinnedCatalogRehearsal, RehearsalPorts, RequestLimits, Requirement,
    TrustedState, TurnBudgets, TurnDecisionContext, TurnDecisionService, TurnExecutionDirective, TurnSourceProof,
    UnknownPolicy, directive_from_selection, effective_agent_exclusions,
};
use crate::agent::embedded_source::{EmbeddedAgentSource, SelectedEmbeddedModel};
use crate::causal_tree::branch::CausalActionKind;
use crate::causal_tree::decision_select::Selection;
use crate::causal_tree::directive::SelectedReadPurpose;
use crate::causal_tree::state::RouterHistoryReadBinding;
use crate::memory::{RouterHistoryDomain, RouterHistoryRead, RouterHistoryReadEvidence, RouterHistoryWriteEvidence};
use anyhow::ensure;
use sha2::{Digest, Sha256};

pub(crate) struct RouterModelReadmission {
    source: Arc<EmbeddedAgentSource>,
    selected: SelectedEmbeddedModel,
}
impl RouterModelReadmission {
    pub(crate) fn consume_for(self, source: &Arc<EmbeddedAgentSource>) -> anyhow::Result<SelectedEmbeddedModel> {
        ensure!(
            Arc::ptr_eq(&self.source, source),
            "Router model admission belongs to another source"
        );
        Ok(self.selected)
    }
}

pub(crate) struct RouterEmbedding {
    source_event_id: String,
    query_sha256: [u8; 32],
    domain: RouterHistoryDomain,
    vector: Vec<f32>,
}
impl RouterEmbedding {
    pub(crate) fn into_parts(self) -> (String, [u8; 32], RouterHistoryDomain, Vec<f32>) {
        (self.source_event_id, self.query_sha256, self.domain, self.vector)
    }
}

/// Unique phase ownership prevents reminting a read grant after an error or
/// retry. It is moved to Reading before issuing the sole production grant.
#[expect(
    clippy::large_enum_variant,
    reason = "Inline continuation preserves the allocation budget; boxing would add a routing allocation"
)]
enum RoutingPhase {
    Pending,
    Reading,
    Consumed {
        source: Arc<EmbeddedAgentSource>,
        transition: Arc<RouterTransition>,
        embedding: Option<RouterEmbedding>,
        main_compaction: crate::config::AgentCompactionConfig,
    },
    Failed {
        transition: Arc<RouterTransition>,
    },
    Closed,
}

pub(crate) struct RouterContinuation<'a> {
    router: &'a crate::router::RouterEngine,
    original: Arc<EmbeddedAgentSource>,
    phase: RoutingPhase,
}
impl<'a> RouterContinuation<'a> {
    pub(crate) const fn new(router: &'a crate::router::RouterEngine, source: Arc<EmbeddedAgentSource>) -> Self {
        Self {
            router,
            original: source,
            phase: RoutingPhase::Pending,
        }
    }
    pub(super) const fn source(&self) -> &Arc<EmbeddedAgentSource> {
        match &self.phase {
            RoutingPhase::Consumed { source, .. } => source,
            _ => &self.original,
        }
    }
    pub(super) fn transition(&self) -> Option<Arc<RouterTransition>> {
        match &self.phase {
            RoutingPhase::Consumed { transition, .. } | RoutingPhase::Failed { transition } => {
                Some(Arc::clone(transition))
            }
            _ => None,
        }
    }
    pub(super) const fn failed(&self) -> bool {
        matches!(self.phase, RoutingPhase::Failed { .. })
    }
    pub(super) const fn main_compaction(&self) -> Option<&crate::config::AgentCompactionConfig> {
        match &self.phase {
            RoutingPhase::Consumed { main_compaction, .. } => Some(main_compaction),
            _ => None,
        }
    }
    pub(crate) fn finish(mut self) -> (Arc<EmbeddedAgentSource>, Option<RouterEmbedding>) {
        match &mut self.phase {
            RoutingPhase::Consumed { source, embedding, .. } => (Arc::clone(source), embedding.take()),
            _ => (Arc::clone(&self.original), None),
        }
    }
}

/// Private consumed facts, never reconstructed from a public terminal receipt.
/// Replay facts are exported only after comparison with the retained engine hops.
pub(super) struct RouterTransition {
    read_id: DecisionId,
    selected_id: crate::causal_tree::branch::CandidateId,
    reference: CapabilityRef,
    binding: RouterHistoryReadBinding,
    history_snapshot: Option<[u8; 32]>,
    failure_stage: Option<&'static str>,
    source_sequence: i64,
    original: Arc<EmbeddedAgentSource>,
    admitted: Arc<EmbeddedAgentSource>,
    route_binding: Option<[u8; 32]>,
    original_budget: TurnBudgets,
    remaining_budget: TurnBudgets,
    deadline: Instant,
}

/// The job owns both the actual backend future and its cleanup ticket. Dropping
/// an awaiting caller detaches this job, not SQLite's inner blocking join. The
/// owner can drain it until the actual backend has returned.
async fn read_owned(
    memory: Arc<dyn Memory>,
    evidence: RouterHistoryReadEvidence,
    deadline: Instant,
    owner: &Arc<crate::runtime::turn_cleanup::CleanupTracker>,
    cancellation: &CancellationToken,
) -> anyhow::Result<RouterHistoryRead> {
    let ticket = owner.register(cancellation.clone())?;
    tokio::spawn(async move {
        let result = memory.read_router_history(evidence, deadline).await;
        // Reads have no ambiguous commit. A returned error still confirms that
        // the backend completed; panic/drop leaves the ticket unconfirmed.
        ticket.finish(true);
        result
    })
    .await
    .context("Router history read owner task failed")?
}

pub(crate) async fn commit_training_owned(
    memory: Arc<dyn Memory>,
    evidence: RouterHistoryWriteEvidence,
    deadline: Instant,
    owner: &Arc<crate::runtime::turn_cleanup::CleanupTracker>,
    cancellation: &CancellationToken,
) -> anyhow::Result<crate::memory::RouterHistoryCommitReceipt> {
    let ticket = owner.register(cancellation.clone())?;
    tokio::spawn(async move {
        let result = memory.commit_router_history(&evidence, deadline).await;
        // An error during PostgreSQL COMMIT may leave its outcome unknown.
        // The backend's real commit receipt is the only success acknowledgement.
        ticket.finish(result.is_ok());
        result
    })
    .await
    .context("Router history write owner task failed")?
}

impl TurnDecisionService {
    pub(crate) async fn decide_embedded_router_initial(
        &self,
        source: Arc<EmbeddedAgentSource>,
        router: &crate::router::RouterEngine,
        cancellation: &CancellationToken,
    ) -> Result<
        (
            Arc<TurnDecisionContext>,
            Arc<EmbeddedAgentSource>,
            Option<RouterEmbedding>,
        ),
        AgentDecisionError,
    > {
        let mut continuation = RouterContinuation::new(router, Arc::clone(&source));
        let context = self
            .decide_agent_initial_inner(
                source.as_ref(),
                AgentTurnKind::Repl,
                cancellation,
                None,
                None,
                Some(&mut continuation),
            )
            .await?;
        let (admitted, embedding) = continuation.finish();
        let context = self
            .record_issued_decision(admitted.as_ref(), context, cancellation)
            .await?;
        Ok((context, admitted, embedding))
    }

    /// An independent durable first row makes a forced drop after a read
    /// observable without inventing a terminal or resetting the decision.
    async fn persist_router_read_owned(
        &self,
        decision: &EngineDecision,
        source: &dyn TurnSourceProof,
        deadline: Instant,
        owner: &Arc<crate::runtime::turn_cleanup::CleanupTracker>,
        cancellation: &CancellationToken,
    ) -> anyhow::Result<()> {
        if !(self.generation.effective.causal_tree.enabled && self.durable_key && self.recordable) {
            // Disabled/privacy turns retain their existing no-decision-write
            // contract; they do not claim durable selected-hop observation.
            return Ok(());
        }
        let record = DecisionRecordBuilder::build_router_history_read(
            decision,
            &self.state_key,
            self.generation.effective.causal_tree.store.record_redacted_projection,
        )?;
        let store = Arc::clone(&self.store);
        let workspace = store.workspace_id().context("Router decision store UUID missing")?;
        let expected_digest = record.replay_digest(&workspace)?;
        let scope = DecisionScope::Owner {
            workspace_id: workspace,
            owner_id: Arc::from(source.owner_id()),
        };
        let id = decision.decision_id;
        let source_id = source
            .source_event_id()
            .context("Router source event missing")?
            .to_owned();
        let session = source.session_key().to_owned();
        let run = source.run_id().to_owned();
        let owner = Arc::clone(owner);
        let ticket = owner.register(cancellation.clone())?;
        tokio::spawn(async move {
            let result = async {
                ensure!(
                    Instant::now() < deadline,
                    "Router read recording expired before queue admission"
                );
                let writer = store.writer().context("Router decision writer unavailable")?;
                ensure!(
                    writer.record_chain(vec![record]) == Enqueue::Queued,
                    "Router selected read record was dropped"
                );
                ensure!(
                    writer.drain(deadline).await,
                    "Router selected read record drain is unconfirmed"
                );
                // Drain is only a FIFO acknowledgement; the writer can report
                // an insert failure before it. Read the real owned row back.
                let reader = store.reader();
                let row = crate::runtime::ingress_recovery::owned_store_read(&owner, move || {
                    Ok(reader.replay_record_until(&scope, &id, deadline)?)
                })
                .await?
                .context("Router selected read is not durable")?;
                ensure!(
                    row.digest == expected_digest
                        && row.decision.decision_id == id
                        && row.decision.prior_decision_id.is_none()
                        && row.decision.has_owner
                        && row.decision.enabled_snapshot
                        && !row.decision.legacy_v1
                        && row.source_event_id.as_deref() == Some(source_id.as_str())
                        && row.decision.session_key.as_deref() == Some(session.as_str())
                        && row.decision.run_id.as_deref() == Some(run.as_str()),
                    "Router selected read differs from its durable owned row"
                );
                Ok::<_, anyhow::Error>(())
            }
            .await;
            // A failed/expired write acknowledgement remains unknown; never
            // clear it just because the caller stopped awaiting this job.
            ticket.finish(result.is_ok());
            result
        })
        .await
        .context("Router selected read record owner task failed")?
    }

    /// Returns the actual first hop and an optional next state. A non-Retrieve
    /// selection closes normally and cannot authorize a history read.
    pub(super) async fn router_history_hop(
        &self,
        state: &CausalState,
        deadline: Instant,
        continuation: &mut RouterContinuation<'_>,
        cancellation: &CancellationToken,
        accounting: &accounting::TurnAccounting<'_>,
    ) -> Result<(EngineDecision, Option<CausalState>), AgentDecisionError> {
        self.router_history_hop_inner(state, deadline, continuation, cancellation, accounting)
            .await
            .map_err(AgentDecisionError::Router)
    }

    async fn router_history_hop_inner(
        &self,
        state: &CausalState,
        deadline: Instant,
        continuation: &mut RouterContinuation<'_>,
        cancellation: &CancellationToken,
        accounting: &accounting::TurnAccounting<'_>,
    ) -> anyhow::Result<(EngineDecision, Option<CausalState>)> {
        ensure!(
            matches!(continuation.phase, RoutingPhase::Pending),
            "Router history phase already consumed"
        );
        continuation.phase = RoutingPhase::Reading;
        let original_owner = Arc::clone(&continuation.original);
        let original = &original_owner;
        let proof = original.as_ref();
        ensure!(
            original.needs_router_history(continuation.router)?,
            "Router history is not selected by entry policy"
        );
        self.resolve_agent_model(proof)?;
        self.check_live_parity()?;
        let domain = continuation.router.history_domain(proof)?;
        let domain_sha256: [u8; 32] = hex::decode(domain.key()?)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("Router domain digest has invalid length"))?;
        let query_sha256: [u8; 32] = Sha256::digest(proof.routing_input().as_bytes()).into();
        let binding = RouterHistoryReadBinding {
            domain_sha256,
            query_sha256,
            version: 1,
        };
        let mut routing_state = state.clone();
        routing_state.trusted = state.trusted.clone().with_router_history(binding.clone());
        let catalog = self.catalog_view();
        let config = &self.generation.effective;
        let model = self.resolve_agent_model(proof)?;
        let model_ref = &routing_state.trusted.parts().resolved_model;
        let observed = self.observed_capability_latency(proof.owner_id(), catalog.generation());
        let cost = super::super::turn_cost::RuntimeCostModel::new(&model, &config.cost, &observed);
        let no_refs = DiscoveredRefs::empty();
        let calls = BTreeMap::new();
        let replayed = BTreeSet::new();
        let gates = GateInputs {
            policy: &self.security,
            owner_id: proof.owner_id(),
            sender: proof.policy_sender(),
            policy_channel: proof.policy_channel(),
            chat_type: proof.policy_chat_type(),
            model_ref,
            model_can_chat: true,
            model_can_call_tools: self.model_can_call_tools(proof),
            decision_endpoint_ready: true,
            child_scope: None,
            verified_approval_calls: BTreeMap::new(),
            call_fingerprints: &calls,
            failed_call_counts: BTreeMap::new(),
            replayed_non_idempotent: &replayed,
            retry_limit: None,
        };
        // No lexical-memory or history read occurs during rehearsal. Metadata
        // availability is the only supplied rehearsal port in this phase.
        let pinned = PinnedCatalogRehearsal { catalog: &catalog };
        let rehearsal = RehearsalPorts {
            memory: None,
            catalog: Some(&pinned),
            skill: None,
        };
        let decision_id = DecisionId::new_runtime();
        let inputs = EngineInputs {
            state: &routing_state,
            catalog: &catalog,
            config: &config.causal_tree,
            key: &self.state_key,
            decision_id,
            cost_model: &cost,
            discovered_refs: &no_refs,
            gates: &gates,
            delegation: None,
            delegation_max_depth: 0,
            prediction: self.factory.prediction_port(Some(accounting)),
            rehearsal_ports: &rehearsal,
            request_limits: RequestLimits {
                max_request_bytes: config.causal_tree.backend.max_request_bytes,
                ..RequestLimits::default()
            },
            model: self.factory.request_model().or_else(|| Some(model.model())),
            can_deliver_text: false,
            may_delegate: false,
            delegation_target: None,
            terminal_only: config.causal_tree.policy.max_decision_hops == 0 || Instant::now() >= deadline,
            allow_terminal_answer: false,
            deadline,
            #[cfg(test)]
            inject_builtin_failure: false,
            #[cfg(test)]
            after_prediction_pause: None,
        };
        self.emit_agent_phase(proof, decision_id, CausalEventKind::SnapshotCreated, None);
        let prepared = CausalDecisionEngine::prepare_hop(&inputs)?;
        self.emit_agent_phase(
            proof,
            decision_id,
            CausalEventKind::CandidatesGenerated,
            Some(prepared.candidate_count()),
        );
        let decision = if inputs.terminal_only {
            CausalDecisionEngine::decide_terminal_prepared(prepared)?
        } else {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => anyhow::bail!("Router decision cancelled"),
                decision = CausalDecisionEngine::decide_prepared(prepared) => decision?,
            }
        };
        self.emit_agent_phase(
            proof,
            decision_id,
            CausalEventKind::CandidateGated,
            Some(decision.pre_commit.outcomes.len()),
        );
        let directive_context = DirectiveContext {
            current_generation: catalog.generation(),
            approval_expires_at: chrono::Utc::now(),
            discovery_limit: 1,
        };
        match directive_from_selection(&decision, &directive_context)? {
            TurnExecutionDirective::Retrieve {
                purpose: SelectedReadPurpose::RouterHistory,
                ..
            } => {}
            TurnExecutionDirective::Clarify { .. }
            | TurnExecutionDirective::Refuse { .. }
            | TurnExecutionDirective::RequestApproval { .. } => {
                continuation.phase = RoutingPhase::Closed;
                return Ok((decision, None));
            }
            _ => anyhow::bail!("Router history selected an incompatible purpose"),
        }
        let selected_id = match &decision.selection {
            Selection::Commit { candidate_id, .. } | Selection::Recover { candidate_id, .. } => candidate_id.clone(),
            _ => anyhow::bail!("Router history lacks a committed selection"),
        };
        let reference = decision
            .state
            .derived
            .candidates()
            .iter()
            .find(|candidate| candidate.candidate.candidate_id() == &selected_id)
            .and_then(|candidate| candidate.candidate.required_capabilities().first())
            .cloned()
            .context("Router selected capability is missing")?;
        let source_sequence = original.input_metadata()?.sequence();
        let mut failure_stage = "grant";
        let mut history_snapshot = None;
        #[cfg(test)]
        let diagnostic_started = Instant::now();
        #[cfg(test)]
        let mut diagnostic_deadline = None;
        #[cfg(test)]
        let mut diagnostic = (None, None, None, None, None, None, None);
        let completed = async {
            self.check_live_parity()?;
            let history = continuation.router.history()?;
            let history_deadline = history.query_deadline(deadline)?;
            #[cfg(test)]
            {
                diagnostic_deadline = Some(history_deadline);
                diagnostic.0 = Some(history_diagnostic_point(
                    diagnostic_started,
                    history_deadline,
                    cancellation,
                ));
            }
            let grant = decision.issue_selected_router_read(&catalog, &directive_context, proof.routing_input())?;
            let evidence = RouterHistoryReadEvidence::from_selected(
                grant,
                proof,
                original.input_metadata()?,
                domain.clone(),
                history_deadline,
            )?;
            let owner =
                crate::runtime::turn_cleanup::current_tracker().context("Router history cleanup owner missing")?;
            ensure!(
                !cancellation.is_cancelled() && Instant::now() < history_deadline,
                "Router history read cancelled before dispatch"
            );
            failure_stage = "record";
            self.persist_router_read_owned(&decision, proof, history_deadline, &owner, cancellation)
                .await?;
            #[cfg(test)]
            {
                diagnostic.1 = Some(history_diagnostic_point(
                    diagnostic_started,
                    history_deadline,
                    cancellation,
                ));
            }
            ensure!(
                !cancellation.is_cancelled() && Instant::now() < history_deadline,
                "Router history read cancelled after selected-hop recording"
            );
            self.check_live_parity()?;
            failure_stage = "read";
            let read = read_owned(
                Arc::clone(&self.memory),
                evidence,
                history_deadline,
                &owner,
                cancellation,
            )
            .await?;
            #[cfg(test)]
            {
                diagnostic.2 = Some(history_diagnostic_point(
                    diagnostic_started,
                    history_deadline,
                    cancellation,
                ));
            }
            ensure!(
                !cancellation.is_cancelled() && Instant::now() < history_deadline,
                "Router history cancelled or expired"
            );
            read.validate(&domain)?;
            #[cfg(test)]
            {
                diagnostic.3 = Some(history_diagnostic_point(
                    diagnostic_started,
                    history_deadline,
                    cancellation,
                ));
            }
            ensure!(
                read.source_sequence == source_sequence,
                "Router history source sequence changed"
            );
            history_snapshot = Some(snapshot_digest(&read));
            #[cfg(test)]
            {
                diagnostic.4 = Some(history_diagnostic_point(
                    diagnostic_started,
                    history_deadline,
                    cancellation,
                ));
            }
            failure_stage = "embedding";
            #[cfg(test)]
            {
                diagnostic.5 = Some(history_diagnostic_point(
                    diagnostic_started,
                    history_deadline,
                    cancellation,
                ));
            }
            let vector = if let Some(local) = history.local_embedder_if_present() {
                local
                    .embed_router_until(proof.routing_input(), history_deadline, cancellation)
                    .await?
            } else {
                let configured = crate::memory::configured_paid_embedder(config, &self.provider_catalog)?;
                ensure!(
                    configured.model == domain.embedding_model
                        && configured.dimensions == usize::from(domain.dimensions),
                    "Router paid embedding differs from admitted history domain"
                );
                embedding::embed_selected(
                    configured,
                    accounting.ensure_scope()?,
                    Arc::clone(&self.state_key),
                    domain_sha256,
                    query_sha256,
                    proof.routing_input(),
                    history_deadline,
                    cancellation,
                )
                .await?
            };
            failure_stage = "rank";
            #[cfg(test)]
            {
                diagnostic.6 = Some(history_diagnostic_point(
                    diagnostic_started,
                    history_deadline,
                    cancellation,
                ));
            }
            let scores = history
                .scoped_similarity(&read, &domain, &vector, history_deadline, cancellation)
                .await?;
            let task = crate::agent::classifier::classify_intent(&config.task_routing, proof.routing_input());
            let ranked = continuation.router.rank_scoped_history(proof, task.intent, &scores)?;
            ensure!(
                !cancellation.is_cancelled() && Instant::now() < history_deadline,
                "Router history ranking expired"
            );
            self.check_live_parity()?;
            failure_stage = "pmm";
            let selected = original.prepare_router_selection(&ranked)?;
            let choice = selected.choice();
            let route = crate::providers::selection::route_target(
                &self.provider_catalog,
                choice.provider_name(),
                choice.model_name(),
            )?;
            let resolved = crate::providers::selection::check_with(
                config,
                &self.provider_catalog,
                choice.provider_name(),
                choice.model_name(),
                crate::providers::selection::credential_for(config, choice.provider_name()),
                &Requirement::chat(),
                UnknownPolicy::Invocation,
            )?;
            ensure!(
                route == choice.binding().route() && choice.binding().matches(&resolved),
                "Router PMM readmission differs from pinned model binding"
            );
            let route_bytes = serde_json::to_vec(&(
                choice.provider_name(),
                choice.model_name(),
                route,
                choice.binding().credential_reference(),
            ))?;
            ensure!(route_bytes.len() <= 32 * 1024, "Router PMM binding exceeds capacity");
            let route_binding = self.state_key.hmac(
                KeyDomain::StateFingerprint,
                &[b"router-readmission-route-v1", &route_bytes],
            );
            failure_stage = "admission";
            let token = RouterModelReadmission {
                source: Arc::clone(original),
                selected,
            };
            let admitted = original.with_admitted_router_selection(token)?;
            let model = self.resolve_agent_model(admitted.as_ref())?;
            let mut parts = state.trusted.parts().clone();
            parts.resolved_model = Arc::from(format!("{}:{}", model.provider_label(), model.model()));
            let effective = effective_agent_exclusions(
                config,
                &self.registry,
                &catalog,
                model.model(),
                &self.policy_version,
                &self.state_key,
            );
            parts.exclusions = effective.names;
            parts.model_admission = effective.model_admission;
            let remaining = accounting.remaining_budgets();
            parts.budgets.tokens = parts.budgets.tokens.min(remaining.tokens);
            parts.budgets.micro_usd = parts.budgets.micro_usd.min(remaining.micro_usd);
            parts.budgets.latency_ms = parts.budgets.latency_ms.min(remaining.latency_ms);
            let effective = crate::router::resolve_effective_compaction_config(
                &config.agent.compaction,
                admitted.provider_name(),
                admitted.model_name(),
                &config.router,
                &config.model_routes,
            );
            let reservation = DecisionTokenReservation::from_effective_compaction(&effective.config)?;
            parts.budgets.tokens = parts.budgets.tokens.min(u64::try_from(reservation.decision_tokens)?);
            let main_compaction = reservation.reserved_compaction(&effective.config)?;
            let remaining_budget = parts.budgets;
            let mut next = state.clone();
            next.trusted = TrustedState::from_runtime(parts);
            next.lineage = HopLineage::After {
                hop: NonZeroU8::new(1).context("invalid Router hop")?,
                prior_decision_id: decision_id.as_uuid(),
            };
            next.derived = DerivedState::default();
            next.observed = ObservedState::from_runtime_records(
                vec![ObservedStep::recorded_by_runtime(
                    ObservedStepKind::RouterHistory,
                    ObservedStepStatus::Ok,
                    Some(selected_id.clone()),
                    None,
                )],
                Vec::new(),
                0,
            )
            .with_exact_call_observations(next.observed.exact_call_observations().cloned());
            next.built_at_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0);
            let transition = Arc::new(RouterTransition {
                read_id: decision_id,
                selected_id: selected_id.clone(),
                reference: reference.clone(),
                binding: binding.clone(),
                history_snapshot,
                failure_stage: None,
                source_sequence: read.source_sequence,
                original: Arc::clone(original),
                admitted: Arc::clone(&admitted),
                route_binding: Some(route_binding),
                original_budget: state.trusted.parts().budgets,
                remaining_budget,
                deadline,
            });
            let embedding = RouterEmbedding {
                source_event_id: proof.source_event_id().context("Router source missing")?.to_owned(),
                query_sha256,
                domain,
                vector,
            };
            continuation.phase = RoutingPhase::Consumed {
                source: admitted,
                transition,
                embedding: Some(embedding),
                main_compaction,
            };
            Ok::<_, anyhow::Error>(next)
        }
        .await;
        #[cfg(test)]
        if completed.is_err() {
            let final_point = diagnostic_deadline
                .map(|local_deadline| history_diagnostic_point(diagnostic_started, local_deadline, cancellation));
            use std::io::Write as _;
            // Diagnostic I/O must not replace the original failed turn.
            let _ = writeln!(
                std::io::stderr(),
                "PRX_ROUTER_HISTORY_DIAGNOSTIC stage={failure_stage} units=us fields=(elapsed,cancelled,expired) order=(deadline,record,read,validate,digest,pre_embed,post_embed) points={diagnostic:?} final={final_point:?}"
            );
        }
        let next = match completed {
            Ok(next) => next,
            Err(_) => {
                // The selected read and any completed local work really
                // happened. Preserve its hop and close without retrying it.
                let mut parts = state.trusted.parts().clone();
                parts.budgets.latency_ms =
                    u64::try_from(deadline.saturating_duration_since(Instant::now()).as_millis()).unwrap_or(0);
                let remaining_budget = parts.budgets;
                let mut next = state.clone();
                next.trusted = TrustedState::from_runtime(parts);
                next.lineage = HopLineage::After {
                    hop: NonZeroU8::new(1).context("invalid Router failed hop")?,
                    prior_decision_id: decision_id.as_uuid(),
                };
                next.derived = DerivedState::default();
                next.observed = ObservedState::from_runtime_records(
                    vec![ObservedStep::recorded_by_runtime(
                        ObservedStepKind::RouterHistory,
                        if cancellation.is_cancelled() {
                            ObservedStepStatus::Cancelled
                        } else {
                            ObservedStepStatus::Failed
                        },
                        Some(selected_id.clone()),
                        None,
                    )],
                    Vec::new(),
                    0,
                )
                .with_exact_call_observations(next.observed.exact_call_observations().cloned());
                let transition = Arc::new(RouterTransition {
                    read_id: decision_id,
                    selected_id,
                    reference,
                    binding,
                    history_snapshot,
                    failure_stage: Some(failure_stage),
                    source_sequence,
                    original: Arc::clone(&continuation.original),
                    admitted: Arc::clone(&continuation.original),
                    route_binding: None,
                    original_budget: state.trusted.parts().budgets,
                    remaining_budget,
                    deadline,
                });
                continuation.phase = RoutingPhase::Failed { transition };
                next
            }
        };
        Ok((decision, Some(next)))
    }
}

// Fixed-size test diagnostics only; no payload, identities, heap log or timed I/O.
#[cfg(test)]
fn history_diagnostic_point(
    started: Instant,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> (u128, bool, bool) {
    let now = Instant::now();
    (
        now.saturating_duration_since(started).as_micros(),
        cancellation.is_cancelled(),
        now >= deadline,
    )
}

fn snapshot_digest(read: &RouterHistoryRead) -> [u8; 32] {
    // Use the existing backend with runtime CPU dispatch; keep the v1 stream.
    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    digest.update(b"router-history-snapshot-v1");
    digest.update(&read.source_sequence.to_be_bytes());
    for row in &read.rows {
        digest.update(&row.source_sequence.to_be_bytes());
        digest.update(&row.terminal_sequence.to_be_bytes());
        for text in [&row.terminal_event_id, &row.provider, &row.model] {
            digest.update(&(text.len() as u64).to_be_bytes());
            digest.update(text.as_bytes());
        }
        digest.update(&[u8::from(row.success)]);
        // Preserve the v1 byte stream while amortizing SHA update overhead.
        let mut bytes = [0_u8; 64];
        let (chunks, remainder) = row.embedding.as_chunks::<16>();
        for values in chunks {
            for (slot, value) in bytes.as_mut_slice().as_chunks_mut::<4>().0.iter_mut().zip(values) {
                *slot = value.to_le_bytes();
            }
            digest.update(&bytes);
        }
        for value in remainder {
            digest.update(&value.to_le_bytes());
        }
    }
    let result = digest.finish();
    let mut bytes = [0_u8; 32];
    bytes.copy_from_slice(result.as_ref());
    bytes
}

impl RouterTransition {
    pub(super) fn replay_facts(
        &self,
        context: &TurnDecisionContext,
        key: &DecisionKey,
    ) -> anyhow::Result<RouterReadFacts> {
        let mut hops = context.audit_hops();
        let read = hops.next().context("Router transition has no read hop")?;
        let next = hops.next().context("Router transition has no readmission hop")?;
        let original = self.original.as_ref();
        let admitted = self.admitted.as_ref();
        ensure!(
            self.original.envelope() == self.admitted.envelope()
                && original.source_event_id() == admitted.source_event_id()
                && original.routing_input() == admitted.routing_input()
                && original.input_metadata()?.verified_raw_for(original)?
                    == admitted.input_metadata()?.verified_raw_for(admitted)?,
            "Router transition changed its immutable source"
        );
        ensure!(read.decision_id == self.read_id, "Router read decision changed");
        ensure!(
            read.deadline == self.deadline
                && next.deadline == self.deadline
                && read.state.lineage == HopLineage::First
                && next.state.lineage.hop() == 1
                && next.state.lineage.prior_decision_id() == Some(read.decision_id.as_uuid())
                && read.state.trusted.router_history() == Some(&self.binding)
                && next.state.trusted.router_history().is_none(),
            "Router transition lineage or purpose changed"
        );
        for hop in [read, next] {
            let trusted = hop.state.trusted.parts();
            ensure!(
                trusted.workspace_id.as_ref() == original.workspace_id()
                    && trusted.owner_id.as_deref() == Some(original.owner_id())
                    && trusted.session_key.as_deref() == Some(original.session_key())
                    && trusted.run_id.as_deref() == Some(original.run_id())
                    && trusted.source_event_id.as_deref() == original.source_event_id()
                    && hop.state.routing_input.as_str() == original.routing_input(),
                "Router transition crosses source scope"
            );
        }
        let selected = match &read.selection {
            Selection::Commit { candidate_id, .. } | Selection::Recover { candidate_id, .. } => candidate_id,
            _ => anyhow::bail!("Router read was not selected"),
        };
        ensure!(
            selected == &self.selected_id
                && matches!(
                    read.pre_commit.outcomes.get(selected),
                    Some(crate::causal_tree::gates::GateOutcome::Passed)
                ),
            "Router read final gate is absent"
        );
        let candidate = read
            .state
            .derived
            .candidates()
            .iter()
            .find(|candidate| candidate.candidate.candidate_id() == selected)
            .map(|candidate| &candidate.candidate)
            .context("Router read candidate missing")?;
        ensure!(
            candidate.action() == CausalActionKind::Retrieve
                && candidate.target() == CapabilityTarget::Memory
                && candidate.required_capabilities() == std::slice::from_ref(&self.reference)
                && self.reference.generation == read.state.trusted.parts().catalog_generation
                && next.state.trusted.parts().catalog_generation == self.reference.generation,
            "Router read capability binding changed"
        );
        ensure!(
            match self.failure_stage {
                Some(_) => self.route_binding.is_none(),
                None => self.route_binding.is_some() && self.history_snapshot.is_some(),
            },
            "Router transition outcome facts are incomplete"
        );
        if self.failure_stage.is_some() {
            ensure!(
                Arc::ptr_eq(&self.original, &self.admitted) && context.router_cleanup_close(),
                "failed Router phase did not preserve fixed close"
            );
        }
        let original_budget = budgets(self.original_budget);
        let remaining_budget = budgets(self.remaining_budget);
        ensure!(
            budgets(read.state.trusted.parts().budgets) == original_budget
                && budgets(next.state.trusted.parts().budgets)
                    .iter()
                    .zip(remaining_budget)
                    .all(|(next, retained)| *next <= retained)
                && remaining_budget
                    .iter()
                    .zip(original_budget)
                    .all(|(remaining, original)| *remaining <= original),
            "Router transition increases or replaces turn budgets"
        );
        let (from_provider, from_model) = original.wire_selection();
        let (to_provider, to_model) = admitted.wire_selection();
        ensure!(
            read.state.trusted.parts().resolved_model.as_ref() == format!("{from_provider}:{from_model}")
                && next.state.trusted.parts().resolved_model.as_ref() == format!("{to_provider}:{to_model}")
                && from_model.len() <= 512
                && to_model.len() <= 512,
            "Router transition model binding differs from actual hops"
        );
        ensure!(
            self.source_sequence > 0
                && self.reference.id.as_str().len() <= 512
                && self.selected_id.as_str().len() <= 512,
            "Router replay facts exceed capacity"
        );
        Ok(RouterReadFacts {
            version: 1,
            readmission_succeeded: self.failure_stage.is_none(),
            failure_stage: self.failure_stage,
            read_decision_id: read.decision_id,
            read_state_fingerprint: read.state.material_fingerprint(key),
            selected_candidate_id: self.selected_id.as_str().to_owned(),
            capability_id: self.reference.id.as_str().to_owned(),
            capability_fingerprint: self.reference.fingerprint.0,
            catalog_generation: self.reference.generation.0,
            domain_sha256: self.binding.domain_sha256,
            query_sha256: self.binding.query_sha256,
            history_snapshot_sha256: self.history_snapshot,
            next_decision_id: next.decision_id,
            next_state_fingerprint: next.state.material_fingerprint(key),
            from_provider_binding: provider_binding(key, from_provider),
            from_model: from_model.to_owned(),
            to_provider_binding: provider_binding(key, to_provider),
            to_model: to_model.to_owned(),
            pmm_route_binding: self.route_binding,
            original_budget,
            remaining_budget: budgets(next.state.trusted.parts().budgets),
            source_sequence: self.source_sequence,
            _issued: (),
        })
    }
}
const fn budgets(value: TurnBudgets) -> [u64; 3] {
    [value.tokens, value.latency_ms, value.micro_usd]
}

#[cfg(test)]
mod tests;
