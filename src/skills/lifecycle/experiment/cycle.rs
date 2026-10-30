//! Pure bounded instruction-cycle compiler. Owns only ordering and operation identities; the
//! existing service resolves authority, runs stages and records their effects.
use super::{
    DevelopmentRole, ExperimentDraft, ExperimentDraftStage, ExperimentError, ExperimentId, ExperimentOperation,
    ExperimentResources, ExperimentSpec,
};
use crate::skills::lifecycle::{EvalSplit, PatternId, SkillId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstructionSchedule {
    #[default]
    Single,
    #[serde(rename = "sequential_2")]
    Sequential2,
    #[serde(rename = "parallel_2")]
    Parallel2,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstructionCycleDraft {
    pub id: ExperimentId,
    pub skill_id: SkillId,
    pub patterns: Vec<PatternId>,
    #[serde(default)]
    pub schedule: InstructionSchedule,
    pub verification_split: EvalSplit,
    pub resources: ExperimentResources,
    pub proposal_allocation: ExperimentResources,
    pub probe_allocation: ExperimentResources,
    pub evaluation_allocation: ExperimentResources,
    pub elapsed_ms: u64,
    pub max_input_bytes: usize,
    pub max_output_bytes: usize,
    pub proposal_timeout_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstructionExperimentPlan {
    pub spec: ExperimentSpec,
    pub operations: BTreeMap<String, ExperimentOperation>,
}
impl InstructionExperimentPlan {
    /// Check every input before the first effect, including unused extra keys.
    pub fn validate(&self) -> Result<(), ExperimentError> {
        self.spec.clone().freeze()?;
        if self.spec.stages.len() != self.operations.len() {
            return Err(ExperimentError::InvalidSpec(
                "operation set differs from frozen stages".into(),
            ));
        }
        for stage in &self.spec.stages {
            let input = self
                .operations
                .get(&stage.id)
                .ok_or_else(|| ExperimentError::InvalidSpec("stage operation missing".into()))?;
            if input.digest()? != stage.input_sha256 || input.kind() != stage.kind {
                return Err(ExperimentError::InvalidSpec("operation changed after planning".into()));
            }
        }
        Ok(())
    }
}
impl InstructionCycleDraft {
    pub fn compile(self) -> Result<ExperimentDraft, ExperimentError> {
        use ExperimentOperation::{DevelopmentProbe, DevelopmentSelection, SelectedEvaluation, Verify};
        if self.patterns.is_empty() || self.patterns.len() > super::super::MAX_PROPOSAL_PATTERNS {
            return Err(ExperimentError::InvalidSpec(
                "cycle needs bounded verified pattern inputs".into(),
            ));
        }
        super::super::ProposerBudget::new(
            self.max_input_bytes,
            self.max_output_bytes,
            std::time::Duration::from_millis(self.proposal_timeout_ms),
        )?;
        let mut stages = vec![ExperimentDraftStage {
            id: "baseline".into(),
            dependencies: vec![],
            operation: DevelopmentProbe { proposal_stage: None },
            allocation: self.probe_allocation,
        }];
        match self.schedule {
            InstructionSchedule::Single => stages.extend(self.round("", "select", None)),
            InstructionSchedule::Sequential2 => {
                stages.extend(self.round("", "select-round1", None));
                stages.extend(self.round("round2-", "select", Some("select-round1")));
            }
            InstructionSchedule::Parallel2 => {
                let left = self.round("branch-a-", "select-a", None);
                let right = self.round("branch-b-", "select-b", None);
                for (a, b) in left.into_iter().zip(right) {
                    stages.push(a);
                    stages.push(b);
                }
                stages.push(ExperimentDraftStage {
                    id: "select".into(),
                    dependencies: vec!["baseline".into(), "select-a".into(), "select-b".into()],
                    operation: DevelopmentSelection {
                        probe_stages: vec!["baseline".into()],
                        parent_selections: vec!["select-a".into(), "select-b".into()],
                    },
                    allocation: ExperimentResources::default(),
                });
            }
        }
        stages.extend([
            ExperimentDraftStage {
                id: "final-evaluation".into(),
                dependencies: vec!["select".into()],
                operation: SelectedEvaluation {
                    selection_stage: "select".into(),
                    split: self.verification_split,
                },
                allocation: self.evaluation_allocation,
            },
            ExperimentDraftStage {
                id: "verify".into(),
                dependencies: vec!["final-evaluation".into()],
                operation: Verify {
                    evaluation_stage: "final-evaluation".into(),
                },
                allocation: ExperimentResources::default(),
            },
        ]);
        let reserved = stages.iter().try_fold(ExperimentResources::default(), |sum, stage| {
            sum.checked_add(stage.allocation)
        })?;
        if !reserved.within(self.resources) {
            return Err(ExperimentError::BudgetExceeded);
        }
        Ok(ExperimentDraft {
            id: self.id,
            skill_id: self.skill_id,
            verification_split: self.verification_split,
            resources: self.resources,
            elapsed_ms: self.elapsed_ms,
            max_concurrent: if self.schedule == InstructionSchedule::Parallel2 {
                2
            } else {
                1
            },
            stages,
        })
    }
    fn round(&self, prefix: &str, selection: &str, parent: Option<&str>) -> Vec<ExperimentDraftStage> {
        use ExperimentOperation::{DevelopmentProbe, DevelopmentProposal, DevelopmentSelection};
        let general = format!("{prefix}general");
        let domain = format!("{prefix}domain");
        let compose = format!("{prefix}compose");
        let gp = format!("{prefix}general-probe");
        let dp = format!("{prefix}domain-probe");
        let cp = format!("{prefix}compose-probe");
        let proposal = |id: String, role, probes: Vec<String>| {
            let mut dependencies = probes.clone();
            dependencies.extend(parent.map(str::to_owned));
            ExperimentDraftStage {
                id,
                dependencies,
                allocation: self.proposal_allocation,
                operation: DevelopmentProposal {
                    role,
                    probe_stages: probes,
                    patterns: self.patterns.clone(),
                    parent_selection: parent.map(str::to_owned),
                    max_input_bytes: self.max_input_bytes,
                    max_output_bytes: self.max_output_bytes,
                    timeout_ms: self.proposal_timeout_ms,
                },
            }
        };
        let probe = |id: String, proposal: String| ExperimentDraftStage {
            id,
            dependencies: vec![proposal.clone()],
            operation: DevelopmentProbe {
                proposal_stage: Some(proposal),
            },
            allocation: self.probe_allocation,
        };
        let comparisons = vec!["baseline".into(), gp.clone(), dp.clone(), cp.clone()];
        let mut dependencies = comparisons.clone();
        dependencies.extend(parent.map(str::to_owned));
        vec![
            proposal(general.clone(), DevelopmentRole::General, vec!["baseline".into()]),
            proposal(domain.clone(), DevelopmentRole::Domain, vec!["baseline".into()]),
            probe(gp.clone(), general),
            probe(dp.clone(), domain),
            proposal(
                compose.clone(),
                DevelopmentRole::Compose,
                vec!["baseline".into(), gp, dp],
            ),
            probe(cp, compose),
            ExperimentDraftStage {
                id: selection.into(),
                dependencies,
                operation: DevelopmentSelection {
                    probe_stages: comparisons,
                    parent_selections: parent.map(str::to_owned).into_iter().collect(),
                },
                allocation: ExperimentResources::default(),
            },
        ]
    }
}
