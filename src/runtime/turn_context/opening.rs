//! The turn opening: the only constructor of the trusted turn state
//! (`TrustedParts`), the per-turn [`OpenedTurn`] and its accounting scope.
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use anyhow::Context;
use tokio_util::sync::CancellationToken;

use super::{
    AgentDecisionError, AgentTurnKind, OpenedTurn, OutgoingDelegation, TurnRuntimeContext, validate_agent_turn_origin,
};
use crate::decision_engine::state::RawRoutingInput;
use crate::runtime::child_turn::segment_observation;
use crate::runtime::decision_accounting::AccountingScope;
use crate::runtime::turn_publication::TurnRuntimeService;
use crate::runtime::turn_source::TurnSourceProof;
use crate::security::decision_id::DecisionId;
use crate::security::runtime_key::KeyDomain;
use crate::tools::capability::child::ChildCapabilityManifest;
use crate::tools::capability::turn_state::{DeliveryConstraints, TrustedParts, TurnBudgets};
use crate::tools::surface_owner::facts::PublicationFacts;

pub(super) struct EffectiveAgentExclusions {
    pub(super) names: Vec<Arc<str>>,
}

impl EffectiveAgentExclusions {
    pub(super) fn for_source(mut self, proof: &dyn TurnSourceProof) -> Self {
        // Every child consumes the root's sole child depth. Operator children
        // carry separate authority rather than an ordinary child manifest.
        if proof
            .child_scope()
            .is_some_and(|scope| scope.gate_inputs().0.authority.remaining_depth == 0)
            || proof.operator_task_authority_id().is_some()
            || proof.operator_process_authority_id().is_some()
        {
            self.names.extend([Arc::from("sessions_spawn"), Arc::from("delegate")]);
        }
        self
    }
}

/// The level a turn runs at: a child turn's sealed scope level (a named
/// agent's own level may differ from the hosting service's), else the
/// service's policy level.
pub(super) fn turn_level(
    security: &crate::security::SecurityPolicy,
    proof: &dyn TurnSourceProof,
) -> crate::security::AutonomyLevel {
    proof
        .child_scope()
        .map_or(security.autonomy, |scope| scope.gate_inputs().0.authority.level)
}

pub(super) fn effective_agent_exclusions(
    level: crate::security::AutonomyLevel,
    config: &crate::config::Config,
) -> EffectiveAgentExclusions {
    let names: BTreeSet<Arc<str>> = config
        .tool_tiering
        .at_level(level, config.autonomy.evaluation_sandbox)
        .always_exclude
        .iter()
        .map(|name| Arc::from(name.as_str()))
        .collect();
    EffectiveAgentExclusions {
        names: names.into_iter().collect(),
    }
}

impl TurnRuntimeService {
    /// Build the turn context, G6 slice and SendLimits without a prediction
    /// port or a directive from selection. Engine-on advice is projected
    /// separately through `DecisionAdvisory`. The turn engine stamps the
    /// call budget from this context.
    pub(crate) async fn open_turn_context(
        &self,
        proof: &dyn TurnSourceProof,
        kind: AgentTurnKind,
        cancellation: &CancellationToken,
    ) -> Result<Arc<OpenedTurn>, AgentDecisionError> {
        self.open_turn_context_observed(proof, kind, cancellation, None).await
    }

    /// Segment admission uses the same builder as a first turn, with the
    /// previous segment's verified observation folded into the trusted state.
    pub(in crate::runtime) async fn open_segment_turn_context(
        &self,
        proof: &dyn TurnSourceProof,
        observation: Option<&segment_observation::SegmentObservation>,
        cancellation: &CancellationToken,
    ) -> Result<Arc<OpenedTurn>, AgentDecisionError> {
        self.open_turn_context_observed(proof, AgentTurnKind::SingleShot, cancellation, observation)
            .await
    }

