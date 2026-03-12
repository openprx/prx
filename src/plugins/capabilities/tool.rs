//! Tool capability adapter — bridges WASM tool plugins to PRX's `Tool` trait.
//!
//! `WasmToolAdapter` wraps a WASM plugin that exports `get-spec` and `execute`,
//! presenting it as a native PRX `Tool` that the LLM can discover and invoke.

use async_trait::async_trait;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Mutex;
use wasmtime::AsContextMut;

use crate::plugins::error::PluginError;
use crate::plugins::host::HostState;
use crate::plugins::manifest::PluginManifest;
use crate::tools::traits::{Tool, ToolResult, ToolSpec};

/// A WASM plugin exposed as a PRX Tool.
///
/// Holds a wasmtime `Store<HostState>` and the component `Instance`.
/// All calls are serialized through a `Mutex` because wasmtime `Store`
/// is not `Sync` (single-owner semantics).
pub struct WasmToolAdapter {
    /// Cached tool spec (populated at load time).
    spec: ToolSpec,
    /// The wasmtime store + instance, behind a mutex for thread safety.
    inner: Arc<Mutex<WasmToolInner>>,
    /// Timeout for execute calls.
    timeout_ms: u64,
}

struct WasmToolInner {
    store: wasmtime::Store<HostState>,
    instance: wasmtime::component::Instance,
}

impl WasmToolAdapter {
    /// Create a new WasmToolAdapter by instantiating the WASM component.
    ///
    /// This creates a Store with HostState, links host functions,
    /// instantiates the component, and calls `get-spec` to cache
    /// the tool specification.
    pub async fn new(
        engine: &wasmtime::Engine,
        component: &wasmtime::component::Component,
        manifest: &PluginManifest,
    ) -> Result<Self, PluginError> {
        Self::new_with_memory(engine, component, manifest, None, None).await
    }

    /// Create a new WasmToolAdapter with an optional memory backend.
    pub async fn new_with_memory(
        engine: &wasmtime::Engine,
        component: &wasmtime::component::Component,
        manifest: &PluginManifest,
        memory: Option<Arc<dyn crate::memory::traits::Memory>>,
        event_bus: Option<Arc<crate::plugins::event_bus::EventBus>>,
    ) -> Result<Self, PluginError> {
        let timeout_ms = manifest.resources.max_execution_time_ms;

        // Build HostState from manifest
        let granted: HashSet<String> = manifest.permissions.required.iter().cloned().collect();
        let optional: HashSet<String> = manifest.permissions.optional.iter().cloned().collect();
        let mut host_state = HostState::new(
            manifest.plugin.name.clone(),
            manifest.config.clone(),
            granted,
            optional,
            manifest.permissions.http_allowlist.clone(),
            timeout_ms,
        );
        if let Some(mem) = memory {
            host_state = host_state.with_memory(mem);
        }
        if let Some(bus) = event_bus {
            host_state = host_state.with_event_bus(bus);
        }

        // Create store with fuel limit
        let mut store = wasmtime::Store::new(engine, host_state);
        store
            .set_fuel(manifest.resources.max_fuel)
            .map_err(|e| PluginError::Instantiation(format!("failed to set fuel: {e}")))?;

        // Create linker and register host functions
        let mut linker = wasmtime::component::Linker::<HostState>::new(engine);
        Self::register_host_functions(&mut linker)?;

        // Instantiate the component
        let instance = linker
            .instantiate_async(&mut store, component)
            .await
            .map_err(|e| {
                PluginError::Instantiation(format!("failed to instantiate component: {e}"))
            })?;

        // Call get-spec to cache the tool specification
        let spec = Self::call_get_spec(&instance, &mut store).await?;

        let inner = WasmToolInner { store, instance };

        Ok(Self {
            spec,
            inner: Arc::new(Mutex::new(inner)),
            timeout_ms,
        })
    }

