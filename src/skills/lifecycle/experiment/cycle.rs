//! Pure k1 stage compiler. Owns only ordering and operation identities; the
//! existing service resolves authority, runs stages and records their effects.
use super::{
    DevelopmentRole, ExperimentDraft, ExperimentDraftStage, ExperimentError, ExperimentId, ExperimentOperation,
    ExperimentResources, ExperimentSpec,
};
use crate::skills::lifecycle::{EvalSplit, PatternId, SkillId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstructionCycleDraft {
    pub id: ExperimentId,
    pub skill_id: SkillId,
    pub patterns: Vec<PatternId>,
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
        use ExperimentOperation::{
            DevelopmentProbe, DevelopmentProposal, DevelopmentSelection, SelectedEvaluation, Verify,
        };
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
        let proposal = |role, probes| DevelopmentProposal {
            role,
            probe_stages: probes,
            patterns: self.patterns.clone(),
            max_input_bytes: self.max_input_bytes,
            max_output_bytes: self.max_output_bytes,
            timeout_ms: self.proposal_timeout_ms,
        };
        let ids = |names: &[&str]| names.iter().map(|name| (*name).into()).collect::<Vec<String>>();
        let contrasts = ids(&["baseline", "general-probe", "domain-probe"]);
        let comparisons = ids(&["baseline", "general-probe", "domain-probe", "compose-probe"]);
        let mut stages = Vec::new();
        let mut add = |name: &str, dependencies: Vec<String>, operation, allocation| {
            stages.push(ExperimentDraftStage {
                id: name.into(),
                dependencies,
                operation,
                allocation,
            })
        };
        add(
            "baseline",
            vec![],
            DevelopmentProbe { proposal_stage: None },
            self.probe_allocation,
        );
        add(
            "general",
            ids(&["baseline"]),
            proposal(DevelopmentRole::General, ids(&["baseline"])),
            self.proposal_allocation,
        );
        add(
            "domain",
            ids(&["baseline"]),
            proposal(DevelopmentRole::Domain, ids(&["baseline"])),
            self.proposal_allocation,
        );
        add(
            "general-probe",
            ids(&["general"]),
            DevelopmentProbe {
                proposal_stage: Some("general".into()),
            },
            self.probe_allocation,
        );
        add(
            "domain-probe",
            ids(&["domain"]),
            DevelopmentProbe {
                proposal_stage: Some("domain".into()),
            },
            self.probe_allocation,
        );
        add(
            "compose",
            contrasts.clone(),
            proposal(DevelopmentRole::Compose, contrasts),
            self.proposal_allocation,
        );
        add(
            "compose-probe",
            ids(&["compose"]),
            DevelopmentProbe {
                proposal_stage: Some("compose".into()),
            },
            self.probe_allocation,
        );
        add(
            "select",
            comparisons.clone(),
            DevelopmentSelection {
                probe_stages: comparisons,
            },
            ExperimentResources::default(),
        );
        add(
            "final-evaluation",
            ids(&["select"]),
            SelectedEvaluation {
                selection_stage: "select".into(),
                split: self.verification_split,
            },
            self.evaluation_allocation,
        );
        add(
            "verify",
            ids(&["final-evaluation"]),
            Verify {
                evaluation_stage: "final-evaluation".into(),
            },
            ExperimentResources::default(),
        );
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
            max_concurrent: 1,
            stages,
        })
    }
}
