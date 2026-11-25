//! The identity of a named agent: its own composed role text takes the
//! identity slot in place of the main persona.
use crate::context::layout::{PromptInputs, Segment, Slot, Source};
use anyhow::Result;

pub(crate) struct RoleIdentitySection {
    text: String,
}

impl RoleIdentitySection {
    pub(crate) fn new(text: &str) -> Self {
        Self { text: text.to_owned() }
    }
}

impl Source for RoleIdentitySection {
    fn slot(&self) -> Slot {
        Slot::Identity
    }

    fn render(&self, _inputs: &PromptInputs<'_>) -> Result<Option<Segment>> {
        Ok(Segment::new(self.text.clone()))
    }
}
