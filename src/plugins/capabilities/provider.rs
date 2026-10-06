//! Provider capability — bridges WASM provider plugins to the PRX `Provider` trait.
//!
//! WASM provider plugins export a `chat` function that receives a list of messages
//! and returns a chat response. The host delegates LLM requests to the plugin
//! when the plugin's declared provider name matches the routing configuration.
//!
//! Streaming is intentionally not supported for WASM providers (WASM components
//! cannot hold open an async stream to the host).
//!
//! # Trap recovery
//!
//! The adapter's store is owned by a [`Supervisor`]: a trapped provider is
//! rebuilt on a brand-new `Store` by the next request, rebuilds are budgeted
//! inside a rolling window, and once the budget is spent the provider is
//! quarantined until the next plugin generation replaces it. Every failure —
//! a trap, a timeout, a quarantine refusal, a request too large for the guest —
//! is returned as an error for the caller's reliability and fallback layer to
//! act on; the adapter never fabricates or truncates a response.

use async_trait::async_trait;
use std::collections::HashSet;
use std::sync::Arc;
use wasmtime::AsContextMut;

use super::recovery::{CallOutcome, CapabilityKind, GuestOp, Respawn, Supervisor};
use crate::plugins::error::{PluginError, PluginResult};
use crate::plugins::host::{HostState, apply_store_epoch_deadline};
use crate::plugins::manifest::PluginManifest;
use crate::providers::traits::{ChatMessage, ChatResponse, Provider, ToolCall};

/// A loaded WASM provider plugin instance.
#[derive(Clone)]
pub struct WasmProvider {
    /// The cached provider name (returned by `name()` at load time).
    provider_name: String,
    /// The store, its instance and their recovery state.
    supervisor: Arc<Supervisor>,
    /// Timeout for `chat` calls (milliseconds).
    timeout_ms: u64,
}

/// One call of the guest's `chat` export.
struct ChatCall {
    provider_name: String,
    messages: Vec<ChatMessage>,
    model: String,
    temperature: f64,
}

impl GuestOp for ChatCall {
    type Output = ChatResponse;

    fn operation(&self) -> &'static str {
        "chat"
    }

    fn invoke<'a>(
        self,
        store: &'a mut wasmtime::Store<HostState>,
        instance: &'a wasmtime::component::Instance,
    ) -> futures_util::future::BoxFuture<'a, CallOutcome<ChatResponse>>
    where
        Self: 'a,
    {
        Box::pin(self.run(store, instance))
    }
}

impl ChatCall {
    async fn run(
        self,
        store: &mut wasmtime::Store<HostState>,
        instance: &wasmtime::component::Instance,
    ) -> CallOutcome<ChatResponse> {
        let provider_name = self.provider_name;
        // Navigate to prx:plugin/provider-exports@0.1.0 → chat
        let Some(iface_idx) =
            instance.get_export_index(store.as_context_mut(), None, "prx:plugin/provider-exports@0.1.0")
        else {
            return CallOutcome::GuestError(PluginError::Runtime(
                "plugin does not export prx:plugin/provider-exports@0.1.0".to_string(),
            ));
        };

        let Some(func_idx) = instance.get_export_index(store.as_context_mut(), Some(&iface_idx), "chat") else {
            return CallOutcome::GuestError(PluginError::Runtime("chat not found in provider-exports".to_string()));
        };

        let Some(chat_fn) = instance.get_func(store.as_context_mut(), &func_idx) else {
            return CallOutcome::GuestError(PluginError::Runtime("chat is not a function".to_string()));
        };

        // Build the list<chat-message> parameter.
        let wasm_messages: Vec<wasmtime::component::Val> = self
            .messages
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
            wasmtime::component::Val::String(self.model.as_str().into()),
            wasmtime::component::Val::Float64(self.temperature),
        ];
        let mut results = [wasmtime::component::Val::Bool(false)];

        if let Err(e) = chat_fn.call_async(store.as_context_mut(), &params, &mut results).await {
            return super::recovery::unwound(store, &e, || format!("chat() call failed: {e}"));
        }

