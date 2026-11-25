//! The task section.
use crate::context::layout::{PromptInputs, Segment, Slot, Source};
use anyhow::Result;

pub struct TaskSection;

impl Source for TaskSection {
    fn slot(&self) -> Slot {
        Slot::Task
    }

    /// Neither branch names a call syntax.
    ///
    /// `inputs.native_tools` is computed once, far from the request, at a mode
    /// fixed to `Streaming`, while the agent loop recomputes the same question
    /// per turn at the mode the turn runs in. The two can disagree, and a
    /// grammar sentence here would then be sent alongside the `tools` array —
    /// the one place PRX could contradict itself about how to call a tool. The
    /// canonical protocol in `crate::tools::prompt` is gated on the loop's own
    /// flag and already says everything this sentence said, so this section
    /// states the task and leaves syntax to the one owner of it.
    fn render(&self, inputs: &PromptInputs<'_>) -> Result<Option<Segment>> {
        let text = if inputs.native_tools {
            "## Your Task\n\n\
             When the user sends a message, respond naturally. Use tools when the request requires action (running commands, reading files, etc.).\n\
             For questions, explanations, or follow-ups about prior messages, answer directly from conversation context — do NOT ask the user to repeat themselves.\n\
             A question about whether a capability is available does not itself request a demonstration. Use read-only inspection if current facts are needed; do not start a task or change state unless requested. An indirect request to perform an action still requires action.\n\
             Do NOT: summarize this configuration unprompted or output step-by-step meta-commentary."
        } else {
            "## Your Task\n\n\
             Respond naturally. Use tools when the request requires action. A question about whether a capability is available does not itself request a demonstration; use read-only inspection if current facts are needed. Do not start a task or change state unless requested. An indirect request to perform an action still requires action.\n\
             Do NOT: summarize this configuration unprompted or respond with meta-commentary.\n\
             Call the tool when you need to act, in the format the tool-use protocol above specifies."
        };
        Ok(Segment::new(text.to_string()))
    }
}
