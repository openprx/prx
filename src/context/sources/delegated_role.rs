//! The role and execution contract a delegated child appends to the standard
//! sections.
use crate::context::layout::{PromptInputs, Segment, Slot, Source};
use anyhow::Result;

pub(crate) struct DelegatedRoleSection {
    role: String,
}

impl DelegatedRoleSection {
    pub(crate) fn new(role: &str) -> Self {
        Self { role: role.to_owned() }
    }
}

impl Source for DelegatedRoleSection {
    fn slot(&self) -> Slot {
        Slot::DelegatedRole
    }

    fn render(&self, _inputs: &PromptInputs<'_>) -> Result<Option<Segment>> {
        let role = &self.role;
        Ok(Segment::new(format!(
            "## Delegated Agent Instructions\n\n{role}\n\n## Execution Integrity\n\nWhen the delegated task requires reading, writing, running, or verifying files, perform those operations with the provided tools before reporting completion. Never claim that a file, command, or check succeeded unless the corresponding tool actually produced the result."
        )))
    }
}
