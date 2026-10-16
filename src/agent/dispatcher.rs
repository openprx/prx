use crate::providers::{ChatMessage, ConversationMessage, ToolResultMessage};
use std::fmt::Write;

/// A parsed tool call, used by the test-only `Agent::execute_tool_call` path.
#[cfg(test)]
#[derive(Debug, Clone)]
pub struct ParsedToolCall {
    pub name: String,
    pub arguments: serde_json::Value,
    pub tool_call_id: Option<String>,
}

/// The outcome of one test-only `Agent::execute_tool_call`.
#[cfg(test)]
#[derive(Debug, Clone)]
pub struct ToolExecutionResult {
    pub name: String,
    pub output: String,
    pub success: bool,
    pub tool_call_id: Option<String>,
}

pub trait ToolDispatcher: Send + Sync {
    fn to_provider_messages(&self, history: &[ConversationMessage]) -> Vec<ChatMessage>;
    /// Restore the public history view without changing the provider wire
    /// representation. Custom dispatchers retain raw messages by default.
    fn restore_history(&self, history: Vec<ChatMessage>) -> Vec<ConversationMessage> {
        history.into_iter().map(ConversationMessage::Chat).collect()
    }
}

#[derive(Default)]
pub struct XmlToolDispatcher;

impl ToolDispatcher for XmlToolDispatcher {
    fn to_provider_messages(&self, history: &[ConversationMessage]) -> Vec<ChatMessage> {
        history
            .iter()
            .flat_map(|msg| match msg {
                ConversationMessage::Chat(chat) => vec![chat.clone()],
                ConversationMessage::AssistantToolCalls { text, .. } => {
                    vec![ChatMessage::assistant(text.clone().unwrap_or_default())]
                }
                ConversationMessage::ToolResults(results) => {
                    let mut content = String::new();
                    for result in results {
                        let _ = writeln!(
                            content,
                            "<tool_result id=\"{}\">\n{}\n</tool_result>",
                            result.tool_call_id, result.content
                        );
                    }
                    vec![ChatMessage::user(format!(
                        "{}\n{content}",
                        crate::agent::turn_contracts::TOOL_RESULTS_PREFIX
                    ))]
                }
            })
            .collect()
    }
}

pub struct NativeToolDispatcher;

impl ToolDispatcher for NativeToolDispatcher {
    fn to_provider_messages(&self, history: &[ConversationMessage]) -> Vec<ChatMessage> {
        history
            .iter()
            .flat_map(|msg| match msg {
                ConversationMessage::Chat(chat) => vec![chat.clone()],
                ConversationMessage::AssistantToolCalls { text, tool_calls } => {
                    let payload = serde_json::json!({
                        "content": text,
                        "tool_calls": tool_calls,
                    });
                    vec![ChatMessage::assistant(payload.to_string())]
                }
                ConversationMessage::ToolResults(results) => results
                    .iter()
                    .map(|result| {
                        ChatMessage::tool(
                            serde_json::json!({
                                "tool_call_id": result.tool_call_id,
                                "content": result.content,
                            })
                            .to_string(),
                        )
                    })
                    .collect(),
            })
            .collect()
    }