        // Parse result<chat-response, string>
        let parsed = match results.first() {
            Some(wasmtime::component::Val::Result(r)) => match r.as_ref() {
                Ok(Some(inner_val)) => WasmProvider::parse_chat_response(inner_val),
                Ok(None) => Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                }),
                Err(Some(inner_err)) => match inner_err.as_ref() {
                    wasmtime::component::Val::String(e) => Err(PluginError::Runtime(format!(
                        "WASM provider '{provider_name}' returned error: {e}"
                    ))),
                    _ => Err(PluginError::Runtime(format!(
                        "WASM provider '{provider_name}' returned unknown error variant"
                    ))),
                },
                Err(None) => Err(PluginError::Runtime(format!(
                    "WASM provider '{provider_name}' returned unknown error"
                ))),
            },
            _ => Err(PluginError::Runtime(
                "chat() returned unexpected value type".to_string(),
            )),
        };
        match parsed {
            Ok(response) => CallOutcome::Completed(response),
            Err(error) => CallOutcome::GuestError(error),
        }
    }
}

/// Bytes of a chat request the guest has to hold: every role and content
/// string plus the model name.
fn chat_payload_bytes(messages: &[ChatMessage], model: &str) -> usize {
    messages.iter().fold(model.len(), |total, message| {
        total
            .saturating_add(message.role.len())
            .saturating_add(message.content.len())
    })
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
        let respawn = Respawn::new(
            CapabilityKind::Provider,
            engine,
            component,
            manifest,
            granted_permissions,
            None,
            event_bus,
            Self::register_host_functions,
        );
        let mut live = respawn.instantiate().await?;

        // Cache the provider name at load time. The probe gets its own full
        // fuel budget, like every later call.
        apply_store_epoch_deadline(&mut live.store, timeout_ms);
        live.store
            .set_fuel(manifest.resources.max_fuel)
            .map_err(|e| PluginError::Instantiation(format!("failed to set fuel: {e}")))?;
        let provider_name = Self::call_name(&live.instance, &mut live.store).await?;

        tracing::info!(
            plugin = %manifest.plugin.name,
            provider = %provider_name,
            "WASM provider registered"
        );

        Ok(Self {
            provider_name,
            supervisor: Arc::new(Supervisor::new(respawn, live).with_bounded_lock_wait()),
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
        super::common::register_common_host_functions(linker)
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

    /// Run one chat request through the supervisor. Every failure is an
    /// error for the caller's reliability layer; nothing is fabricated or
    /// truncated.
    async fn call_chat(&self, messages: &[ChatMessage], model: &str, temperature: f64) -> anyhow::Result<ChatResponse> {
        let payload_bytes = chat_payload_bytes(messages, model);
        if !self.supervisor.admit_payload("chat", payload_bytes) {
            return Err(anyhow::Error::from(PluginError::ResourceLimit(format!(
                "WASM provider '{}' request of {payload_bytes} bytes exceeds the {}-byte payload cap \
                     (resources.max_payload_bytes)",
                self.provider_name,
                self.supervisor.max_payload_bytes()
            ))));
        }
        // Detached: a caller that gives up — a stopped turn, a caller-side
        // deadline shorter than the plugin's — cannot drop the request
        // mid-guest-call. A guest that hangs is charged by its own timeout.
        match self
            .supervisor
            .call_detached(ChatCall {
                provider_name: self.provider_name.clone(),
                messages: messages.to_vec(),
                model: model.to_string(),
                temperature,
            })
            .await
        {
            Ok(response) => Ok(response),
            // A guest call that ran past its budget and a request that could
            // not get the store within that budget read the same, as they
            // always have; only the first is charged to the plugin.
            Err(error @ PluginError::Timeout(_)) => Err(anyhow::Error::from(error).context(format!(
                "WASM provider '{}' timed out after {}ms",
                self.provider_name, self.timeout_ms
            ))),
            // Keep the type, so callers such as the reliability layer can
            // tell a quarantine or a refused payload apart without parsing text.
            Err(error) => Err(error.into()),
        }
    }

    /// Plugin that exports this provider.
    pub fn plugin_name(&self) -> &str {
        self.supervisor.plugin_name()
    }

    /// Per-adapter health for `wasm_plugins_status`.
    pub fn diagnostics(&self) -> serde_json::Value {
        self.supervisor.diagnostics(serde_json::json!({
            "plugin": self.supervisor.plugin_name(),
            "provider": self.provider_name,
        }))
    }

    pub fn is_quarantined(&self) -> bool {
        self.supervisor.is_quarantined()
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
    fn supports_native_tools(&self) -> bool {
        true
    }

    async fn chat(
        &self,
        request: crate::providers::traits::ChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        self.call_chat(request.messages, model, temperature).await
    }

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

        let response = self.call_chat(&messages, model, temperature).await?;
        Ok(response.text.unwrap_or_default())
    }

    /// Pass the full message history to the WASM plugin.
    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let response = self.call_chat(messages, model, temperature).await?;
        Ok(response.text.unwrap_or_default())
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

    const TRAPPING_ECHO_MANIFEST: &str = r#"
[plugin]
name = "echo-provider"
version = "0.1.0"
wasm = "plugin.wasm"

[[capabilities]]
type = "provider"
name = "wasm-echo"

[permissions]
required = []
optional = []

[resources]
max_fuel = 1000000000
max_memory_mb = 8
max_execution_time_ms = 30000
max_payload_bytes = 8388608
"#;

    /// D-023: a caller whose deadline is shorter than the plugin's (the 5 s
    /// pre-gate classifier, a stopped turn) gives up on a request. The request
    /// still runs to its own outcome, so a guest that traps or hangs is charged
    /// for it by its own failure or timeout — not excused as the caller's
    /// cancellation.
    #[tokio::test]
    async fn an_abandoned_provider_request_is_still_charged_for_its_own_failure() {
        use crate::providers::traits::ChatRequest;
        use std::future::Future;
        use std::task::{Context, Waker};

        let engine = crate::plugins::shared_wasm_engine().expect("test: shared wasmtime engine");
        let wasm = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("pdk/rust/examples/echo-provider/plugin.wasm");
        let bytes = std::fs::read(wasm).expect("test: the committed provider fixture is readable");
        let component =
            wasmtime::component::Component::from_binary(&engine, &bytes).expect("test: the fixture is a component");
        let manifest: PluginManifest = toml::from_str(TRAPPING_ECHO_MANIFEST).expect("test: manifest parses");
        let provider = WasmProvider::new(&engine, &component, &manifest, HashSet::new(), None)
            .await
            .expect("test: the fixture instantiates");

        // Four megabytes of message trap an 8 MB guest.
        let trapping = [ChatMessage::user("x".repeat(4 * 1024 * 1024))];
        {
            let guard = provider.supervisor.lock().await;
            {
                let request = ChatRequest {
                    messages: &trapping,
                    tools: None,
                };
                let mut chat = std::pin::pin!(provider.chat(request, "fixture-model", 0.0));
                assert!(
                    chat.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending(),
                    "the request must be waiting for the store"
                );
                for _ in 0..8 {
                    tokio::task::yield_now().await;
                }
            }
            drop(guard);
        }

        let reply = provider
            .chat_with_system(None, "probe", "fixture-model", 0.0)
            .await
            .expect("test: the next request runs on a rebuilt store");
        assert_eq!(reply, "wasm-echo/fixture-model: probe");
        let diagnostics = provider.diagnostics();
        assert_eq!(
            diagnostics.get("failure_count").and_then(serde_json::Value::as_u64),
            Some(1),
            "the abandoned request must have run and been charged for its trap: {diagnostics}"
        );
        assert_eq!(
            diagnostics.get("restart_count").and_then(serde_json::Value::as_u64),
            Some(1),
            "{diagnostics}"
        );
    }
}