    pub(in crate::runtime) async fn open_turn_context_observed(
        &self,
        proof: &dyn TurnSourceProof,
        kind: AgentTurnKind,
        cancellation: &CancellationToken,
        observation: Option<&segment_observation::SegmentObservation>,
    ) -> Result<Arc<OpenedTurn>, AgentDecisionError> {
        if cancellation.is_cancelled() {
            return Err(AgentDecisionError::Cancelled);
        }
        let model = self.resolve_agent_model(proof)?;
        crate::runtime::ingress_recovery::service_ready(self)
            .await
            .map_err(AgentDecisionError::IngressRecovery)?;
        let budget = self.agent_budget_snapshot(proof)?;
        let limits = self.send_limits(proof)?;
        let deadline = crate::runtime::logical_clock::now()
            .checked_add(Duration::from_millis(budget.budgets.latency_ms))
            .ok_or(AgentDecisionError::Deadline)?;
        let owner_filesystem = self
            .console_filesystem(proof)
            .await
            .map_err(AgentDecisionError::PendingApproval)?;
        let catalog = self.catalog_view();
        let model_ref: Arc<str> = Arc::from(format!("{}:{}", model.provider_label(), model.model()));
        let effective_exclusions =
            effective_agent_exclusions(turn_level(&self.security, proof), &self.generation.effective).for_source(proof);
        let g6_slice = budget.budgets.tokens;
        let trusted = TrustedParts {
            workspace_id: Arc::from(proof.workspace_id()),
            owner_id: Some(Arc::from(proof.owner_id())),
            session_key: Some(Arc::from(proof.session_key())),
            run_id: Some(Arc::from(proof.run_id())),
            task_id: proof.task_id().map(Arc::from),
            channel: None,
            source_event_id: proof.source_event_id().map(Arc::from),
            causation_event_id: None,
            surface: proof.surface(),
            // A child turn runs at its sealed scope level (a named agent's
            // own level may differ from the hosting service's).
            side_effect_mode: match turn_level(&self.security, proof) {
                crate::security::AutonomyLevel::ReadOnly => {
                    crate::tools::capability::turn_state::SideEffectMode::ReadOnly
                }
                crate::security::AutonomyLevel::Supervised => {
                    crate::tools::capability::turn_state::SideEffectMode::ApprovalRequired
                }
                crate::security::AutonomyLevel::Full => {
                    crate::tools::capability::turn_state::SideEffectMode::GuardedWrite
                }
            },
            exclusions: effective_exclusions.names,
            constraints: {
                let mut constraints = Vec::new();
                if turn_level(&self.security, proof) == crate::security::AutonomyLevel::Full
                    && proof.surface() == crate::tools::capability::turn_state::TurnSurfaceKind::Server
                    && proof.policy_channel() == "console"
                    && proof.h2_entry_origin().is_none()
                    && matches!(
                        proof
                            .memory_scope()
                            .agent_partition
                            .as_ref()
                            .map(|partition| &partition.key),
                        Some(crate::memory::PartitionKey::Named { .. })
                    )
                {
                    constraints.push(crate::tools::capability::turn_state::ConstraintCode::new(
                        "full_targeted_agent",
                    ));
                }
                if proof.owner_isolated() {
                    constraints.push(crate::tools::capability::turn_state::ConstraintCode::new(
                        "owner_isolated",
                    ));
                }
                if proof.intercepts_writes() {
                    constraints.push(crate::tools::capability::turn_state::ConstraintCode::new(
                        "intercepts_writes",
                    ));
                }
                if owner_filesystem.is_some() {
                    constraints.push(crate::tools::capability::turn_state::ConstraintCode::new(
                        "owner_private_filesystem_v1",
                    ));
                }
                constraints
            },
            approval_pending: false,
            approval_grants: Vec::new(),
            budgets: TurnBudgets {
                tokens: g6_slice,
                latency_ms: budget.budgets.latency_ms,
                micro_usd: limits.remaining_micro_usd,
            },
            delivery: DeliveryConstraints {
                interactive: kind == AgentTurnKind::Repl || proof.approval_review_adapter().is_some(),
                can_ask_question: kind == AgentTurnKind::Repl || proof.approval_review_adapter().is_some(),
                approval_channel: proof.approval_review_adapter().is_some(),
            },
            catalog_generation: catalog.generation(),
            policy_version: Arc::clone(&self.policy_version),
            resolved_model: Arc::clone(&model_ref),
            resolved_provider: Arc::from(model.provider_label()),
            resolved_model_id: Arc::from(model.model()),
        };
        let projected_input = crate::multimodal::project_image_markers_for_routing(proof.routing_input()).into_owned();
        let goal = RawRoutingInput::new(projected_input.clone());
        let routing_input = RawRoutingInput::new(projected_input);
        let decision_id = DecisionId::new_runtime();
        let budgets = trusted.budgets;
        let observed = segment_observation::initial_observations(observation, deadline, cancellation)?;
        let outgoing_delegation = match self.outgoing_delegation(proof, &trusted, &routing_input, decision_id, &catalog)
        {
            Some(issued) => Some(OutgoingDelegation {
                manifest: ChildCapabilityManifest::derive_strict_subset(&issued.parent, &issued.requested)?,
                parent_allowed: issued.parent.allowed,
            }),
            None => None,
        };
        let mut facts = PublicationFacts {
            policy: Arc::clone(&self.security),
            owner_id: Arc::from(proof.owner_id()),
            sender: Arc::from(proof.policy_sender()),
            policy_channel: Arc::from(proof.policy_channel()),
            chat_type: Arc::from(proof.policy_chat_type()),
            model_ref: Arc::clone(&model_ref),
            model_can_chat: true,
            model_can_call_tools: self.model_can_call_tools(proof),
            child_scope: proof.child_scope().map(|scope| {
                let (manifest, constraints) = scope.gate_inputs();
                (Arc::new(manifest.clone()), Arc::new(constraints.clone()))
            }),
            // Discovery needs the search tool on this child's own surface: a
            // child whose allowance or exclusions hide it cannot search, so
            // its manifest members are published directly.
            child_discovery: proof.child_scope().is_some()
                && self.execution.current_descriptor("capability_search").is_some()
                && catalog
                    .iter_entries()
                    .any(|entry| entry.public_name == "capability_search"),
            child_served: proof
                .child_scope()
                .is_some_and(|scope| scope.gate_inputs().0.authority.level == crate::security::AutonomyLevel::Full)
                .then(|| Arc::new(self.child_served_entries(&catalog))),
            fit_defers: false,
        };
        // The published surface is an execution rule over the pinned scope,
        // the publication facts and the catalog (R28).
        let publish = |facts: &PublicationFacts| {
            crate::tools::surface_owner::publication::PublicationRules {
                trusted: &trusted,
                facts,
                catalog: &catalog,
            }
            .surface(outgoing_delegation.is_some())
        };
        let mut published = publish(&facts);
        // Fit the tool definitions to the model's window (deterministic for
        // the window and the tool set). A deferring turn is republished with
        // only the tools the surface policy names.
        let fit = self.fit_tool_surface(
            proof.session_key(),
            budget.main_compaction.max_context_tokens,
            &published,
        );
        let tool_surface_tokens = if fit.defers {
            facts.fit_defers = true;
            published = publish(&facts);
            self.published_surface_tokens(&published, true)
        } else {
            fit.full_tokens
        };
        // The turn's one accounting scope, created at opening. Without the
        // prediction hop nothing has been charged to it yet.
        let accounting = accounting_scope(self, proof, budgets, deadline);
        let call = Arc::new(self.call_context(
            proof,
            &model,
            budget.main_compaction.max_context_tokens,
            facts.model_can_call_tools,
            catalog.generation(),
        ));
        let runtime = Arc::new(TurnRuntimeContext {
            decision_id,
            trusted,
            routing_input,
            goal,
            has_conversation_history: proof.has_conversation_history(),
            observed,
            budgets,
            accounting,
            send_limits: limits,
            catalog: Arc::clone(&self.catalog),
            publication_policy: self.publication_policy.clone(),
            catalog_generation: catalog.generation(),
            call,
            admission: facts,
            published,
            discovery: crate::tools::capability::search_tool::DiscoveryState::default(),
            outgoing_delegation,
            state_key: Arc::clone(&self.state_key),
            holder: Arc::clone(&self.runtime),
            model_catalog: self.provider_catalog.release_pin(),
            main_parameters: std::sync::OnceLock::new(),
            recursive_allowance: self.runtime.recursive_allowances().retain(proof),
            execution_trace: std::sync::OnceLock::new(),
            execution_closed: Arc::new(AtomicBool::new(false)),
        });
        Ok(Arc::new(OpenedTurn {
            runtime,
            owner_filesystem,
            source_actor: (Arc::from(proof.policy_channel()), Arc::from(proof.policy_sender())),
            service_id: self.service_id,
            main_compaction: budget.main_compaction,
            tool_surface_tokens,
            deadline,
            registry_drift_noted: AtomicBool::new(false),
        }))
    }

