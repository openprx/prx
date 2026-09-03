#[allow(warnings)]
#[cfg(target_arch = "wasm32")]
mod bindings;

struct EchoProvider;

#[cfg(target_arch = "wasm32")]
mod wasm_exports {
    use super::{bindings, EchoProvider};
    use bindings::exports::prx::plugin::provider_exports::{ChatMessage, ChatResponse, Guest};

    impl Guest for EchoProvider {
        fn name() -> String {
            "wasm-echo".to_string()
        }

        fn chat(messages: Vec<ChatMessage>, model: String, _temperature: f64) -> Result<ChatResponse, String> {
            let content = messages
                .iter()
                .rev()
                .find(|message| message.role == "user")
                .map(|message| message.content.as_str())
                .unwrap_or("");
            Ok(ChatResponse {
                text: Some(format!("wasm-echo/{model}: {content}")),
                tool_calls: Vec::new(),
            })
        }
    }

    bindings::export!(EchoProvider with_types_in bindings);
}
