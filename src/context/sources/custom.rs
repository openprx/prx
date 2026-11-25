//! A section a caller adds through `SystemPromptBuilder::add_section`.
use crate::agent::prompt::PromptSection;
use crate::context::layout::{PromptInputs, Segment, Slot, Source};
use anyhow::Result;

pub(crate) struct CustomSection(Box<dyn PromptSection>);

impl CustomSection {
    pub(crate) fn new(section: Box<dyn PromptSection>) -> Self {
        Self(section)
    }
}

impl Source for CustomSection {
    fn slot(&self) -> Slot {
        Slot::Custom
    }

    fn name(&self) -> &str {
        self.0.name()
    }

    fn render(&self, inputs: &PromptInputs<'_>) -> Result<Option<Segment>> {
        Ok(Segment::new(self.0.build(inputs)?))
    }
}
