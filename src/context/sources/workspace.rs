//! The workspace line.
use crate::context::layout::{PromptInputs, Segment, Slot, Source};
use anyhow::Result;

pub struct WorkspaceSection;

impl Source for WorkspaceSection {
    fn slot(&self) -> Slot {
        Slot::Workspace
    }

    fn render(&self, inputs: &PromptInputs<'_>) -> Result<Option<Segment>> {
        Ok(Segment::new(format!(
            "## Workspace\n\nWorking directory: `{}`",
            inputs.workspace_dir.display()
        )))
    }
}
