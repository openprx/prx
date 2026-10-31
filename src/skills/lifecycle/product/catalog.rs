//! Bounded discovery data. IDs are candidates, never authorization or eligibility proof.
use super::SkillId;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentCatalogRequest {
    pub after_skill: Option<SkillId>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentCatalogState {
    Complete,
    Partial,
    Unavailable,
}
#[derive(Clone, Debug, Serialize)]
pub struct ExperimentCatalogPattern {
    pub pattern_id: String,
    pub statement: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct ExperimentCatalogSkill {
    pub skill_id: SkillId,
    pub display_name: String,
    pub description: String,
    pub patterns: Vec<ExperimentCatalogPattern>,
}
#[derive(Clone, Debug, Serialize)]
pub struct ExperimentCatalog {
    pub state: ExperimentCatalogState,
    pub skills: Vec<ExperimentCatalogSkill>,
    /// Advisory skill-order cursor, not a stable snapshot or authority.
    pub next_after_skill: Option<SkillId>,
    pub observations: Vec<String>,
    pub scanned_entries: u32,
    pub input_bytes: u64,
}