    /// The turn's call context: identity, level, place and model, computed
    /// once here from the same inputs the opening already used.
    fn call_context(
        &self,
        proof: &dyn TurnSourceProof,
        model: &crate::providers::catalog::ResolvedModel,
        window_tokens: usize,
        can_call_tools: bool,
        catalog_generation: crate::tools::capability::contract::CatalogGeneration,
    ) -> crate::context::call::TurnCallContext {
        use crate::context::call::{
            CallAuthority, CallIdentity, CallModel, CallPlace, LevelReason, TurnCallContext,
        };
        let memory_partition = proof
            .memory_scope()
            .agent_partition
            .as_ref()
            .map(|partition| partition.key.clone());
        let agent_id = match &memory_partition {
            Some(crate::memory::PartitionKey::Named { name, .. }) => Some(Arc::from(name.as_str())),
            Some(crate::memory::PartitionKey::Main) | None => None,
        };
        let reason = if proof.child_scope().is_some() {
            LevelReason::ChildScope
        } else {
            LevelReason::ServicePolicy
        };
        let workspace_root = self.security.workspace_dir.clone();
        let working_dir = crate::security::path_policy::effective_root(
            self.generation.effective.runtime.workdir.as_deref(),
            &self.security,
        )
        .to_path_buf();
        TurnCallContext {
            identity: CallIdentity {
                owner_id: Arc::from(proof.owner_id()),
                session_key: Arc::from(proof.session_key()),
                run_id: Arc::from(proof.run_id()),
                agent_id,
                surface: proof.surface(),
                channel: Arc::from(proof.policy_channel()),
                task_id: proof.task_id().map(Arc::from),
            },
            authority: CallAuthority {
                level: turn_level(&self.security, proof),
                reason,
            },
            place: CallPlace {
                workspace_root,
                working_dir,
            },
            model: CallModel {
                provider: Arc::from(model.provider_label()),
                model: Arc::from(model.model()),
                window_tokens,
                can_call_tools,
            },
            catalog_generation,
            memory_partition,
        }
    }

