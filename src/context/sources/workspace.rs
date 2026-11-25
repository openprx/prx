//! The workspace line.
use crate::context::layout::{PromptInputs, Segment, Slot, Source};
use anyhow::Result;

pub struct WorkspaceSection;

impl Source for WorkspaceSection {
    fn slot(&self) -> Slot {
        Slot::Workspace
    }

    fn render(&self, inputs: &PromptInputs<'_>) -> Result<Option<Segment>> {
        if !super::host_identity::share_host_identity() {
            return Ok(Segment::new("## Workspace\n\nWorking directory: `.`".to_string()));
        }
        Ok(Segment::new(format!(
            "## Workspace\n\nWorking directory: `{}`",
            inputs.workspace_dir.display()
        )))
    }
}
