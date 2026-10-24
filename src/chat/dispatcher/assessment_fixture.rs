//! Explicit reply-assessment responses for transport/publication fixtures.
//! These fixtures test the protocol, not the accuracy of a model's assessment.
#![cfg(test)]

use crate::providers::traits::{ChatRequest, ChatResponse, ChatTrace};

pub(super) const ACCEPT: &str = r#"{"decision":"accept","reason":"fixture evidence accepted"}"#;
pub(super) const INFORMATIONAL: &str = r#"{"requirement":"informational"}"#;

pub(super) fn reply(system: &str) -> Option<&'static str> {
    if system.contains("Review the proposed final reply against the CURRENT request") {
        Some(ACCEPT)
    } else if system.contains("Classify the execution requirement of the CURRENT user request") {
        Some(INFORMATIONAL)
    } else {
        None
    }
}

pub(super) fn response(request: &ChatRequest<'_>) -> Option<ChatResponse> {
    let system = request.messages.first().filter(|message| message.role == "system")?;
    let text = reply(&system.content)?;
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
    }
    Some(ChatResponse {
        text: Some(text.to_owned()),
        tool_calls: Vec::new(),
        reasoning_content: None,
    })
}

pub(super) fn trace(request: &ChatRequest<'_>, model: &str) -> Option<ChatTrace> {
    Some(ChatTrace {
        response: response(request)?,
        attempts: Vec::new(),
        final_provider: "fixture".to_owned(),
        final_model: model.to_owned(),
        served_member: None,
        tokens_used: Default::default(),
    })
}
