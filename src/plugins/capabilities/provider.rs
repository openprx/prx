//! Provider capability — bridges WASM provider plugins to the PRX `Provider` trait.
//!
//! WASM provider plugins export a `chat` function that receives a list of messages
//! and returns a chat response. The host delegates LLM requests to the plugin
//! when the plugin's declared provider name matches the routing configuration.
//!
//! Streaming is intentionally not supported for WASM providers (WASM components
//! cannot hold open an async stream to the host).

use async_trait::async_trait;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Mutex;
use wasmtime::AsContextMut;

use crate::plugins::error::{PluginError, PluginResult};
use crate::plugins::host::HostState;
use crate::plugins::manifest::PluginManifest;
use crate::providers::traits::{ChatMessage, ChatResponse, Provider, ToolCall};

/// A loaded WASM provider plugin instance.
pub struct WasmProvider {
    /// The cached provider name (returned by `name()` at load time).
    provider_name: String,
    /// wasmtime store + instance, behind a mutex (Store is not Sync).
    inner: Arc<Mutex<WasmProviderInner>>,
    /// Timeout for `chat` calls (milliseconds).
    timeout_ms: u64,
}

struct WasmProviderInner {
    store: wasmtime::Store<HostState>,
    instance: wasmtime::component::Instance,
}

// SAFETY: All results[N] accesses index a Vec that was just constructed with
// the correct number of elements, so the index is always valid.
#[allow(clippy::indexing_slicing)]
impl WasmProvider {
    /// Create a new `WasmProvider` from a compiled WASM component.
    ///
    /// Steps:
    /// 1. Build `HostState` from the manifest permissions/config.
    /// 2. Instantiate the component.
    /// 3. Call `name()` export to cache the provider name.
    ///
    /// `granted_permissions` must come from `LoadedPlugin.granted_permissions`
    /// (policy-filtered), NOT directly from the manifest.
    pub async fn new(
        engine: &wasmtime::Engine,
        component: &wasmtime::component::Component,
        manifest: &PluginManifest,
        granted_permissions: HashSet<String>,
        event_bus: Option<Arc<crate::plugins::event_bus::EventBus>>,
    ) -> PluginResult<Self> {
        let timeout_ms = manifest.resources.max_execution_time_ms;

        let granted: HashSet<String> = granted_permissions;
        let optional: HashSet<String> = manifest.permissions.optional.iter().cloned().collect();
        let mut host_state = HostState::new(
            manifest.plugin.name.clone(),
            manifest.config.clone(),
            granted,
            optional,
            manifest.permissions.http_allowlist.clone(),
            timeout_ms,
        );
        if let Some(bus) = event_bus {
            host_state = host_state.with_event_bus(bus);
        }

        let mut store = wasmtime::Store::new(engine, host_state);
        store
            .set_fuel(manifest.resources.max_fuel)
            .map_err(|e| PluginError::Instantiation(format!("failed to set fuel: {e}")))?;

        let mut linker = wasmtime::component::Linker::<HostState>::new(engine);
        Self::register_host_functions(&mut linker)?;

        let instance = linker
            .instantiate_async(&mut store, component)
            .await
            .map_err(|e| PluginError::Instantiation(format!("failed to instantiate provider: {e}")))?;

        // Cache the provider name at load time.
        let provider_name = Self::call_name(&instance, &mut store).await?;

        tracing::info!(
            plugin = %manifest.plugin.name,
            provider = %provider_name,
            "WASM provider registered"
        );

        Ok(Self {
            provider_name,
            inner: Arc::new(Mutex::new(WasmProviderInner { store, instance })),
            timeout_ms,
        })
    }

    /// The provider name as declared by the WASM plugin.
    pub fn provider_name(&self) -> &str {
        &self.provider_name
    }

    /// Register host functions needed by provider world plugins.
    ///
    /// Provider world imports: log, config, http-outbound, events.
    fn register_host_functions(linker: &mut wasmtime::component::Linker<HostState>) -> PluginResult<()> {
        super::common::register_log_host_functions(linker)?;
        super::common::register_config_host_functions(linker)?;
        super::common::register_http_host_functions(linker)?;
        super::common::register_websocket_host_functions(linker)?;
        super::common::register_event_host_functions(linker)?;
        Ok(())
    }

