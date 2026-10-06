//! Tool capability adapter — bridges WASM tool plugins to PRX's `Tool` trait.
//!
//! `WasmToolAdapter` wraps a WASM plugin that exports `get-spec` and `execute`,
//! presenting it as a native PRX `Tool` that the LLM can discover and invoke.
//!
//! # Trap recovery
//!
//! The adapter's store is owned by a [`Supervisor`]: a trapped tool is rebuilt
//! on a brand-new `Store` by the next call, rebuilds are budgeted inside a
//! rolling window, and once the budget is spent the tool is quarantined until
//! the next plugin generation replaces it. A tool call that cannot run —
//! trapped, timed out, refused while quarantined, or carrying arguments the
//! guest cannot hold — always comes back as a failed `ToolResult`, never as a
//! success the guest did not produce. A quarantined tool stays listed so the
//! model is told why it cannot be used.

use async_trait::async_trait;
use std::collections::HashSet;
use std::sync::Arc;
use wasmtime::AsContextMut;

use super::recovery::{CallOutcome, CapabilityKind, GuestOp, Respawn, Supervisor};
use crate::plugins::error::PluginError;
use crate::plugins::host::{HostState, apply_store_epoch_deadline};
use crate::plugins::manifest::PluginManifest;
use crate::tools::traits::{Tool, ToolResult, ToolSpec};

const TOOL_INTERFACES: [&str; 2] = ["prx:plugin/tool-exports@0.1.0", "prx:plugin/tool-exports"];

/// A WASM plugin exposed as a PRX Tool.
///
/// The wasmtime `Store<HostState>` and the component `Instance` live inside a
/// [`Supervisor`], which serializes calls because a `Store` is not `Sync`.
pub struct WasmToolAdapter {
    /// Cached tool spec (populated at load time).
    spec: ToolSpec,
    /// The store, its instance and their recovery state.
    supervisor: Arc<Supervisor>,
    /// Timeout for execute calls.
    timeout_ms: u64,
}

/// One call of the guest's `execute` export.
struct ExecuteCall {
    args: String,
}

impl GuestOp for ExecuteCall {
    type Output = ToolResult;

    fn operation(&self) -> &'static str {
        "execute"
    }

    fn invoke<'a>(
        self,
        store: &'a mut wasmtime::Store<HostState>,
        instance: &'a wasmtime::component::Instance,
    ) -> futures_util::future::BoxFuture<'a, CallOutcome<ToolResult>>
    where
        Self: 'a,
    {
        Box::pin(self.run(store, instance))
    }
}

impl ExecuteCall {
    async fn run(
        self,
        store: &mut wasmtime::Store<HostState>,
        instance: &wasmtime::component::Instance,
    ) -> CallOutcome<ToolResult> {
        // Navigate to execute function with compatibility fallback.
        let Some(iface_idx) = TOOL_INTERFACES
            .iter()
            .find_map(|name| instance.get_export_index(store.as_context_mut(), None, name))
        else {
            return CallOutcome::GuestError(PluginError::Runtime(format!(
                "plugin does not export any supported tool interface: {}",
                TOOL_INTERFACES.join(", ")
            )));
        };

        let Some(func_idx) = instance.get_export_index(store.as_context_mut(), Some(&iface_idx), "execute") else {
            return CallOutcome::GuestError(PluginError::Runtime("execute not found in tool-exports".to_string()));
        };

        let Some(execute_fn) = instance.get_func(store.as_context_mut(), &func_idx) else {
            return CallOutcome::GuestError(PluginError::Runtime("execute is not a function".to_string()));
        };

        // Call execute(args: string) -> plugin-result
        let args_val = wasmtime::component::Val::String(self.args.as_str().into());
        let mut results = [wasmtime::component::Val::Bool(false)];
        if let Err(e) = execute_fn
            .call_async(store.as_context_mut(), &[args_val], &mut results)
            .await
        {
            return super::recovery::unwound(store, &e, || format!("execute call failed: {e}"));
        }

        // Parse the plugin-result record. A guest that reports
        // `success: false` completed normally: that is its answer, not a failure
        // of the adapter.
        match results.first() {
            Some(wasmtime::component::Val::Record(fields)) => CallOutcome::Completed(parse_plugin_result(fields)),
            _ => CallOutcome::GuestError(PluginError::Runtime(
                "execute returned unexpected value type".to_string(),
            )),
        }
    }
}

