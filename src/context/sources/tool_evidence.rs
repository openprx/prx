//! The tool-evidence rule (`tool_evidence::TOOL_EVIDENCE_PROMPT_RULE`).
use crate::context::layout::{PromptInputs, Segment, Slot, Source};
use anyhow::Result;

pub struct ToolEvidenceSection;

impl Source for ToolEvidenceSection {
    fn slot(&self) -> Slot {
        Slot::ToolEvidence
    }

    fn render(&self, _inputs: &PromptInputs<'_>) -> Result<Option<Segment>> {
        Ok(Segment::new(
            crate::agent::tool_evidence::TOOL_EVIDENCE_PROMPT_RULE.to_owned(),
        ))
    }
}