    fn restore_history(&self, history: Vec<ChatMessage>) -> Vec<ConversationMessage> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct AssistantHistory {
            content: Option<String>,
            tool_calls: Vec<crate::providers::ToolCall>,
        }
        history
            .into_iter()
            .map(|message| {
                let candidate = match message.role.as_str() {
                    "assistant" if message.content.starts_with('{') => {
                        serde_json::from_str::<AssistantHistory>(&message.content)
                            .ok()
                            .filter(|history| !history.tool_calls.is_empty())
                            .map(|history| ConversationMessage::AssistantToolCalls {
                                text: history.content,
                                tool_calls: history.tool_calls,
                            })
                    }
                    "tool" if message.content.starts_with('{') => {
                        serde_json::from_str::<ToolResultMessage>(&message.content)
                            .ok()
                            .map(|result| ConversationMessage::ToolResults(vec![result]))
                    }
                    _ => None,
                };
                if let Some(candidate) = candidate {
                    // Reasoning, extra fields, whitespace and any unsupported
                    // shape must survive intact. Projection is presentation only.
                    let round_trip = self.to_provider_messages(std::slice::from_ref(&candidate));
                    if round_trip.len() == 1
                        && round_trip
                            .first()
                            .is_some_and(|wire| wire.role == message.role && wire.content == message.content)
                    {
                        return candidate;
                    }
                }
                ConversationMessage::Chat(message)
            })
            .collect()
    }
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_history_restores_tool_types_with_exact_wire_round_trip() {
        let dispatcher = NativeToolDispatcher;
        let messages = vec![
            ChatMessage::user("original input"),
            ChatMessage::assistant(
                serde_json::json!({"content": null, "tool_calls": [
                    {"id": "prx_call", "name": "echo", "arguments": "{}"}
                ]})
                .to_string(),
            ),
            ChatMessage::tool(serde_json::json!({"tool_call_id": "prx_call", "content": "result"}).to_string()),
            ChatMessage::assistant("final reply"),
        ];
        let history = dispatcher.restore_history(messages.clone());
        assert!(
            matches!(&history[1], ConversationMessage::AssistantToolCalls { tool_calls, .. } if tool_calls.len() == 1)
        );
        assert!(
            matches!(&history[2], ConversationMessage::ToolResults(results) if results.len() == 1 && results[0].tool_call_id == "prx_call")
        );
        let restored = dispatcher.to_provider_messages(&history);
        assert_eq!(
            serde_json::to_value(restored).unwrap(),
            serde_json::to_value(messages).unwrap()
        );
    }

    #[test]
    fn native_history_preserves_partially_completed_tool_batches() {
        let dispatcher = NativeToolDispatcher;
        let messages = vec![
            ChatMessage::assistant(
                serde_json::json!({"content": "two calls", "tool_calls": [
                    {"id": "prx_first", "name": "echo", "arguments": "{}"},
                    {"id": "prx_second", "name": "echo", "arguments": "{}"}
                ]})
                .to_string(),
            ),
            ChatMessage::tool(serde_json::json!({"tool_call_id": "prx_first", "content": "completed"}).to_string()),
        ];
        let history = dispatcher.restore_history(messages.clone());
        assert_eq!(history.len(), 2);
        assert!(
            matches!(&history[0], ConversationMessage::AssistantToolCalls { tool_calls, .. }
            if tool_calls.len() == 2 && tool_calls[1].id == "prx_second")
        );
        assert!(matches!(&history[1], ConversationMessage::ToolResults(results)
            if results.len() == 1 && results[0].tool_call_id == "prx_first"));
        assert_eq!(
            serde_json::to_value(dispatcher.to_provider_messages(&history)).unwrap(),
            serde_json::to_value(messages).unwrap()
        );
    }

    #[test]
    fn native_history_keeps_reasoning_extensions_and_malformed_payloads_verbatim() {
        let dispatcher = NativeToolDispatcher;
        let messages = vec![
            ChatMessage::assistant(
                serde_json::json!({"content": null, "reasoning_content": "reasoning",
                "tool_calls": [{"id": "prx_call", "name": "echo", "arguments": "{}"}]})
                .to_string(),
            ),
            ChatMessage::tool(
                serde_json::json!({"tool_call_id": "prx_call", "content": "result", "extension": true}).to_string(),
            ),
            ChatMessage::assistant("{malformed"),
            ChatMessage::tool(" {\"tool_call_id\":\"prx_call\",\"content\":\"result\"} "),
            ChatMessage::user("{\"content\":null,\"tool_calls\":[]}"),
        ];
        let history = dispatcher.restore_history(messages.clone());
        assert!(
            history
                .iter()
                .all(|message| matches!(message, ConversationMessage::Chat(_)))
        );
        assert_eq!(
            serde_json::to_value(dispatcher.to_provider_messages(&history)).unwrap(),
            serde_json::to_value(messages).unwrap()
        );
    }

    #[test]
    fn xml_history_does_not_reinterpret_native_json() {
        let messages = vec![ChatMessage::assistant(
            serde_json::json!({"content": null, "tool_calls": [
                {"id": "prx_call", "name": "echo", "arguments": "{}"}
            ]})
            .to_string(),
        )];
        let history = XmlToolDispatcher.restore_history(messages.clone());
        assert!(matches!(&history[0], ConversationMessage::Chat(_)));
        assert_eq!(
            serde_json::to_value(XmlToolDispatcher.to_provider_messages(&history)).unwrap(),
            serde_json::to_value(messages).unwrap()
        );
    }
}
