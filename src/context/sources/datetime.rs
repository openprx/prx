//! The time-zone line. It carries no date: the date is in the tail of the
//! live user message.
use crate::context::layout::{PromptInputs, Segment, Slot, Source};
use anyhow::Result;
use chrono::Local;

pub struct DateTimeSection;

impl Source for DateTimeSection {
    fn slot(&self) -> Slot {
        Slot::DateTime
    }

    fn render(&self, _inputs: &PromptInputs<'_>) -> Result<Option<Segment>> {
        let now = Local::now();
        Ok(Segment::new(format!(
            "## Current Date & Time\n\nTimezone: {}",
            now.format("%Z")
        )))
    }
}
