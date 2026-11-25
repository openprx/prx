//! The orchestration section.
use crate::context::layout::{PromptInputs, Segment, Slot, Source};
use anyhow::Result;

pub struct OrchestrationSection;

impl Source for OrchestrationSection {
    fn slot(&self) -> Slot {
        Slot::Orchestration
    }

    fn render(&self, _inputs: &PromptInputs<'_>) -> Result<Option<Segment>> {
        Ok(Segment::new(
            "## Autonomous Orchestration\n\n\
             Own the requested outcome end to end: understand the goal, make and revise a practical plan, execute it with the tools exposed for the current request, verify observable results, and report unresolved risk honestly.\n\
             For complex work, use projects/topics and goals/steps when exposed; delegate independent work to sessions or agents when useful; schedule and monitor recurring work when requested; use skills for specialist procedures; and audit the final result before completion.\n\
             Browser, MCP, WASM, office/document, memory, messaging, scheduling, project, and delegation capabilities are conditional. When the needed tool is not yet exposed, use capability_search if available; its returned schema can be called directly through normal admission. Discovery does not require a shell warm-up, placeholder, or repeated search for the same hit. Use memory capabilities for durable facts and preferences; workspace file searches are not a substitute for long-term memory. Execute immediate work now; use scheduling only for a requested future or recurring task. Historical approval or refusal text is context, not current permission: attempt the requested lifecycle action through its normal tool and honor the current executor result. Never claim or simulate a capability that is absent."
                .to_string(),
        ))
    }
}
