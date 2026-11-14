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
use crate::security::runtime_key::{DecisionKey, KeyDomain};
use crate::tools::capability::catalog::CatalogView;
use crate::tools::capability::child::ChildCapabilityManifest;
use crate::tools::capability::turn_state::{DeliveryConstraints, TrustedParts, TurnBudgets};
use crate::tools::surface_owner::facts::PublicationFacts;
use crate::tools::traits::Tool;

pub(super) struct EffectiveAgentExclusions {
    pub(super) names: Vec<Arc<str>>,
    pub(super) model_admission:
        Option<Arc<dyn crate::tools::capability::turn_state::model_admission::ModelAdmissionView>>,
}

pub(super) fn effective_agent_exclusions(
    config: &crate::config::Config,
    registry: &[Box<dyn Tool>],
    catalog: &CatalogView<'_>,
    model_name: &str,
    policy_version: &str,
    key: &DecisionKey,
) -> EffectiveAgentExclusions {
    let names: BTreeSet<Arc<str>> = config
        .tool_tiering
        .always_exclude
        .iter()
        .map(|name| Arc::from(name.as_str()))
        .collect();
    let model_admission = config.tool_tiering.model_allowlists.get(model_name).map(|allowlist| {
        crate::tools::capability::model_policy::ModelAdmission::fresh(
            registry,
            catalog,
            model_name,
            allowlist,
            policy_version,
            key,
        ) as Arc<dyn crate::tools::capability::turn_state::model_admission::ModelAdmissionView>
    });
    EffectiveAgentExclusions {
        names: names.into_iter().collect(),
        model_admission,
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
        let effective_exclusions = effective_agent_exclusions(
            &self.generation.effective,
            &self.registry,
            &catalog,
            model.model(),
            &self.policy_version,
            &self.state_key,
        );
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
            side_effect_mode: match self.security.autonomy {
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
            model_admission: effective_exclusions.model_admission,
            constraints: {
                let mut constraints = Vec::new();
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
        let facts = PublicationFacts {
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
            child_discovery: proof.child_scope().is_some()
                && self.execution.current_descriptor("capability_search").is_some(),
            child_served: (proof.child_scope().is_some()
                && self.security.autonomy == crate::security::AutonomyLevel::Full)
                .then(|| Arc::new(self.child_served_entries(&catalog))),
        };
        // The published surface is an execution rule over the pinned scope,
        // the publication facts and the catalog (R28).
        let published = crate::tools::surface_owner::publication::PublicationRules {
            trusted: &trusted,
            facts: &facts,
            catalog: &catalog,
        }
        .surface(outgoing_delegation.is_some(), &|bytes| {
            crate::cost::estimate::estimate_cost(&model, &self.generation.effective.cost, bytes)
        });
        // The turn's one accounting scope, created at opening. Without the
        // prediction hop nothing has been charged to it yet.
        let accounting = accounting_scope(self, proof, budgets, deadline);
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
            deadline,
        }))
    }

    pub(crate) async fn open_issued_turn_context(
        &self,
        proof: &dyn TurnSourceProof,
        kind: AgentTurnKind,
        cancellation: &CancellationToken,
    ) -> Result<Arc<OpenedTurn>, AgentDecisionError> {
        let opened = self.open_turn_context(proof, kind, cancellation).await?;
        self.admit_opened_context(proof, opened, cancellation).await
    }

    pub(in crate::runtime) async fn open_issued_segment_turn_context(
        &self,
        proof: &dyn TurnSourceProof,
        observation: Option<&crate::runtime::child_turn::segment_observation::SegmentObservation>,
        cancellation: &CancellationToken,
    ) -> Result<Arc<OpenedTurn>, AgentDecisionError> {
        let opened = self.open_segment_turn_context(proof, observation, cancellation).await?;
        self.admit_opened_context(proof, opened, cancellation).await
    }

    /// Admit an opened turn: the run allowance, the cancellation checks and
    /// the delegator readiness check.
    pub(crate) async fn admit_opened_context(
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
/// `None` when the turn cannot be charged: an enabled plane without durable
/// recording, an unverifiable origin, or no cleanup owner.
fn accounting_scope(
    service: &TurnRuntimeService,
    proof: &dyn TurnSourceProof,
    budgets: TurnBudgets,
    deadline: Instant,
) -> Option<Arc<AccountingScope>> {
    if service.generation.effective.causal_tree.enabled && (!service.durable_key || !service.recordable) {
        return None;
    }
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