    /// Register all host functions in the linker.
    fn register_host_functions(
        linker: &mut wasmtime::component::Linker<HostState>,
    ) -> Result<(), PluginError> {
        // Add WASI interfaces to the linker so WASM components that import
        // wasi:* (e.g. wasi:cli/environment) resolve correctly.
        wasmtime_wasi::add_to_linker_async(linker)
            .map_err(|e| PluginError::Instantiation(format!("WASI link: {e}")))?;

        // log and config are identical across all capability types — use the
        // shared helpers from `common` to avoid duplication.
        super::common::register_log_host_functions(linker)?;
        super::common::register_config_host_functions(linker)?;

        // prx:host/kv@0.1.0
        // Tool plugins use result<T, string> return types, which differ from the
        // simpler variants in middleware/hook/cron. We register kv manually here.
        let mut kv_inst = linker
            .instance("prx:host/kv@0.1.0")
            .map_err(|e| PluginError::Instantiation(format!("linker error (kv): {e}")))?;

        kv_inst
            .func_wrap_async(
                "get",
                |store: wasmtime::StoreContextMut<'_, HostState>, (key,): (String,)| {
                    Box::new(async move {
                        if let Err(e) = store.data().check_permission("kv") {
                            tracing::warn!("{e}");
                            return Ok((None::<Vec<u8>>,));
                        }
                        let kv = store.data().kv_store.clone();
                        let guard = kv.read().await;
                        let val = guard.get(&key).cloned();
                        Ok((val,))
                    })
                },
            )
            .map_err(|e| PluginError::Instantiation(format!("link kv.get: {e}")))?;

        kv_inst
            .func_wrap_async(
                "set",
                |store: wasmtime::StoreContextMut<'_, HostState>,
                 (key, value): (String, Vec<u8>)| {
                    Box::new(async move {
                        if let Err(e) = store.data().check_permission("kv") {
                            return Ok((Err::<(), String>(e),));
                        }
                        let kv = store.data().kv_store.clone();
                        let mut guard = kv.write().await;
                        guard.insert(key, value);
                        Ok((Ok::<(), String>(()),))
                    })
                },
            )
            .map_err(|e| PluginError::Instantiation(format!("link kv.set: {e}")))?;

