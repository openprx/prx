//! Explicit reply-assessment responses for transport/publication fixtures.
//! These fixtures test the protocol, not the accuracy of a model's assessment.
#![cfg(test)]

use crate::providers::traits::{ChatRequest, ChatResponse, ChatTrace};

pub(super) const ACCEPT: &str = r#"{"decision":"accept","reason":"fixture evidence accepted","constraints":[]}"#;
pub(super) const INFORMATIONAL: &str = r#"{"requirement":"informational"}"#;
pub(super) const EXECUTION_REQUIRED: &str = r#"{"requirement":"execution_required"}"#;

pub(super) fn reply(system: &str, requirement: &'static str) -> Option<&'static str> {
    if system.contains("Review the proposed final reply against the CURRENT request") {
        Some(ACCEPT)
    } else if system.contains("Classify the execution requirement of current_request") {
        assert!(matches!(requirement, INFORMATIONAL | EXECUTION_REQUIRED));
        Some(requirement)
    } else {
        None
    }
}

pub(super) fn response(request: &ChatRequest<'_>, requirement: &'static str) -> Option<ChatResponse> {
    let system = request.messages.first().filter(|message| message.role == "system")?;
    let text = reply(&system.content, requirement)?;
    assert!(request.tools.is_none(), "assessment cannot publish tools");
    assert_eq!(request.messages.len(), 2, "bounded assessment envelope");
    let evidence: serde_json::Value = serde_json::from_str(&request.messages.get(1)?.content).unwrap();
    if text == ACCEPT {
        assert!(
            evidence
                .get("observed_tools")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|tools| !tools.is_empty())
        );
        assert!(
            evidence
                .get("proposed_final_reply")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|draft| !draft.is_empty())
        );
    } else {
        assert!(evidence.get("conversation").is_some_and(serde_json::Value::is_array));
        assert!(
            evidence
                .get("current_request")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|text| !text.trim().is_empty())
        );
    }
    Some(ChatResponse {
        text: Some(text.to_owned()),
        tool_calls: Vec::new(),
        reasoning_content: None,
    })
}

pub(super) fn trace(request: &ChatRequest<'_>, model: &str, requirement: &'static str) -> Option<ChatTrace> {
    Some(ChatTrace {
        response: response(request, requirement)?,
        attempts: Vec::new(),
        final_provider: "fixture".to_owned(),
        final_model: model.to_owned(),
        served_member: None,
        tokens_used: Default::default(),
    })
}