fn parse_plugin_result(fields: &[(String, wasmtime::component::Val)]) -> ToolResult {
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
    ToolResult { success, output, error }
}

// SAFETY: All results[N] accesses index a Vec that was just constructed with
// the correct number of elements (e.g. `vec![Val::Bool(false); 1]`),
// so the index is always valid.
#[allow(clippy::indexing_slicing)]
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
        granted_permissions: HashSet<String>,
    ) -> Result<Self, PluginError> {
        Self::new_with_memory(engine, component, manifest, granted_permissions, None, None).await
    }

    /// Create a new WasmToolAdapter with an optional memory backend.
    ///
    /// `granted_permissions` must come from `LoadedPlugin.granted_permissions`
    /// (policy-filtered), NOT directly from the manifest.
    pub async fn new_with_memory(
        engine: &wasmtime::Engine,
        component: &wasmtime::component::Component,
        manifest: &PluginManifest,
        granted_permissions: HashSet<String>,
        memory: Option<Arc<dyn crate::memory::traits::Memory>>,
        event_bus: Option<Arc<crate::plugins::event_bus::EventBus>>,
    ) -> Result<Self, PluginError> {
        let timeout_ms = manifest.resources.max_execution_time_ms;
        let respawn = Respawn::new(
            CapabilityKind::Tool,
            engine,
            component,
            manifest,
            granted_permissions,
            memory,
            event_bus,
            Self::register_host_functions,
        );
        let mut live = respawn.instantiate().await?;

        // Call get-spec to cache the tool specification. It gets its own full
        // fuel budget, like every later call.
        apply_store_epoch_deadline(&mut live.store, timeout_ms);
        live.store
            .set_fuel(manifest.resources.max_fuel)
            .map_err(|e| PluginError::Instantiation(format!("failed to set fuel: {e}")))?;
        let spec = Self::call_get_spec(&live.instance, &mut live.store).await?;

        Ok(Self {
            spec,
            supervisor: Arc::new(Supervisor::new(respawn, live).with_bounded_lock_wait()),
            timeout_ms,
        })
    }

    /// Plugin that exports this tool.
    pub fn plugin_name(&self) -> &str {
        self.supervisor.plugin_name()
    }

    /// Per-adapter health for `wasm_plugins_status`.
    pub fn diagnostics(&self) -> serde_json::Value {
        self.supervisor.diagnostics(serde_json::json!({
            "plugin": self.supervisor.plugin_name(),
            "tool": self.spec.name,
        }))
    }

    pub fn is_quarantined(&self) -> bool {
        self.supervisor.is_quarantined()
    }

    /// Register all host functions in the linker.
    fn register_host_functions(linker: &mut wasmtime::component::Linker<HostState>) -> Result<(), PluginError> {
        // Add WASI interfaces to the linker so WASM components that import
        // wasi:* (e.g. wasi:cli/environment) resolve correctly.
        wasmtime_wasi::p2::add_to_linker_async(linker)
            .map_err(|e| PluginError::Instantiation(format!("WASI link: {e}")))?;

        // log and config are identical across all capability types — use the
        // shared helpers from `common` to avoid duplication.
        super::common::register_log_host_functions(linker)?;
        super::common::register_config_host_functions(linker)?;

        // Tool plugins use result<T, string> return types for kv/http/memory,
        // so these interfaces are linked manually here.
        for iface in ["prx:host/kv@0.1.0", "prx:host/kv"] {
            let mut kv_inst = linker
                .instance(iface)
                .map_err(|e| PluginError::Instantiation(format!("linker error ({iface}): {e}")))?;

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
                .map_err(|e| PluginError::Instantiation(format!("link {iface}.get: {e}")))?;

            kv_inst
                .func_wrap_async(
                    "set",
                    |store: wasmtime::StoreContextMut<'_, HostState>, (key, value): (String, Vec<u8>)| {
                        Box::new(async move {
                            if let Err(e) = store.data().check_permission("kv") {
                                return Ok((Err::<(), String>(e),));
                            }
                            let kv = store.data().kv_store.clone();
                            let quota = store.data().kv_quota_bytes;
                            let result = crate::plugins::host::host_kv_set_bounded(&kv, quota, key, value).await;
                            Ok((result,))
                        })
                    },
                )
                .map_err(|e| PluginError::Instantiation(format!("link {iface}.set: {e}")))?;

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
                .map_err(|e| PluginError::Instantiation(format!("link {iface}.delete: {e}")))?;

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
                            let keys: Vec<String> = guard.keys().filter(|k| k.starts_with(&prefix)).cloned().collect();
                            Ok((keys,))
                        })
                    },
                )
                .map_err(|e| PluginError::Instantiation(format!("link {iface}.list-keys: {e}")))?;
        }

        for iface in ["prx:host/http-outbound@0.1.0", "prx:host/http-outbound"] {
            let mut http_inst = linker
                .instance(iface)
                .map_err(|e| PluginError::Instantiation(format!("linker error ({iface}): {e}")))?;

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
                                return Ok((Err::<(u16, Vec<(String, String)>, Vec<u8>), String>(
                                    e,
                                ),));
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
                                        Ok(bytes) => {
                                            Ok((Ok((status, resp_headers, bytes.to_vec())),))
                                        }
                                        Err(e) => Ok((Err(format!("body read error: {e}")),)),
                                    }
                                }
                                Err(e) => Ok((Err(format!("request failed: {e}")),)),
                            }
                        })
                    },
                )
                .map_err(|e| PluginError::Instantiation(format!("link {iface}.request: {e}")))?;
        }

        super::common::register_websocket_host_functions(linker)?;

        // prx:host/memory — connected to real memory backend
        for iface in ["prx:host/memory@0.1.0", "prx:host/memory"] {
            let mut mem_inst = linker
                .instance(iface)
                .map_err(|e| PluginError::Instantiation(format!("linker error ({iface}): {e}")))?;

            mem_inst
                .func_wrap_async(
                    "store",
                    |store: wasmtime::StoreContextMut<'_, HostState>, (text, category): (String, String)| {
                        Box::new(async move {
                            if let Err(e) = store.data().check_permission("memory") {
                                return Ok((Err::<String, String>(e),));
                            }
                            let mem = match &store.data().memory {
                                Some(m) => Arc::clone(m),
                                None => {
                                    return Ok((Err::<String, String>("memory backend not configured".to_string()),));
                                }
                            };
                            let plugin_name = store.data().plugin_name.clone();
                            match super::common::store_plugin_memory(mem.as_ref(), &plugin_name, &text, &category).await
                            {
                                Ok(key) => Ok((Ok::<String, String>(key),)),
                                Err(e) => Ok((Err(format!("memory store failed: {e}")),)),
                            }
                        })
                    },
                )
                .map_err(|e| PluginError::Instantiation(format!("link {iface}.store: {e}")))?;

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
                                        .map(|e| (e.id, e.content, e.category.to_string(), e.score.unwrap_or(0.0)))
                                        .collect();
                                    Ok((Ok(results),))
                                }
                                Err(e) => Ok((Err(format!("memory recall failed: {e}")),)),
                            }
                        })
                    },
                )
                .map_err(|e| PluginError::Instantiation(format!("link {iface}.recall: {e}")))?;
        }

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
        let iface_idx = iface_name_candidates
            .iter()
            .find_map(|name| instance.get_export_index(store.as_context_mut(), None, name))
            .ok_or_else(|| {
                PluginError::Instantiation(format!(
                    "plugin does not export any supported tool interface: {}",
                    iface_name_candidates.join(", ")
                ))
            })?;

        // Get the get-spec function from within that interface
        let func_idx = instance
            .get_export_index(store.as_context_mut(), Some(&iface_idx), "get-spec")
            .ok_or_else(|| PluginError::Instantiation("get-spec not found in tool-exports".to_string()))?;

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

        let parameters: serde_json::Value = serde_json::from_str(&params_schema)
            .unwrap_or_else(|_| serde_json::json!({"type": "object", "properties": {}}));

        Ok(ToolSpec {
            name,
            description,
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

        // Refused before the store lock: arguments the guest cannot hold would
        // trap it on `memory.grow` and poison the store.
        if !self.supervisor.admit_payload("execute", args_str.len()) {
            let error = PluginError::ResourceLimit(format!(
                "tool arguments of {} bytes exceed the {}-byte payload cap (resources.max_payload_bytes)",
                args_str.len(),
                self.supervisor.max_payload_bytes()
            ));
            return Ok(failed_tool_result(format!("WASM plugin error: {error}")));
        }

        // Detached: a stopped turn cannot drop the tool call mid-guest-call.
        Ok(
            match self.supervisor.call_detached(ExecuteCall { args: args_str }).await {
                Ok(tool_result) => tool_result,
                // A guest call that ran past its budget and a call that could not
                // get the store within that budget read the same, as they always
                // have; only the first is charged to the plugin.
                Err(PluginError::Timeout(_)) => {
                    failed_tool_result(format!("WASM plugin timed out after {}ms", self.timeout_ms))
                }
                Err(error) => failed_tool_result(format!("WASM plugin error: {error}")),
            },
        )
    }
}