    /// Call the `name` export to get the provider name.
    async fn call_name(
        instance: &wasmtime::component::Instance,
        store: &mut wasmtime::Store<HostState>,
    ) -> PluginResult<String> {
        let iface_idx = instance
            .get_export_index(store.as_context_mut(), None, "prx:plugin/provider-exports@0.1.0")
            .ok_or_else(|| {
                PluginError::Instantiation("plugin does not export prx:plugin/provider-exports@0.1.0".to_string())
            })?;

        let func_idx = instance
            .get_export_index(store.as_context_mut(), Some(&iface_idx), "name")
            .ok_or_else(|| PluginError::Instantiation("name not found in provider-exports".to_string()))?;

        let name_fn = instance
            .get_func(store.as_context_mut(), &func_idx)
            .ok_or_else(|| PluginError::Instantiation("name is not a function".to_string()))?;

        let mut results = vec![wasmtime::component::Val::Bool(false)];
        name_fn
            .call_async(store.as_context_mut(), &[], &mut results)
            .await
            .map_err(|e| PluginError::Runtime(format!("name() call failed: {e}")))?;

        match &results[0] {
            wasmtime::component::Val::String(s) => Ok(s.to_string()),
            _ => Err(PluginError::Runtime(
                "name() returned unexpected value type".to_string(),
            )),
        }
    }

    /// Internal: call the `chat` export with a message list.
    async fn call_chat_inner(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> PluginResult<ChatResponse> {
        let mut inner = self.inner.lock().await;
        let WasmProviderInner {
            ref mut store,
            ref instance,
        } = *inner;

        // Navigate to prx:plugin/provider-exports@0.1.0 → chat
        let iface_idx = instance
            .get_export_index(store.as_context_mut(), None, "prx:plugin/provider-exports@0.1.0")
            .ok_or_else(|| {
                PluginError::Runtime("plugin does not export prx:plugin/provider-exports@0.1.0".to_string())
            })?;

        let func_idx = instance
            .get_export_index(store.as_context_mut(), Some(&iface_idx), "chat")
            .ok_or_else(|| PluginError::Runtime("chat not found in provider-exports".to_string()))?;

        let chat_fn = instance
            .get_func(store.as_context_mut(), &func_idx)
            .ok_or_else(|| PluginError::Runtime("chat is not a function".to_string()))?;

        // Build the list<chat-message> parameter.
        let wasm_messages: Vec<wasmtime::component::Val> = messages
            .iter()
            .map(|m| {
                wasmtime::component::Val::Record(vec![
                    ("role".to_string(), wasmtime::component::Val::String(m.role.clone())),
                    (
                        "content".to_string(),
                        wasmtime::component::Val::String(m.content.clone()),
                    ),
                ])
            })
            .collect();

        let params = [
            wasmtime::component::Val::List(wasm_messages),
            wasmtime::component::Val::String(model.into()),
            wasmtime::component::Val::Float64(temperature),
        ];
        let mut results = vec![wasmtime::component::Val::Bool(false)];

        chat_fn
            .call_async(store.as_context_mut(), &params, &mut results)
            .await
            .map_err(|e| PluginError::Runtime(format!("chat() call failed: {e}")))?;

        // Parse result<chat-response, string>
        match &results[0] {
            wasmtime::component::Val::Result(r) => match r.as_ref() {
                Ok(Some(inner_val)) => Self::parse_chat_response(inner_val),
                Ok(None) => Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                }),
                Err(Some(inner_err)) => match inner_err.as_ref() {
                    wasmtime::component::Val::String(e) => Err(PluginError::Runtime(format!(
                        "WASM provider '{}' returned error: {e}",
                        self.provider_name
                    ))),
                    _ => Err(PluginError::Runtime(format!(
                        "WASM provider '{}' returned unknown error variant",
                        self.provider_name
                    ))),
                },
                Err(None) => Err(PluginError::Runtime(format!(
                    "WASM provider '{}' returned unknown error",
                    self.provider_name
                ))),
            },
            _ => Err(PluginError::Runtime(
                "chat() returned unexpected value type".to_string(),
            )),
        }
    }

    /// Parse a `chat-response` record value into a Rust `ChatResponse`.
    fn parse_chat_response(val: &wasmtime::component::Val) -> PluginResult<ChatResponse> {
        let fields = match val {
            wasmtime::component::Val::Record(f) => f,
            _ => {
                return Err(PluginError::Runtime("chat-response is not a record".to_string()));
            }
        };

        // text: option<string>
        let text = fields
            .iter()
            .find(|(k, _)| k == "text")
            .and_then(|(_, v)| match v {
                wasmtime::component::Val::Option(opt) => match opt.as_deref() {
                    Some(wasmtime::component::Val::String(s)) => Some(Some(s.to_string())),
                    _ => Some(None),
                },
                _ => None,
            })
            .flatten();

        // tool-calls: list<tool-call>
        let tool_calls = fields
            .iter()
            .find(|(k, _)| k == "tool-calls")
            .and_then(|(_, v)| match v {
                wasmtime::component::Val::List(items) => {
                    let calls: Vec<ToolCall> = items.iter().filter_map(Self::parse_tool_call).collect();
                    Some(calls)
                }
                _ => None,
            })
            .unwrap_or_default();

        Ok(ChatResponse {
            text,
            tool_calls,
            reasoning_content: None,
        })
    }

    /// Parse a single `tool-call` record value.
    fn parse_tool_call(val: &wasmtime::component::Val) -> Option<ToolCall> {
        let fields = match val {
            wasmtime::component::Val::Record(f) => f,
            _ => return None,
        };

        let get_str = |name: &str| -> String {
            fields
                .iter()
                .find(|(k, _)| k == name)
                .and_then(|(_, v)| match v {
                    wasmtime::component::Val::String(s) => Some(s.to_string()),
                    _ => None,
                })
                .unwrap_or_default()
        };

        Some(ToolCall {
            id: get_str("id"),
            name: get_str("name"),
            arguments: get_str("arguments"),
        })
    }
}

