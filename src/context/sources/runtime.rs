//! The runtime line: host, operating system and model.
use crate::context::layout::{PromptInputs, Segment, Slot, Source};
use anyhow::Result;

pub struct RuntimeSection;

impl Source for RuntimeSection {
    fn slot(&self) -> Slot {
        Slot::Runtime
    }

    fn render(&self, inputs: &PromptInputs<'_>) -> Result<Option<Segment>> {
        let host = hostname::get().map_or_else(|_| "unknown".into(), |h| h.to_string_lossy().to_string());
        Ok(Segment::new(format!(
            "## Runtime\n\nHost: {host} | OS: {} | Model: {}",
            std::env::consts::OS,
            inputs.model_name
        )))
    }
}