const fn failed_tool_result(error: String) -> ToolResult {
    ToolResult {
        success: false,
        output: String::new(),
        error: Some(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll, Waker};

    /// Short enough to keep the timeout regressions fast, long enough that an
    /// unblocked `execute` never comes near it.
    const SHORT_TIMEOUT_MS: u64 = 200;

    fn voice_manifest(timeout_ms: u64) -> PluginManifest {
        toml::from_str(&format!(
            r#"
[plugin]
name = "voice-talk-realtime"
version = "0.1.0"
wasm = "plugin.wasm"

[[capabilities]]
type = "tool"
name = "voice_session"

[permissions]
required = ["log", "config", "kv"]
optional = []

[resources]
max_fuel = 1000000000
max_memory_mb = 8
max_execution_time_ms = {timeout_ms}
"#
        ))
        .expect("test: voice tool manifest parses")
    }

    /// The committed `voice-talk-realtime` component. Its `execute` increments
    /// a counter through the async `kv` host functions, so a test that holds the
    /// adapter's KV write lock suspends the guest inside a real call.
    async fn voice_adapter(timeout_ms: u64) -> WasmToolAdapter {
        let engine = crate::plugins::shared_wasm_engine().expect("test: shared wasmtime engine");
        let wasm = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/voice-talk-realtime/plugin.wasm");
        let bytes = std::fs::read(wasm).expect("test: the committed tool fixture is readable");
        let component =
            wasmtime::component::Component::from_binary(&engine, &bytes).expect("test: the fixture is a component");
        let granted = ["log", "config", "kv"].into_iter().map(str::to_string).collect();
        WasmToolAdapter::new_with_memory(&engine, &component, &voice_manifest(timeout_ms), granted, None, None)
            .await
            .expect("test: the fixture instantiates")
    }

    /// The KV namespace the guest's `kv` host functions lock.
    fn kv_namespace(adapter: &WasmToolAdapter) -> Arc<tokio::sync::RwLock<std::collections::HashMap<String, Vec<u8>>>> {
        adapter.supervisor.kv_namespace()
    }

    fn poll_once<F: Future + ?Sized>(future: Pin<&mut F>) -> Poll<F::Output> {
        future.poll(&mut Context::from_waker(Waker::noop()))
    }

    fn session_args() -> serde_json::Value {
        serde_json::json!({"provider": "openai"})
    }

    async fn assert_healthy_call(adapter: &WasmToolAdapter, context: &str) {
        let result = adapter
            .execute(session_args())
            .await
            .expect("test: execute returns a ToolResult");
        assert!(result.success, "{context}: {:?}", result.error);
        assert!(
            result.output.contains("wss://api.openai.com"),
            "{context}: {}",
            result.output
        );
    }

    /// D-023: a caller that gives up on a tool call — a user stopping the
    /// turn — no longer cancels the guest call. The guest finishes (or reaches
    /// its own timeout) on its own task, so the caller can neither poison the
    /// store nor be charged for it, and four abandonments leave a healthy tool
    /// live with every abandoned call completed.
    #[tokio::test]
    async fn an_abandoned_tool_call_finishes_on_its_own_and_poisons_nothing() {
        let adapter = voice_adapter(30_000).await;
        let kv = kv_namespace(&adapter);
        for round in 1..=4 {
            let hold = kv.write().await;
            {
                let mut call = std::pin::pin!(adapter.execute(session_args()));
                assert!(
                    poll_once(call.as_mut()).is_pending(),
                    "round {round}: the call must be pending"
                );
                // Let the call reach the guest and suspend on the held KV lock
                // before its caller gives up.
                for _ in 0..8 {
                    tokio::task::yield_now().await;
                }
            }
            drop(hold);
            assert_healthy_call(&adapter, &format!("round {round}: the call after an abandoned one")).await;
        }
        let sessions = kv
            .read()
            .await
            .get("openai_session_count")
            .and_then(|bytes| serde_json::from_slice::<i64>(bytes).ok());
        assert_eq!(sessions, Some(8), "every abandoned call must still have completed");
        let diagnostics = adapter.diagnostics();
        for (field, expected) in [("restart_count", 0), ("cancelled_count", 0), ("failure_count", 0)] {
            assert_eq!(
                diagnostics.get(field).and_then(serde_json::Value::as_u64),
                Some(expected),
                "{field}: {diagnostics}"
            );
        }
    }

    /// A guest that stays suspended past `max_execution_time_ms` is timed out
    /// inside the store lock, so the timeout is recorded as a poisoning outcome
    /// of the plugin: the next call rebuilds, and a plugin that keeps timing out
    /// spends its budget and is quarantined.
    #[tokio::test]
    async fn a_timed_out_tool_is_rebuilt_then_quarantined() {
        let adapter = voice_adapter(SHORT_TIMEOUT_MS).await;
        let kv = kv_namespace(&adapter);

        let hold = kv.write().await;
        let timed_out = adapter.execute(session_args()).await.expect("test: a ToolResult");
        drop(hold);
        assert!(!timed_out.success);
        assert!(
            timed_out.error.as_deref().unwrap_or_default().contains("timed out"),
            "{timed_out:?}"
        );
        assert_healthy_call(&adapter, "the call after a timeout").await;

        let hold = kv.write().await;
        for attempt in 1..=4 {
            let result = adapter.execute(session_args()).await.expect("test: a ToolResult");
            assert!(!result.success, "attempt {attempt} must fail while the KV lock is held");
        }
        drop(hold);
        let refused = adapter.execute(session_args()).await.expect("test: a ToolResult");
        assert!(!refused.success, "a quarantined tool must not report success");
        assert!(
            refused.error.as_deref().unwrap_or_default().contains("quarantined"),
            "{refused:?}"
        );
    }

    /// R-C3Xg G6: pins the one production path left for a guest call dropped
    /// mid-run — its task dropped at runtime shutdown. It is driven through the
    /// test-only in-line `Supervisor::call`, whose future plays the part of
    /// the dropped task: the committed guest is suspended inside a real `kv`
    /// host call, the future is dropped, and wasmtime's real "future dropped"
    /// unwind poisons the store. The adapter must be marked dead without a
    /// charge, rebuilt on a fresh store by the next call with its host KV
    /// intact, and never report `cannot enter component instance`.
    #[tokio::test]
    async fn a_call_dropped_mid_guest_is_rebuilt_uncharged_on_a_real_store() {
        let adapter = voice_adapter(30_000).await;
        let kv = kv_namespace(&adapter);
        assert_healthy_call(&adapter, "before the drop").await;

        let hold = kv.write().await;
        {
            let mut call = std::pin::pin!(adapter.supervisor.call(ExecuteCall {
                args: session_args().to_string(),
            }));
            assert!(
                poll_once(call.as_mut()).is_pending(),
                "the guest must be suspended inside the kv host call"
            );
        }
        drop(hold);

        let dropped = adapter.diagnostics();
        assert_eq!(
            dropped.get("state").and_then(serde_json::Value::as_str),
            Some("restarting"),
            "{dropped}"
        );
        assert_eq!(
            dropped.get("cancelled_count").and_then(serde_json::Value::as_u64),
            Some(1),
            "{dropped}"
        );

        assert_healthy_call(&adapter, "the call after the dropped one").await;
        let after = adapter.diagnostics();
        assert_eq!(
            after.get("restart_count").and_then(serde_json::Value::as_u64),
            Some(1),
            "{after}"
        );
        assert_eq!(
            after.get("state").and_then(serde_json::Value::as_str),
            Some("live"),
            "{after}"
        );
        assert!(
            !after
                .get("last_error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .contains("cannot enter component instance"),
            "{after}"
        );
        let sessions = kv
            .read()
            .await
            .get("openai_session_count")
            .and_then(|bytes| serde_json::from_slice::<i64>(bytes).ok());
        assert_eq!(
            sessions,
            Some(2),
            "host KV survives the rebuild; the dropped call never wrote"
        );
    }
}
