use crate::providers::{ChatMessage, ChatResponse, ConversationMessage, ToolResultMessage};
use serde_json::Value;
use std::fmt::Write;

#[derive(Debug, Clone)]
pub struct ParsedToolCall {
    pub name: String,
    pub arguments: Value,
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ToolExecutionResult {
    pub name: String,
    pub output: String,
    pub success: bool,
    pub tool_call_id: Option<String>,
}

pub trait ToolDispatcher: Send + Sync {
    fn parse_response(&self, response: &ChatResponse) -> (String, Vec<ParsedToolCall>);
    fn format_results(&self, results: &[ToolExecutionResult]) -> ConversationMessage;
    fn to_provider_messages(&self, history: &[ConversationMessage]) -> Vec<ChatMessage>;
    /// Restore the public history view without changing the provider wire
    /// representation. Custom dispatchers retain raw messages by default.
    fn restore_history(&self, history: Vec<ChatMessage>) -> Vec<ConversationMessage> {
        history.into_iter().map(ConversationMessage::Chat).collect()
    }
    fn should_send_tool_specs(&self) -> bool;
}

#[derive(Default)]
pub struct XmlToolDispatcher;

impl XmlToolDispatcher {
    fn parse_xml_tool_calls(response: &str) -> (String, Vec<ParsedToolCall>) {
        let mut text_parts = Vec::new();
        let mut calls = Vec::new();
        let mut remaining = response;

        while let Some(start) = remaining.find("<tool_call>") {
            let before = &remaining[..start];
            if !before.trim().is_empty() {
                text_parts.push(before.trim().to_string());
            }

            if let Some(end) = remaining[start..].find("</tool_call>") {
                let inner = &remaining[start + 11..start + end];
                match serde_json::from_str::<Value>(inner.trim()) {
                    Ok(parsed) => {
                        let name = parsed.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                        if name.is_empty() {
                            remaining = &remaining[start + end + 12..];
                            continue;
                        }
                        let arguments = parsed
                            .get("arguments")
                            .cloned()
                            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
                        calls.push(ParsedToolCall {
                            name,
                            arguments,
                            tool_call_id: None,
                        });
                    }
                    Err(e) => {
                        tracing::warn!("Malformed <tool_call> JSON: {e}");
                    }
                }
                remaining = &remaining[start + end + 12..];
            } else {
                break;
            }
        }

        if !remaining.trim().is_empty() {
            text_parts.push(remaining.trim().to_string());
        }

        (text_parts.join("\n"), calls)
    }
}

impl ToolDispatcher for XmlToolDispatcher {
    fn parse_response(&self, response: &ChatResponse) -> (String, Vec<ParsedToolCall>) {
        let text = response.text_or_empty();
        Self::parse_xml_tool_calls(text)
    }

    fn format_results(&self, results: &[ToolExecutionResult]) -> ConversationMessage {
        let mut content = String::new();
        for result in results {
            let status = if result.success { "ok" } else { "error" };
            let _ = writeln!(
                content,
                "<tool_result name=\"{}\" status=\"{}\">\n{}\n</tool_result>",
                result.name, status, result.output
            );
        }
        ConversationMessage::Chat(ChatMessage::user(format!(
            "{}\n{content}",
            crate::agent::turn_contracts::TOOL_RESULTS_PREFIX
        )))
    }

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

    fn should_send_tool_specs(&self) -> bool {
        false
    }
}

pub struct NativeToolDispatcher;

impl ToolDispatcher for NativeToolDispatcher {
    fn parse_response(&self, response: &ChatResponse) -> (String, Vec<ParsedToolCall>) {
        let text = response.text.clone().unwrap_or_default();
        let calls = response
            .tool_calls
            .iter()
            .map(|tc| ParsedToolCall {
                name: tc.name.clone(),
                arguments: serde_json::from_str(&tc.arguments).unwrap_or_else(|e| {
                    tracing::warn!(
                        tool = %tc.name,
                        error = %e,
                        "Failed to parse native tool call arguments as JSON; defaulting to empty object"
                    );
                    Value::Object(serde_json::Map::new())
                }),
                tool_call_id: Some(tc.id.clone()),
            })
            .collect();
        (text, calls)
    }

    fn format_results(&self, results: &[ToolExecutionResult]) -> ConversationMessage {
        let messages = results
            .iter()
            .map(|result| ToolResultMessage {
                tool_call_id: result.tool_call_id.clone().unwrap_or_else(|| "unknown".to_string()),
                content: result.output.clone(),
            })
            .collect();
        ConversationMessage::ToolResults(messages)
    }

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

    fn should_send_tool_specs(&self) -> bool {
        true
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

    #[test]
    fn xml_dispatcher_parses_tool_calls() {
        let response = ChatResponse {
            text: Some(
                "Checking\n<tool_call>{\"name\":\"shell\",\"arguments\":{\"command\":\"ls\"}}</tool_call>".into(),
            ),
            tool_calls: vec![],
            reasoning_content: None,
        };
        let dispatcher = XmlToolDispatcher;
        let (_, calls) = dispatcher.parse_response(&response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
    }

    #[test]
    fn native_dispatcher_roundtrip() {
        let response = ChatResponse {
            text: Some("ok".into()),
            tool_calls: vec![crate::providers::ToolCall {
                id: "tc1".into(),
                name: "file_read".into(),
                arguments: "{\"path\":\"a.txt\"}".into(),
            }],
            reasoning_content: None,
        };
        let dispatcher = NativeToolDispatcher;
        let (_, calls) = dispatcher.parse_response(&response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool_call_id.as_deref(), Some("tc1"));

        let msg = dispatcher.format_results(&[ToolExecutionResult {
            name: "file_read".into(),
            output: "hello".into(),
            success: true,
            tool_call_id: Some("tc1".into()),
        }]);
        match msg {
            ConversationMessage::ToolResults(results) => {
                assert_eq!(results.len(), 1);
                assert_eq!(results[0].tool_call_id, "tc1");
            }
            _ => panic!("expected tool results"),
        }
    }

    #[test]
    fn xml_format_results_contains_tool_result_tags() {
        let dispatcher = XmlToolDispatcher;
        let msg = dispatcher.format_results(&[ToolExecutionResult {
            name: "shell".into(),
            output: "ok".into(),
            success: true,
            tool_call_id: None,
        }]);
        let rendered = match msg {
            ConversationMessage::Chat(chat) => chat.content,
            _ => String::new(),
        };
        assert!(rendered.contains("<tool_result"));
        assert!(rendered.contains("shell"));
    }

    #[test]
    fn native_format_results_keeps_tool_call_id() {
        let dispatcher = NativeToolDispatcher;
        let msg = dispatcher.format_results(&[ToolExecutionResult {
            name: "shell".into(),
            output: "ok".into(),
            success: true,
            tool_call_id: Some("tc-1".into()),
        }]);

        match msg {
            ConversationMessage::ToolResults(results) => {
                assert_eq!(results.len(), 1);
                assert_eq!(results[0].tool_call_id, "tc-1");
            }
            _ => panic!("expected ToolResults variant"),
        }
    }
}
