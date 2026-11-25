//! The skills index.
use crate::context::layout::{PromptInputs, Segment, Slot, Source};
use anyhow::Result;

pub struct SkillsSection;

impl Source for SkillsSection {
    fn slot(&self) -> Slot {
        Slot::Skills
    }

    fn render(&self, inputs: &PromptInputs<'_>) -> Result<Option<Segment>> {
        Ok(Segment::new(crate::skills::skills_to_prompt(
            inputs.skills,
            inputs.workspace_dir,
        )))
    }
}
