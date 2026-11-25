//! The safety section.
use crate::context::layout::{PromptInputs, Segment, Slot, Source};
use anyhow::Result;

/// The safety paragraph of a Full turn's system prompt.
pub const FULL_AUTONOMY_SAFETY: &str = "You are running with full autonomy. PRX applies no restrictions to commands, \
     paths, network access or tools; only operating-system permissions apply. Act directly and do not ask for \
     confirmation unless the user asked you to.";

pub struct SafetySection;

impl Source for SafetySection {
    fn slot(&self) -> Slot {
        Slot::Safety
    }

    fn render(&self, inputs: &PromptInputs<'_>) -> Result<Option<Segment>> {
        if inputs.autonomy == crate::security::AutonomyLevel::Full {
            return Ok(Segment::new(format!("## Safety\n\n{FULL_AUTONOMY_SAFETY}")));
        }
        Ok(Segment::new("## Safety\n\n- Do not exfiltrate private data.\n- Do not run destructive commands without asking.\n- Do not bypass oversight or approval mechanisms.\n- Prefer `trash` over `rm` (recoverable beats gone forever).\n- When in doubt, ask before acting externally.".into()))
    }
}