#[async_trait]
impl Provider for WasmProvider {
    /// Execute a chat completion via the WASM plugin.
    ///
    /// Builds a message list from the system prompt and user message,
    /// delegates to the WASM `chat` export, and returns the text response.
    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let mut messages = Vec::new();
        if let Some(sys) = system_prompt {
            messages.push(ChatMessage::system(sys));
        }
        messages.push(ChatMessage::user(message));

        let result = tokio::time::timeout(
            std::time::Duration::from_millis(self.timeout_ms),
            self.call_chat_inner(&messages, model, temperature),
        )
        .await;

        match result {
            Err(_) => anyhow::bail!(
                "WASM provider '{}' timed out after {}ms",
                self.provider_name,
                self.timeout_ms
            ),
            Ok(Err(e)) => anyhow::bail!("{e}"),
            Ok(Ok(resp)) => Ok(resp.text.unwrap_or_default()),
        }
    }

    /// Pass the full message history to the WASM plugin.
    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(self.timeout_ms),
            self.call_chat_inner(messages, model, temperature),
        )
        .await;

        match result {
            Err(_) => anyhow::bail!(
                "WASM provider '{}' timed out after {}ms",
                self.provider_name,
                self.timeout_ms
            ),
            Ok(Err(e)) => anyhow::bail!("{e}"),
            Ok(Ok(resp)) => Ok(resp.text.unwrap_or_default()),
        }
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;
    use wasmtime::component::Val;

    fn str_val(s: &str) -> Val {
        Val::String(s.into())
    }

    // --- parse_tool_call ---

    #[test]
    fn parse_tool_call_valid() {
        let record = Val::Record(vec![
            ("id".to_string(), str_val("call-1")),
            ("name".to_string(), str_val("get_weather")),
            ("arguments".to_string(), str_val(r#"{"city":"NYC"}"#)),
        ]);
        let tc = WasmProvider::parse_tool_call(&record).expect("should parse tool call");
        assert_eq!(tc.id, "call-1");
        assert_eq!(tc.name, "get_weather");
        assert_eq!(tc.arguments, r#"{"city":"NYC"}"#);
    }

    #[test]
    fn parse_tool_call_missing_fields_defaults_to_empty() {
        let record = Val::Record(vec![
            ("name".to_string(), str_val("my_tool")),
            // id and arguments intentionally omitted — defaults to ""
        ]);
        let tc = WasmProvider::parse_tool_call(&record).expect("should parse with default empty strings");
        assert_eq!(tc.name, "my_tool");
        assert_eq!(tc.id, "");
        assert_eq!(tc.arguments, "");
    }

    #[test]
    fn parse_tool_call_not_a_record_returns_none() {
        let val = Val::Bool(true);
        assert!(WasmProvider::parse_tool_call(&val).is_none());
    }

    // --- parse_chat_response ---

    #[test]
    fn parse_chat_response_with_text() {
        let record = Val::Record(vec![
            (
                "text".to_string(),
                Val::Option(Some(Box::new(str_val("Hello from WASM!")))),
            ),
            ("tool-calls".to_string(), Val::List(vec![])),
        ]);
        let resp = WasmProvider::parse_chat_response(&record).expect("should parse");
        assert_eq!(resp.text, Some("Hello from WASM!".to_string()));
        assert!(resp.tool_calls.is_empty());
    }

    #[test]
    fn parse_chat_response_text_none() {
        let record = Val::Record(vec![
            ("text".to_string(), Val::Option(None)),
            ("tool-calls".to_string(), Val::List(vec![])),
        ]);
        let resp = WasmProvider::parse_chat_response(&record).expect("should parse");
        assert!(resp.text.is_none());
        assert!(resp.tool_calls.is_empty());
    }

    #[test]
    fn parse_chat_response_with_tool_calls() {
        let tool_call = Val::Record(vec![
            ("id".to_string(), str_val("tc-1")),
            ("name".to_string(), str_val("search")),
            ("arguments".to_string(), str_val(r#"{"q":"rust"}"#)),
        ]);
        let record = Val::Record(vec![
            ("text".to_string(), Val::Option(None)),
            ("tool-calls".to_string(), Val::List(vec![tool_call])),
        ]);
        let resp = WasmProvider::parse_chat_response(&record).expect("should parse");
        assert!(resp.text.is_none());
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "tc-1");
        assert_eq!(resp.tool_calls[0].name, "search");
        assert_eq!(resp.tool_calls[0].arguments, r#"{"q":"rust"}"#);
    }

    #[test]
    fn parse_chat_response_multiple_tool_calls() {
        let tc1 = Val::Record(vec![
            ("id".to_string(), str_val("tc-1")),
            ("name".to_string(), str_val("tool_a")),
            ("arguments".to_string(), str_val("{}")),
        ]);
        let tc2 = Val::Record(vec![
            ("id".to_string(), str_val("tc-2")),
            ("name".to_string(), str_val("tool_b")),
            ("arguments".to_string(), str_val("{}")),
        ]);
        let record = Val::Record(vec![
            ("text".to_string(), Val::Option(Some(Box::new(str_val("also text"))))),
            ("tool-calls".to_string(), Val::List(vec![tc1, tc2])),
        ]);
        let resp = WasmProvider::parse_chat_response(&record).expect("should parse");
        assert_eq!(resp.text, Some("also text".to_string()));
        assert_eq!(resp.tool_calls.len(), 2);
        assert_eq!(resp.tool_calls[0].name, "tool_a");
        assert_eq!(resp.tool_calls[1].name, "tool_b");
    }

    #[test]
    fn parse_chat_response_not_a_record_returns_error() {
        let val = Val::Bool(false);
        assert!(WasmProvider::parse_chat_response(&val).is_err());
    }

    #[test]
    fn parse_chat_response_missing_optional_fields() {
        // Only 'text' field — tool-calls field absent → defaults to empty vec
        let record = Val::Record(vec![("text".to_string(), Val::Option(Some(Box::new(str_val("hi")))))]);
        let resp = WasmProvider::parse_chat_response(&record).expect("should parse gracefully");
        assert_eq!(resp.text, Some("hi".to_string()));
        assert!(resp.tool_calls.is_empty());
    }
}