    pub(crate) async fn open_issued_turn_context(
        &self,
        proof: &dyn TurnSourceProof,
        kind: AgentTurnKind,
        cancellation: &CancellationToken,
    ) -> Result<Arc<OpenedTurn>, AgentDecisionError> {
        let opened = self.open_turn_context(proof, kind, cancellation).await?;
        self.admit_opened_context(proof, opened, cancellation)
    }

    pub(in crate::runtime) async fn open_issued_segment_turn_context(
        &self,
        proof: &dyn TurnSourceProof,
        observation: Option<&crate::runtime::child_turn::segment_observation::SegmentObservation>,
        cancellation: &CancellationToken,
    ) -> Result<Arc<OpenedTurn>, AgentDecisionError> {
        let opened = self.open_segment_turn_context(proof, observation, cancellation).await?;
        self.admit_opened_context(proof, opened, cancellation)
    }

    /// Admit an opened turn: the run allowance, the cancellation checks and
    /// the delegator readiness check.
    pub(crate) fn admit_opened_context(
        &self,
        proof: &dyn TurnSourceProof,
        context: Arc<OpenedTurn>,
        cancellation: &CancellationToken,
    ) -> Result<Arc<OpenedTurn>, AgentDecisionError> {
        // Admission, including a pause before any child publication, owns the
        // original run allowance. The context lease cleans up failed issuance.
        context.runtime().issue_recursive_allowance(proof);
        if cancellation.is_cancelled() {
            return Err(AgentDecisionError::Cancelled);
        }
        // A published delegator makes delegation storage reachable (R28).
        let admits_delegation = context.execution_refs().iter().any(|reference| {
            self.catalog_view().entry_any(&reference.id).is_some_and(|entry| {
                entry.fingerprint == reference.fingerprint
                    && entry.generation == reference.generation
                    && crate::tools::surface_owner::publication::is_delegator(&entry.public_name)
            })
        });
        if admits_delegation {
            self.loop_runtime
                .as_ref()
                .context("ordinary delegation runtime missing")
                .map_err(AgentDecisionError::DelegationScope)?;
            self.verified_delegation_scope(&context)
                .map_err(AgentDecisionError::DelegationScope)?;
            if cancellation.is_cancelled() {
                return Err(AgentDecisionError::Cancelled);
            }
        }

        Ok(context)
    }
}

/// The turn's one accounting scope, created at opening (R9). The scope is
/// bound to the source run exactly as the retired prediction hop bound it.
/// `None` when the turn cannot be charged: an unverifiable origin or no
/// cleanup owner.
fn accounting_scope(
    service: &TurnRuntimeService,
    proof: &dyn TurnSourceProof,
    budgets: TurnBudgets,
    deadline: Instant,
) -> Option<Arc<AccountingScope>> {
    validate_agent_turn_origin(&service.generation, service.surface, proof).ok()?;
    let cleanup = crate::runtime::turn_cleanup::current_tracker()?;
    let key = &service.state_key;
    Some(Arc::new(
        AccountingScope::new(
            service.generation.effective.clone(),
            key.clone(),
            cleanup,
            accounting_scope_id(key, proof),
            budgets,
            deadline,
        )
        .with_root_agent(proof.memory_scope().agent_partition.as_ref())
        .with_cost_root_agent(proof.cost_root_agent())
        .with_money(service.runtime.recursive_money(proof)),
    ))
}

fn accounting_scope_id(key: &crate::security::runtime_key::DecisionKey, proof: &dyn TurnSourceProof) -> [u8; 32] {
    key.hmac(
        KeyDomain::TerminalClaim,
        &[
            b"turn-decision-accounting-v1",
            proof.workspace_id().as_bytes(),
            proof.owner_id().as_bytes(),
            proof.session_key().as_bytes(),
            proof.run_id().as_bytes(),
            proof.source_event_id().unwrap_or("").as_bytes(),
            proof.config_source_revision().as_bytes(),
        ],
    )
}