        kv_inst
            .func_wrap_async(
                "delete",
                |store: wasmtime::StoreContextMut<'_, HostState>, (key,): (String,)| {
                    Box::new(async move {
                        if let Err(e) = store.data().check_permission("kv") {
                            return Ok((Err::<bool, String>(e),));
                        }
                        let kv = store.data().kv_store.clone();
                        let mut guard = kv.write().await;
                        let existed = guard.remove(&key).is_some();
                        Ok((Ok::<bool, String>(existed),))
                    })
                },
            )
            .map_err(|e| PluginError::Instantiation(format!("link kv.delete: {e}")))?;

        kv_inst
            .func_wrap_async(
                "list-keys",
                |store: wasmtime::StoreContextMut<'_, HostState>, (prefix,): (String,)| {
                    Box::new(async move {
                        if let Err(e) = store.data().check_permission("kv") {
                            tracing::warn!("{e}");
                            return Ok((vec![],));
                        }
                        let kv = store.data().kv_store.clone();
                        let guard = kv.read().await;
                        let keys: Vec<String> = guard
                            .keys()
                            .filter(|k| k.starts_with(&prefix))
                            .cloned()
                            .collect();
                        Ok((keys,))
                    })
                },
            )
            .map_err(|e| PluginError::Instantiation(format!("link kv.list-keys: {e}")))?;

        // prx:host/http-outbound@0.1.0
        let mut http_inst = linker
            .instance("prx:host/http-outbound@0.1.0")
            .map_err(|e| PluginError::Instantiation(format!("linker error (http): {e}")))?;

        http_inst
            .func_wrap_async(
                "request",
                |store: wasmtime::StoreContextMut<'_, HostState>,
                 (method, url, headers, body): (
                    String,
                    String,
                    Vec<(String, String)>,
                    Option<Vec<u8>>,
                )| {
                    Box::new(async move {
                        if let Err(e) = store.data().check_permission("http-outbound") {
                            return Ok((Err::<(u16, Vec<(String, String)>, Vec<u8>), String>(e),));
                        }
                        if !store.data().check_url_allowed(&url) {
                            return Ok((Err(format!("URL not in allowlist: {url}")),));
                        }

                        let client = reqwest::Client::new();
                        let mut req = match method.to_uppercase().as_str() {
                            "GET" => client.get(&url),
                            "POST" => client.post(&url),
                            "PUT" => client.put(&url),
                            "DELETE" => client.delete(&url),
                            "PATCH" => client.patch(&url),
                            "HEAD" => client.head(&url),
                            _ => return Ok((Err(format!("unsupported method: {method}")),)),
                        };

                        for (k, v) in &headers {
                            req = req.header(k.as_str(), v.as_str());
                        }

                        if let Some(b) = body {
                            req = req.body(b);
                        }

                        match req.send().await {
                            Ok(resp) => {
                                let status = resp.status().as_u16();
                                let resp_headers: Vec<(String, String)> = resp
                                    .headers()
                                    .iter()
                                    .map(|(k, v)| {
                                        (k.to_string(), v.to_str().unwrap_or("").to_string())
                                    })
                                    .collect();
                                match resp.bytes().await {
                                    Ok(bytes) => Ok((Ok((status, resp_headers, bytes.to_vec())),)),
                                    Err(e) => Ok((Err(format!("body read error: {e}")),)),
                                }
                            }
                            Err(e) => Ok((Err(format!("request failed: {e}")),)),
                        }
                    })
                },
            )
            .map_err(|e| PluginError::Instantiation(format!("link http.request: {e}")))?;

        super::common::register_websocket_host_functions(linker)?;

        // prx:host/memory@0.1.0 — connected to real memory backend
        let mut mem_inst = linker
            .instance("prx:host/memory@0.1.0")
            .map_err(|e| PluginError::Instantiation(format!("linker error (memory): {e}")))?;

        mem_inst
            .func_wrap_async(
                "store",
                |store: wasmtime::StoreContextMut<'_, HostState>,
                 (text, category): (String, String)| {
                    Box::new(async move {
                        if let Err(e) = store.data().check_permission("memory") {
                            return Ok((Err::<String, String>(e),));
                        }
                        let mem = match &store.data().memory {
                            Some(m) => Arc::clone(m),
                            None => {
                                return Ok((Err::<String, String>(
                                    "memory backend not configured".to_string(),
                                ),));
                            }
                        };
                        let plugin_name = store.data().plugin_name.clone();
                        let key = format!("plugin:{plugin_name}:{}", uuid::Uuid::new_v4());
                        let cat = match category.as_str() {
                            "core" => crate::memory::traits::MemoryCategory::Core,
                            "daily" => crate::memory::traits::MemoryCategory::Daily,
                            "conversation" => crate::memory::traits::MemoryCategory::Conversation,
                            other => {
                                crate::memory::traits::MemoryCategory::Custom(other.to_string())
                            }
                        };
                        match mem.store(&key, &text, cat, None).await {
                            Ok(()) => Ok((Ok::<String, String>(key),)),
                            Err(e) => Ok((Err(format!("memory store failed: {e}")),)),
                        }
                    })
                },
            )
            .map_err(|e| PluginError::Instantiation(format!("link memory.store: {e}")))?;

        mem_inst
            .func_wrap_async(
                "recall",
                |store: wasmtime::StoreContextMut<'_, HostState>, (query, limit): (String, u32)| {
                    Box::new(async move {
                        if let Err(e) = store.data().check_permission("memory") {
                            return Ok((Err::<Vec<(String, String, String, f64)>, String>(e),));
                        }
                        let mem = match &store.data().memory {
                            Some(m) => Arc::clone(m),
                            None => {
                                return Ok((Err::<Vec<(String, String, String, f64)>, String>(
                                    "memory backend not configured".to_string(),
                                ),));
                            }
                        };
                        match mem.recall(&query, limit as usize, None).await {
                            Ok(entries) => {
                                let results: Vec<(String, String, String, f64)> = entries
                                    .into_iter()
                                    .map(|e| {
                                        (
                                            e.id,
                                            e.content,
                                            e.category.to_string(),
                                            e.score.unwrap_or(0.0),
                                        )
                                    })
                                    .collect();
                                Ok((Ok(results),))
                            }
                            Err(e) => Ok((Err(format!("memory recall failed: {e}")),)),
                        }
                    })
                },
            )
            .map_err(|e| PluginError::Instantiation(format!("link memory.recall: {e}")))?;

        // prx:host/events@0.1.0
        super::common::register_event_host_functions(linker)?;

        Ok(())
    }

    /// Call the guest's `get-spec` export to obtain the tool specification.
    ///
    /// In the Component Model, exports are navigated via `get_export` to find
    /// the interface instance, then `get_typed_func` to get individual functions.
    async fn call_get_spec(
        instance: &wasmtime::component::Instance,
        store: &mut wasmtime::Store<HostState>,
    ) -> Result<ToolSpec, PluginError> {
        // Navigate to the exported interface with compatibility fallback.
        let iface_name_candidates = ["prx:plugin/tool-exports@0.1.0", "prx:plugin/tool-exports"];
        let (_iface_name, iface_idx) = iface_name_candidates
            .iter()
            .find_map(|name| {
                instance
                    .get_export(store.as_context_mut(), None, name)
                    .map(|idx| (*name, idx))
            })
            .ok_or_else(|| {
                PluginError::Instantiation(format!(
                    "plugin does not export any supported tool interface: {}",
                    iface_name_candidates.join(", ")
                ))
            })?;

        // Get the get-spec function from within that interface
        let func_idx = instance
            .get_export(store.as_context_mut(), Some(&iface_idx), "get-spec")
            .ok_or_else(|| {
                PluginError::Instantiation("get-spec not found in tool-exports".to_string())
            })?;

        let get_spec_fn = instance
            .get_func(store.as_context_mut(), &func_idx)
            .ok_or_else(|| PluginError::Instantiation("get-spec is not a function".to_string()))?;

        // Call it using the untyped Func::call_async API for maximum compatibility
        // get-spec returns a single record value (the tool-spec)
        let mut results = vec![wasmtime::component::Val::Bool(false); 1];
        get_spec_fn
            .call_async(store.as_context_mut(), &[], &mut results)
            .await
            .map_err(|e| PluginError::Runtime(format!("get-spec call failed: {e}")))?;

        // The return type is a record (tool-spec) with fields: name, description, parameters-schema
        // Component Model returns records as a single Record value
        let spec_record = &results[0];
        let (name, description, params_schema) = match spec_record {
            wasmtime::component::Val::Record(fields) => {
                let name = fields
                    .iter()
                    .find(|(k, _)| k == "name")
                    .and_then(|(_, v)| match v {
                        wasmtime::component::Val::String(s) => Some(s.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| "unknown".into());
                let desc = fields
                    .iter()
                    .find(|(k, _)| k == "description")
                    .and_then(|(_, v)| match v {
                        wasmtime::component::Val::String(s) => Some(s.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                let schema = fields
                    .iter()
                    .find(|(k, _)| k == "parameters-schema")
                    .and_then(|(_, v)| match v {
                        wasmtime::component::Val::String(s) => Some(s.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| r#"{"type":"object"}"#.into());
                (name, desc, schema)
            }
            _ => {
                return Err(PluginError::Runtime(
                    "get-spec returned unexpected value type".to_string(),
                ));
            }
        };

        // Post-return cleanup
        get_spec_fn
            .post_return_async(store.as_context_mut())
            .await
            .map_err(|e| PluginError::Runtime(format!("get-spec post_return failed: {e}")))?;

        let parameters: serde_json::Value = serde_json::from_str(&params_schema)
            .unwrap_or_else(|_| serde_json::json!({"type": "object", "properties": {}}));

        Ok(ToolSpec {
            name: name.to_string(),
            description: description.to_string(),
            parameters,
        })
    }
}

#[async_trait]
impl Tool for WasmToolAdapter {
    fn name(&self) -> &str {
        &self.spec.name
    }

    fn description(&self) -> &str {
        &self.spec.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.spec.parameters.clone()
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let args_str = serde_json::to_string(&args)?;

        let result = tokio::time::timeout(
            std::time::Duration::from_millis(self.timeout_ms),
            self.execute_inner(&args_str),
        )
        .await;

        match result {
            Ok(Ok(tool_result)) => Ok(tool_result),
            Ok(Err(e)) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("WASM plugin error: {e}")),
            }),
            Err(_) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("WASM plugin timed out after {}ms", self.timeout_ms)),
            }),
        }
    }
}

impl WasmToolAdapter {
    async fn execute_inner(&self, args_str: &str) -> Result<ToolResult, PluginError> {
        let mut inner = self.inner.lock().await;
        let WasmToolInner {
            ref mut store,
            ref instance,
        } = *inner;

        // Navigate to execute function with compatibility fallback.
        let iface_name_candidates = ["prx:plugin/tool-exports@0.1.0", "prx:plugin/tool-exports"];
        let (_iface_name, iface_idx) = iface_name_candidates
            .iter()
            .find_map(|name| {
                instance
                    .get_export(store.as_context_mut(), None, name)
                    .map(|idx| (*name, idx))
            })
            .ok_or_else(|| {
                PluginError::Runtime(format!(
                    "plugin does not export any supported tool interface: {}",
                    iface_name_candidates.join(", ")
                ))
            })?;

        let func_idx = instance
            .get_export(store.as_context_mut(), Some(&iface_idx), "execute")
            .ok_or_else(|| PluginError::Runtime("execute not found in tool-exports".to_string()))?;

        let execute_fn = instance
            .get_func(store.as_context_mut(), &func_idx)
            .ok_or_else(|| PluginError::Runtime("execute is not a function".to_string()))?;

        // Call execute(args: string) -> plugin-result
        let args_val = wasmtime::component::Val::String(args_str.into());
        let mut results = vec![wasmtime::component::Val::Bool(false)];
        execute_fn
            .call_async(store.as_context_mut(), &[args_val], &mut results)
            .await
            .map_err(|e| PluginError::Runtime(format!("execute call failed: {e}")))?;

        // Parse the plugin-result record
        let (success, output, error) = match &results[0] {
            wasmtime::component::Val::Record(fields) => {
                let success = fields
                    .iter()
                    .find(|(k, _)| k == "success")
                    .and_then(|(_, v)| match v {
                        wasmtime::component::Val::Bool(b) => Some(*b),
                        _ => None,
                    })
                    .unwrap_or(false);
                let output = fields
                    .iter()
                    .find(|(k, _)| k == "output")
                    .and_then(|(_, v)| match v {
                        wasmtime::component::Val::String(s) => Some(s.to_string()),
                        _ => None,
                    })
                    .unwrap_or_default();
                let error = fields
                    .iter()
                    .find(|(k, _)| k == "error")
                    .and_then(|(_, v)| match v {
                        wasmtime::component::Val::Option(opt) => match opt.as_deref() {
                            Some(wasmtime::component::Val::String(s)) => Some(Some(s.to_string())),
                            _ => Some(None),
                        },
                        _ => None,
                    })
                    .flatten();
                (success, output, error)
            }
            _ => {
                return Err(PluginError::Runtime(
                    "execute returned unexpected value type".to_string(),
                ));
            }
        };

        // Post-return cleanup
        execute_fn
            .post_return_async(store.as_context_mut())
            .await
            .map_err(|e| PluginError::Runtime(format!("execute post_return failed: {e}")))?;

        Ok(ToolResult {
            success,
            output,
            error,
        })
    }
}
