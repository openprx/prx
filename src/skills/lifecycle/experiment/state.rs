//! Replayable stage accounting. These records never confer execution authority.
use super::{ExperimentError, ExperimentResources, FrozenExperiment};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExperimentEvent {
    Reserved {
        stage: String,
    },
    Dispatched {
        stage: String,
    },
    DispatchUnknown {
        stage: String,
    },
    Settled {
        stage: String,
        usage: ExperimentResources,
        evidence_sha256: String,
        reference: Option<String>,
        successful: bool,
    },
    CleanupConfirmed {
        stage: String,
    },
    Cancelled,
}
impl ExperimentEvent {
    pub(super) fn stage(&self) -> Option<&str> {
        match self {
            Self::Reserved { stage }
            | Self::Dispatched { stage }
            | Self::DispatchUnknown { stage }
            | Self::Settled { stage, .. }
            | Self::CleanupConfirmed { stage } => Some(stage),
            Self::Cancelled => None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum StageState {
    #[default]
    Planned,
    Reserved,
    Dispatched,
    DispatchUnknown,
    Settled {
        usage: ExperimentResources,
        evidence_sha256: String,
        successful: bool,
        cleanup_confirmed: bool,
    },
}
impl StageState {
    pub const fn successful(&self) -> bool {
        matches!(
            self,
            Self::Settled {
                successful: true,
                cleanup_confirmed: true,
                ..
            }
        )
    }
    const fn active(&self) -> bool {
        !matches!(
            self,
            Self::Planned
                | Self::Settled {
                    cleanup_confirmed: true,
                    ..
                }
        )
    }
}
#[derive(Clone, Debug, Default)]
pub struct ExperimentState {
    pub stages: BTreeMap<String, StageState>,
    pub cancelled: bool,
}
impl ExperimentState {
    /// Processing completion alone is not semantic correctness. Only a bound
    /// independently verified stage can qualify the complete experiment.
    pub fn qualified(&self, plan: &FrozenExperiment) -> bool {
        !self.cancelled
            && self
                .committed(plan)
                .is_ok_and(|value| value.within(plan.spec().resources))
            && self.stages.values().all(StageState::successful)
            && plan.spec().stages.iter().any(|stage| {
                stage.kind == super::ExperimentStageKind::IndependentVerification
                    && self.stages.get(&stage.id).is_some_and(StageState::successful)
            })
    }
    pub fn new(plan: &FrozenExperiment) -> Self {
        Self {
            stages: plan
                .spec()
                .stages
                .iter()
                .map(|stage| (stage.id.clone(), StageState::Planned))
                .collect(),
            cancelled: false,
        }
    }
    /// Consumed and unresolved liabilities are disjoint. A settled stage whose
    /// cleanup is unconfirmed retains the unused part of its reservation.
    pub fn committed(&self, plan: &FrozenExperiment) -> Result<ExperimentResources, ExperimentError> {
        plan.spec()
            .stages
            .iter()
            .try_fold(ExperimentResources::default(), |sum, stage| {
                let state = self.stages.get(&stage.id).ok_or_else(|| invalid("missing stage"))?;
                let amount = match state {
                    StageState::Planned => ExperimentResources::default(),
                    StageState::Settled {
                        usage,
                        cleanup_confirmed: true,
                        ..
                    } => *usage,
                    StageState::Settled { usage, .. } => stage.allocation.component_max(*usage),
                    _ => stage.allocation,
                };
                sum.checked_add(amount)
            })
    }
    pub fn apply(
        &mut self,
        plan: &FrozenExperiment,
        event: &ExperimentEvent,
        now: DateTime<Utc>,
    ) -> Result<(), ExperimentError> {
        // Apply into a copy so failed validation cannot partially mutate state.
        let mut next = self.clone();
        next.apply_inner(plan, event, now)?;
        *self = next;
        Ok(())
    }
    fn apply_inner(
        &mut self,
        plan: &FrozenExperiment,
        event: &ExperimentEvent,
        now: DateTime<Utc>,
    ) -> Result<(), ExperimentError> {
        if matches!(event, ExperimentEvent::Cancelled) {
            if self.cancelled {
                return Err(invalid("already cancelled"));
            }
            self.cancelled = true;
            return Ok(());
        }
        let name = event.stage().ok_or_else(|| invalid("missing event stage"))?;
        let stage = plan
            .spec()
            .stages
            .iter()
            .find(|stage| stage.id == name)
            .ok_or_else(|| invalid("unknown stage"))?;
        let old = self.stages.get(name).ok_or_else(|| invalid("missing stage state"))?;
        let new = match event {
            ExperimentEvent::Reserved { .. } => {
                plan.ensure_live(now)?;
                if self.cancelled || !matches!(old, StageState::Planned) {
                    return Err(invalid("stage cannot reserve"));
                }
                if !stage
                    .dependencies
                    .iter()
                    .all(|id| self.stages.get(id).is_some_and(StageState::successful))
                {
                    return Err(invalid("dependencies are not successfully cleaned up"));
                }
                if self.stages.values().filter(|value| value.active()).count() >= plan.spec().max_concurrent as usize {
                    return Err(invalid("concurrency exhausted"));
                }
                if !self
                    .committed(plan)?
                    .checked_add(stage.allocation)?
                    .within(plan.spec().resources)
                {
                    return Err(ExperimentError::BudgetExceeded);
                }
                StageState::Reserved
            }
            ExperimentEvent::Dispatched { .. } => {
                plan.ensure_live(now)?;
                if self.cancelled || !matches!(old, StageState::Reserved) {
                    return Err(invalid("stage cannot dispatch"));
                }
                StageState::Dispatched
            }
            ExperimentEvent::DispatchUnknown { .. } if matches!(old, StageState::Dispatched) => {
                StageState::DispatchUnknown
            }
            ExperimentEvent::Settled {
                usage,
                evidence_sha256,
                successful,
                ..
            } if matches!(old, StageState::Dispatched | StageState::DispatchUnknown) => {
                if !super::types::is_digest(evidence_sha256) {
                    return Err(ExperimentError::InvalidEvidence("missing evidence digest".into()));
                }
                // Preserve an observed overrun rather than dropping the receipt.
                // It prevents further reservations and can never qualify.
                StageState::Settled {
                    usage: *usage,
                    evidence_sha256: evidence_sha256.clone(),
                    successful: *successful && usage.within(stage.allocation),
                    cleanup_confirmed: false,
                }
            }
            ExperimentEvent::CleanupConfirmed { .. } => match old {
                StageState::Settled {
                    usage,
                    evidence_sha256,
                    successful,
                    cleanup_confirmed: false,
                } => StageState::Settled {
                    usage: *usage,
                    evidence_sha256: evidence_sha256.clone(),
                    successful: *successful,
                    cleanup_confirmed: true,
                },
                // A reservation that never handed off has no execution liability.
                StageState::Reserved => StageState::Settled {
                    usage: ExperimentResources::default(),
                    evidence_sha256: stage.input_sha256.clone(),
                    successful: false,
                    cleanup_confirmed: true,
                },
                _ => return Err(invalid("cleanup requires a settled or never-dispatched stage")),
            },
            _ => return Err(invalid("illegal or duplicate stage transition")),
        };
        self.stages.insert(name.to_string(), new);
        Ok(())
    }
}
fn invalid(message: &str) -> ExperimentError {
    ExperimentError::InvalidTransition(message.into())
}
