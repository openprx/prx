//! Foreground chat retains its interactive approval strategies.
use crate::runtime::envelope::RuntimeEnvelope;
use crate::tools::ToolExecutionContext;
pub(crate) fn tui(envelope: RuntimeEnvelope) -> ToolExecutionContext {
    ToolExecutionContext::new(envelope, "private")
        .with_chat_id("terminal:user")
        .with_dispatch_origin(crate::tools::DispatchOrigin::Operator)
}
pub(crate) fn line_mode(envelope: RuntimeEnvelope) -> ToolExecutionContext {
    ToolExecutionContext::new(envelope, "private")
        .with_chat_id("terminal:user")
        .with_dispatch_origin(crate::tools::DispatchOrigin::Operator)
}
