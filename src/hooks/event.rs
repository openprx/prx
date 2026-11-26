use serde_json::json;

#[derive(Debug, Clone, Copy)]
pub enum HookEvent {
    AgentStart,
    AgentEnd,
    LlmRequest,
    LlmResponse,
    ToolCallStart,
    ToolCall,
    TurnComplete,
    Error,
    SessionOpened,
    SessionEnded,
    TurnOpened,
    ApprovalRequested,
    ApprovalDecided,
    ChildStarted,
    ChildFinished,
    ChildFailed,
    ReductionFinished,
    ReductionStarted,
    ScheduleRunStarted,
    ScheduleRunFinished,
    BudgetWarning,
    HookFailed,
}

impl HookEvent {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgentStart => "agent_start",
            Self::AgentEnd => "agent_end",
            Self::LlmRequest => "llm_request",
            Self::LlmResponse => "llm_response",
            Self::ToolCallStart => "tool_call_start",
            Self::ToolCall => "tool_call",
            Self::TurnComplete => "turn_complete",
            Self::Error => "error",
            Self::SessionOpened => "session_opened",
            Self::SessionEnded => "session_ended",
            Self::TurnOpened => "turn_opened",
            Self::ApprovalRequested => "approval_requested",
            Self::ApprovalDecided => "approval_decided",
            Self::ChildStarted => "child_started",
            Self::ChildFinished => "child_finished",
            Self::ChildFailed => "child_failed",
            Self::ReductionFinished => "reduction_finished",
            Self::ReductionStarted => "reduction_started",
            Self::ScheduleRunStarted => "schedule_run_started",
            Self::ScheduleRunFinished => "schedule_run_finished",
            Self::BudgetWarning => "budget_warning",
            Self::HookFailed => "hook_failed",
        }
    }

    #[cfg(feature = "wasm-plugins")]
    pub(super) const fn wasm_lifecycle_name(self) -> &'static str {
        match self {
            // The native hook name predates the public WASM lifecycle schema.
            Self::AgentEnd => "agent_stop",
            _ => self.as_str(),
        }
    }

    pub(super) const ALL: [Self; 22] = [
        Self::AgentStart,
        Self::AgentEnd,
        Self::LlmRequest,
        Self::LlmResponse,
        Self::ToolCallStart,
        Self::ToolCall,
        Self::TurnComplete,
        Self::Error,
        Self::SessionOpened,
        Self::SessionEnded,
        Self::TurnOpened,
        Self::ApprovalRequested,
        Self::ApprovalDecided,
        Self::ChildStarted,
        Self::ChildFinished,
        Self::ChildFailed,
        Self::ReductionFinished,
        Self::ReductionStarted,
        Self::ScheduleRunStarted,
        Self::ScheduleRunFinished,
        Self::BudgetWarning,
        Self::HookFailed,
    ];

    pub(super) fn from_name(name: &str) -> Option<Self> {
        let normalized = normalize_event_name(name);
        Self::ALL.into_iter().find(|event| event.as_str() == normalized)
    }
}

pub(super) fn normalize_event_name(name: &str) -> String {
    name.trim().to_ascii_lowercase().replace('-', "_")
}

/// The payload keys that can quote a provider label: the label itself and the
/// error texts that name it.
const PROVIDER_LABEL_PAYLOAD_KEYS: [&str; 4] = ["provider", "message", "error", "error_message"];

/// `payload` as a hook script may see it: a provider label (a `custom:` URL
/// may carry `user:password@`) and the error texts that quote one are
/// redacted with the one helper every other surface uses. Borrowed when
/// nothing needed redacting.
pub(super) fn redacted_hook_payload(payload: &serde_json::Value) -> std::borrow::Cow<'_, serde_json::Value> {
    let Some(object) = payload.as_object() else {
        return std::borrow::Cow::Borrowed(payload);
    };
    let needs_redaction = PROVIDER_LABEL_PAYLOAD_KEYS.iter().any(|key| {
        object
            .get(*key)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|text| {
                matches!(
                    crate::providers::redact_endpoint_userinfo(text),
                    std::borrow::Cow::Owned(_)
                )
            })
    });
    if !needs_redaction {
        return std::borrow::Cow::Borrowed(payload);
    }
    let mut redacted = object.clone();
    for key in PROVIDER_LABEL_PAYLOAD_KEYS {
        if let Some(serde_json::Value::String(text)) = redacted.get_mut(key)
            && let std::borrow::Cow::Owned(safe) = crate::providers::redact_endpoint_userinfo(text)
        {
            *text = safe;
        }
    }
    std::borrow::Cow::Owned(serde_json::Value::Object(redacted))
}

pub fn payload_error(component: &str, message: &str) -> serde_json::Value {
    json!({
        "component": component,
        "message": message,
    })
}
