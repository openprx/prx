//! Middleware capability — bridges WASM middleware plugins to the message pipeline.
//!
//! Middleware plugins can intercept and transform data at four pipeline stages:
//! - `inbound`: after receiving a ChannelMessage
//! - `outbound`: before sending a response
//! - `llm_request`: before sending to the LLM (can modify messages/tools)
//! - `llm_response`: after receiving from the LLM (can post-process)

use std::sync::Arc;
use tokio::sync::Mutex;
use wasmtime::AsContextMut;

use crate::plugins::error::{PluginError, PluginResult};
use crate::plugins::host::HostState;
use crate::plugins::manifest::PluginManifest;

/// Pipeline stages where middleware can intercept data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MiddlewareStage {
    Inbound,
    Outbound,
    LlmRequest,
    LlmResponse,
}

impl MiddlewareStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inbound => "inbound",
            Self::Outbound => "outbound",
            Self::LlmRequest => "llm_request",
            Self::LlmResponse => "llm_response",
        }
    }
}

/// A loaded middleware plugin instance.
pub struct WasmMiddleware {
    plugin_name: String,
    priority: i32,
    inner: Arc<Mutex<WasmMiddlewareInner>>,
    timeout_ms: u64,
}

struct WasmMiddlewareInner {
    store: wasmtime::Store<HostState>,
    instance: wasmtime::component::Instance,
}

impl WasmMiddleware {
    /// Create a new middleware adapter from a compiled WASM component.
    pub async fn new(
        engine: &wasmtime::Engine,
        component: &wasmtime::component::Component,
        manifest: &PluginManifest,
        priority: i32,
    ) -> PluginResult<Self> {
        let timeout_ms = manifest.resources.max_execution_time_ms;

        let granted = manifest.permissions.required.iter().cloned().collect();
        let optional = manifest.permissions.optional.iter().cloned().collect();
        let host_state = HostState::new(
            manifest.plugin.name.clone(),
            manifest.config.clone(),
            granted,
            optional,
            manifest.permissions.http_allowlist.clone(),
            timeout_ms,
        );

        let mut store = wasmtime::Store::new(engine, host_state);
        store.set_fuel(manifest.resources.max_fuel).map_err(|e| {
            PluginError::Instantiation(format!("failed to set fuel: {e}"))
        })?;

        let mut linker = wasmtime::component::Linker::<HostState>::new(engine);
        Self::register_host_functions(&mut linker)?;

        let instance = linker
            .instantiate_async(&mut store, component)
            .await
            .map_err(|e| {
                PluginError::Instantiation(format!("failed to instantiate middleware: {e}"))
            })?;

        Ok(Self {
            plugin_name: manifest.plugin.name.clone(),
            priority,
            inner: Arc::new(Mutex::new(WasmMiddlewareInner { store, instance })),
            timeout_ms,
        })
    }

    /// Process data at a specific pipeline stage.
    pub async fn process(&self, stage: MiddlewareStage, data_json: &str) -> PluginResult<String> {
        let mut inner = self.inner.lock().await;
        let WasmMiddlewareInner { store, instance } = &mut *inner;

        // Navigate to the exported interface: prx:plugin/middleware-exports@0.1.0
        let iface_idx = instance
            .get_export(
                store.as_context_mut(),
                None,
                "prx:plugin/middleware-exports@0.1.0",
            )
            .ok_or_else(|| {
                PluginError::Runtime(
                    "plugin does not export prx:plugin/middleware-exports@0.1.0".to_string(),
                )
            })?;

        let func_idx = instance
            .get_export(store.as_context_mut(), Some(&iface_idx), "process")
            .ok_or_else(|| {
                PluginError::Runtime("process not found in middleware-exports".to_string())
            })?;

        let func = instance
            .get_func(store.as_context_mut(), &func_idx)
            .ok_or_else(|| {
                PluginError::Runtime("process is not a function".to_string())
            })?;

        // Call with (stage, data_json) -> result<string, string>
        let stage_str = stage.as_str();
        let params = [
            wasmtime::component::Val::String(stage_str.into()),
            wasmtime::component::Val::String(data_json.into()),
        ];
        let mut results = [wasmtime::component::Val::Bool(false)];

        let call_result = tokio::time::timeout(
            std::time::Duration::from_millis(self.timeout_ms),
            func.call_async(&mut *store, &params, &mut results),
        )
        .await;

        match call_result {
            Err(_) => Err(PluginError::Timeout(self.timeout_ms)),
            Ok(Err(e)) => Err(PluginError::Runtime(format!(
                "middleware '{}' process error: {e}",
                self.plugin_name
            ))),
            Ok(Ok(())) => {
                func.post_return_async(&mut *store).await.ok();
                // Parse the result variant
                match &results[0] {
                    wasmtime::component::Val::Result(r) => match r.as_ref() {
                        Ok(Some(b)) => match b.as_ref() {
                            wasmtime::component::Val::String(s) => Ok(s.to_string()),
                            _ => Ok(data_json.to_string()),
                        },
                        Err(Some(b)) => match b.as_ref() {
                            wasmtime::component::Val::String(e) => {
                                Err(PluginError::Runtime(format!(
                                    "middleware '{}' returned error: {e}",
                                    self.plugin_name
                                )))
                            }
                            _ => Ok(data_json.to_string()),
                        },
                        _ => Ok(data_json.to_string()),
                    },
                    _ => Ok(data_json.to_string()),
                }
            }
        }
    }

    pub fn plugin_name(&self) -> &str {
        &self.plugin_name
    }

    pub fn priority(&self) -> i32 {
        self.priority
    }

    /// Register host functions for middleware plugins.
    fn register_host_functions(
        linker: &mut wasmtime::component::Linker<HostState>,
    ) -> PluginResult<()> {
        super::common::register_common_host_functions(linker)
    }
}

/// Middleware chain that manages multiple middleware plugins sorted by priority.
pub struct MiddlewareChain {
    middlewares: Vec<WasmMiddleware>,
}

impl MiddlewareChain {
    /// Create a new empty middleware chain.
    pub fn new() -> Self {
        Self {
            middlewares: Vec::new(),
        }
    }

    /// Add a middleware, maintaining priority order.
    pub fn add(&mut self, mw: WasmMiddleware) {
        self.middlewares.push(mw);
        self.middlewares.sort_by_key(|m| m.priority());
    }

    /// Process data through all middlewares for a given stage.
    pub async fn process(&self, stage: MiddlewareStage, data_json: &str) -> String {
        let mut data = data_json.to_string();
        for mw in &self.middlewares {
            match mw.process(stage, &data).await {
                Ok(transformed) => data = transformed,
                Err(e) => {
                    tracing::warn!(
                        plugin = %mw.plugin_name(),
                        stage = %stage.as_str(),
                        error = %e,
                        "middleware processing failed, passing data through unchanged"
                    );
                }
            }
        }
        data
    }

    /// Returns true if the chain has no middlewares.
    pub fn is_empty(&self) -> bool {
        self.middlewares.is_empty()
    }

    /// Number of middlewares in the chain.
    pub fn len(&self) -> usize {
        self.middlewares.len()
    }
}
