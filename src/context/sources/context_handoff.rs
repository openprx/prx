//! The context-handoff rule (`compaction::CONTEXT_HANDOFF_PROMPT_RULE`).
use crate::context::layout::{PromptInputs, Segment, Slot, Source};
use anyhow::Result;

pub struct ContextHandoffSection;

impl Source for ContextHandoffSection {
    fn slot(&self) -> Slot {
        Slot::ContextHandoff
    }

    fn render(&self, _inputs: &PromptInputs<'_>) -> Result<Option<Segment>> {
        Ok(Segment::new(
            crate::agent::compaction::CONTEXT_HANDOFF_PROMPT_RULE.to_owned(),
        ))
    }
}
