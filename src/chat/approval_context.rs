//! Foreground chat retains its interactive approval strategies.
use crate::runtime::envelope::RuntimeEnvelope;
use crate::tools::ToolExecutionContext;
pub(super) fn tui(envelope: RuntimeEnvelope) -> ToolExecutionContext {
    ToolExecutionContext::new(envelope, "private").with_chat_id("terminal:user")
}
pub(super) fn line_mode(envelope: RuntimeEnvelope) -> ToolExecutionContext {
    ToolExecutionContext::new(envelope, "private").with_chat_id("terminal:user")
}
