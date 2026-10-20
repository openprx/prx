//! Common host function registration for WASM plugins.
//!
//! Provides shared linker setup for the canonical PDK host interfaces. All
//! capability adapters use the same WIT ABI so a component can move between
//! tool, middleware, hook, and cron worlds without host-signature drift.

use std::sync::Arc;

use crate::plugins::error::{PluginError, PluginResult};
use crate::plugins::host::HostState;

/// Register `prx:host/log@0.1.0` host functions into the linker.
pub fn register_log_host_functions(linker: &mut wasmtime::component::Linker<HostState>) -> PluginResult<()> {
    for iface in ["prx:host/log@0.1.0", "prx:host/log"] {
        let mut log_inst = linker
            .instance(iface)
            .map_err(|e| PluginError::Instantiation(format!("linker error ({iface}): {e}")))?;
        log_inst
            .func_new("log", |store, _type, params, results| {
                let [
                    wasmtime::component::Val::Enum(level),
                    wasmtime::component::Val::String(message),
                ] = params
                else {
                    return Err(wasmtime::Error::msg(
                        "log host function expected (level enum, message string)",
                    ));
                };
                if !results.is_empty() {
                    return Err(wasmtime::Error::msg("log host function expected no results"));
                }
                let name = store.data().plugin_name.clone();
                match level.as_str() {
                    "trace" => tracing::trace!(plugin = %name, "{message}"),
                    "debug" => tracing::debug!(plugin = %name, "{message}"),
                    "info" => tracing::info!(plugin = %name, "{message}"),
                    "warn" => tracing::warn!(plugin = %name, "{message}"),
                    "error" => tracing::error!(plugin = %name, "{message}"),
                    _ => tracing::info!(plugin = %name, level = %level, "{message}"),
                }
                Ok(())
            })
            .map_err(|e| PluginError::Instantiation(format!("link {iface}.log: {e}")))?;
    }
    Ok(())
}

/// Register `prx:host/config@0.1.0` host functions into the linker.
pub fn register_config_host_functions(linker: &mut wasmtime::component::Linker<HostState>) -> PluginResult<()> {
    for iface in ["prx:host/config@0.1.0", "prx:host/config"] {
        let mut config_inst = linker
            .instance(iface)
            .map_err(|e| PluginError::Instantiation(format!("linker error ({iface}): {e}")))?;
        config_inst
            .func_wrap(
                "get",
                |store: wasmtime::StoreContextMut<'_, HostState>, (key,): (String,)| {
                    let value = store.data().config.get(&key).cloned();
                    Ok((value,))
                },
            )
            .map_err(|e| PluginError::Instantiation(format!("link {iface}.get: {e}")))?;
        config_inst
            .func_wrap("get-all", |store: wasmtime::StoreContextMut<'_, HostState>, (): ()| {
                let pairs: Vec<(String, String)> = store
                    .data()
                    .config
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                Ok((pairs,))
            })
            .map_err(|e| PluginError::Instantiation(format!("link {iface}.get-all: {e}")))?;
    }
    Ok(())
}

/// Register `prx:host/kv@0.1.0` host functions into the linker.
///
/// Uses the canonical PDK ABI. Permission and quota failures are returned to
/// the guest for mutating operations; reads retain their option/list shape.
pub fn register_kv_host_functions(linker: &mut wasmtime::component::Linker<HostState>) -> PluginResult<()> {
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
                        let value = guard.get(&key).cloned();
                        Ok((value,))
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

    Ok(())
}

/// Register `prx:host/events@0.1.0` host functions into the linker.
///
/// Exposes `publish`, `subscribe`, and `unsubscribe` to WASM plugins.
/// All calls require the `"events"` permission; violations return an error
/// string to the plugin rather than panicking.
pub fn register_event_host_functions(linker: &mut wasmtime::component::Linker<HostState>) -> PluginResult<()> {
    use crate::plugins::event_bus::MAX_PAYLOAD_BYTES;

    for iface in ["prx:host/events@0.1.0", "prx:host/events"] {
        let mut inst = linker
            .instance(iface)
            .map_err(|e| PluginError::Instantiation(format!("linker error ({iface}): {e}")))?;

        // ── publish ──
        inst.func_wrap_async(
            "publish",
            |store: wasmtime::StoreContextMut<'_, HostState>, (topic, payload): (String, String)| {
                Box::new(async move {
                    // Permission check.
                    if let Err(e) = store.data().check_permission("events") {
                        tracing::warn!("{e}");
                        return Ok((Err::<(), String>(e),));
                    }
                    // Payload size check.
                    if payload.len() > MAX_PAYLOAD_BYTES {
                        let err = format!(
                            "event bus: payload size {} exceeds maximum {} bytes",
                            payload.len(),
                            MAX_PAYLOAD_BYTES
                        );
                        return Ok((Err(err),));
                    }
                    match &store.data().event_bus {
                        None => {
                            // No event bus configured — silently succeed.
                            tracing::debug!(topic = %topic, "event bus not configured, dropping publish");
                            Ok((Ok(()),))
                        }
                        Some(bus) => {
                            let bus = bus.clone();
                            match bus.publish(&topic, &payload).await {
                                Ok(()) => Ok((Ok(()),)),
                                Err(e) => Ok((Err(e),)),
                            }
                        }
                    }
                })
            },
        )
        .map_err(|e| PluginError::Instantiation(format!("link {iface}.publish: {e}")))?;

        // ── subscribe ──
        inst.func_wrap_async(
            "subscribe",
            |store: wasmtime::StoreContextMut<'_, HostState>, (pattern,): (String,)| {
                Box::new(async move {
                    if let Err(e) = store.data().check_permission("events") {
                        tracing::warn!("{e}");
                        return Ok((Err::<u64, String>(e),));
                    }
                    let Some(event_sink) = store.data().event_sink.clone() else {
                        return Ok((Err(
                            "event subscriptions require a hook-capable plugin event handler".to_string()
                        ),));
                    };
                    match &store.data().event_bus {
                        None => Ok((Err("event bus is not configured".to_string()),)),
                        Some(bus) => {
                            let plugin_name = store.data().plugin_name.clone();
                            let bus = bus.clone();
                            match bus.subscribe_pump(&plugin_name, &pattern, event_sink).await {
                                Ok(id) => {
                                    tracing::debug!(
                                        plugin = %plugin_name,
                                        pattern = %pattern,
                                        subscription_id = id,
                                        "event bus: subscription registered"
                                    );
                                    Ok((Ok(id),))
                                }
                                Err(e) => Ok((Err(e),)),
                            }
                        }
                    }
                })
            },
        )
        .map_err(|e| PluginError::Instantiation(format!("link {iface}.subscribe: {e}")))?;

        // ── unsubscribe ──
        inst.func_wrap_async(
            "unsubscribe",
            |store: wasmtime::StoreContextMut<'_, HostState>, (sub_id,): (u64,)| {
                Box::new(async move {
                    if let Err(e) = store.data().check_permission("events") {
                        tracing::warn!("{e}");
                        return Ok((Err::<(), String>(e),));
                    }
                    match &store.data().event_bus {
                        None => Ok((Ok(()),)),
                        Some(bus) => {
                            let bus = bus.clone();
                            let plugin_name = store.data().plugin_name.clone();
                            match bus.unsubscribe(&plugin_name, sub_id).await {
                                Ok(()) => Ok((Ok(()),)),
                                Err(e) => Ok((Err(e),)),
                            }
                        }
                    }
                })
            },
        )
        .map_err(|e| PluginError::Instantiation(format!("link {iface}.unsubscribe: {e}")))?;
    }

    Ok(())
}

/// One `memory.store` host call: a fresh `plugin:<name>:<uuid>` key in the
/// workspace memory, written with the plugin as its origin (channel
/// `plugin:<name>`, no owner). Returns the key.
///
/// Written with no origin at all, the row matched the snapshot's
/// "no caller context" arm and was exported to the Git-visible
/// `MEMORY_SNAPSHOT.md` as if the operator had written it; a plugin's row may
/// carry text from any channel its plugin handles.
///
/// The origin travels as metadata, not as a caller context: a plugin is not
/// a caller with a workspace or an owner, and `PostgresMemory` refuses a
/// context write that has neither, while its metadata write stays the
/// system-scoped write plugins always made (R-INT2A F1).
pub(crate) async fn store_plugin_memory(
    memory: &dyn crate::memory::traits::Memory,
    plugin_name: &str,
    text: &str,
    category: &str,
) -> anyhow::Result<String> {
    let key = format!("plugin:{plugin_name}:{}", uuid::Uuid::new_v4());
    let category = match category {
        "core" => crate::memory::traits::MemoryCategory::Core,
        "daily" => crate::memory::traits::MemoryCategory::Daily,
        "conversation" => crate::memory::traits::MemoryCategory::Conversation,
        other => crate::memory::traits::MemoryCategory::Custom(other.to_string()),
    };
    let metadata = crate::memory::traits::MemoryStoreMetadata {
        channel: Some(format!("plugin:{plugin_name}")),
        ..crate::memory::traits::MemoryStoreMetadata::default()
    };
    memory.store_with_metadata(&key, text, category, None, metadata).await?;
    Ok(key)
}

/// Register `prx:host/http-outbound@0.1.0` host functions into the linker.
///
/// Exposes an HTTP `request` function to WASM plugins. Calls are guarded by
/// the `"http-outbound"` permission and the configured URL allowlist.
pub fn register_http_host_functions(linker: &mut wasmtime::component::Linker<HostState>) -> PluginResult<()> {
    for iface in ["prx:host/http-outbound@0.1.0", "prx:host/http-outbound"] {
        let mut http_inst = linker
            .instance(iface)
            .map_err(|e| PluginError::Instantiation(format!("linker error ({iface}): {e}")))?;

        http_inst
            .func_wrap_async(
                "request",
                |mut store: wasmtime::StoreContextMut<'_, HostState>,
                 (method, url, headers, body): (
                    String,
                    String,
                    Vec<(String, String)>,
                    Option<Vec<u8>>,
                )| {
                    // Permission, allowlist, address literal and request budget
                    // are all decided before the guest is suspended on I/O.
                    let authorized = store
                        .data()
                        .check_permission("http-outbound")
                        .and_then(|()| store.data_mut().authorize_outbound(&url, &["http", "https"]));
                    let allowlist = Arc::clone(&store.data().outbound);
                    let timeout_ms = store.data().timeout_ms;
                    Box::new(async move {
                        let target = match authorized {
                            Ok(target) => target,
                            Err(error) => {
                                return Ok((Err::<(u16, Vec<(String, String)>, Vec<u8>), String>(error),));
                            }
                        };
                        let result = crate::plugins::host::perform_http_request(
                            allowlist, timeout_ms, method, target, headers, body,
                        )
                        .await;
                        Ok((result,))
                    })
                },
            )
            .map_err(|e| PluginError::Instantiation(format!("link {iface}.request: {e}")))?;
    }

    Ok(())
}

/// Register `prx:host/memory@0.1.0` using the configured live memory backend.
pub fn register_memory_host_functions(linker: &mut wasmtime::component::Linker<HostState>) -> PluginResult<()> {
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
                        let Some(memory) = store.data().memory.clone() else {
                            return Ok((Err::<String, String>("memory backend not configured".to_string()),));
                        };
                        let plugin_name = store.data().plugin_name.clone();
                        match store_plugin_memory(memory.as_ref(), &plugin_name, &text, &category).await {
                            Ok(key) => Ok((Ok::<String, String>(key),)),
                            Err(error) => Ok((Err(format!("memory store failed: {error}")),)),
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
                        let Some(memory) = store.data().memory.clone() else {
                            return Ok((Err::<Vec<(String, String, String, f64)>, String>(
                                "memory backend not configured".to_string(),
                            ),));
                        };
                        match memory.recall(&query, limit as usize, None).await {
                            Ok(entries) => Ok((Ok(entries
                                .into_iter()
                                .map(|entry| {
                                    (
                                        entry.id,
                                        entry.content,
                                        entry.category.to_string(),
                                        entry.score.unwrap_or(0.0),
                                    )
                                })
                                .collect()),)),
                            Err(error) => Ok((Err(format!("memory recall failed: {error}")),)),
                        }
                    })
                },
            )
            .map_err(|e| PluginError::Instantiation(format!("link {iface}.recall: {e}")))?;
    }
    Ok(())
}

/// Register `prx:host/websocket-outbound@0.1.0` host functions into the linker.
///
/// Exposes websocket connect/send/receive/close operations to WASM plugins.
/// Calls are guarded by the `"websocket-outbound"` permission and the
/// configured outbound URL allowlist.
pub fn register_websocket_host_functions(linker: &mut wasmtime::component::Linker<HostState>) -> PluginResult<()> {
    for iface in ["prx:host/websocket-outbound@0.1.0", "prx:host/websocket-outbound"] {
        let mut ws_inst = linker
            .instance(iface)
            .map_err(|e| PluginError::Instantiation(format!("linker error ({iface}): {e}")))?;

        ws_inst
            .func_wrap_async(
                "connect",
                |mut store: wasmtime::StoreContextMut<'_, HostState>, (url,): (String,)| {
                    let authorized = store
                        .data()
                        .check_permission("websocket-outbound")
                        .and_then(|()| store.data_mut().authorize_outbound(&url, &["ws", "wss"]));
                    let timeout_ms = store.data().timeout_ms;
                    let connector = Arc::clone(&store.data().websocket_connector);
                    let sessions = Arc::clone(&store.data().websocket_sessions);
                    let next_session_id = Arc::clone(&store.data().next_websocket_session_id);
                    Box::new(async move {
                        let target = match authorized {
                            Ok(target) => target.to_string(),
                            Err(error) => return Ok((Err::<u64, String>(error),)),
                        };
                        let result = crate::plugins::host::websocket_connect_with(
                            connector,
                            sessions,
                            next_session_id,
                            timeout_ms,
                            target,
                        )
                        .await;
                        Ok((result,))
                    })
                },
            )
            .map_err(|e| PluginError::Instantiation(format!("link {iface}.connect: {e}")))?;

        ws_inst
            .func_wrap_async(
                "send",
                |store: wasmtime::StoreContextMut<'_, HostState>, (session_id, message): (u64, String)| {
                    let permission = store.data().check_permission("websocket-outbound");
                    let timeout_ms = store.data().timeout_ms;
                    let sessions = Arc::clone(&store.data().websocket_sessions);
                    Box::new(async move {
                        if let Err(error) = permission {
                            return Ok((Err::<(), String>(error),));
                        }
                        let result =
                            crate::plugins::host::websocket_send_with(sessions, timeout_ms, session_id, message).await;
                        Ok((result,))
                    })
                },
            )
            .map_err(|e| PluginError::Instantiation(format!("link {iface}.send: {e}")))?;

        ws_inst
            .func_wrap_async(
                "receive",
                |store: wasmtime::StoreContextMut<'_, HostState>, (session_id,): (u64,)| {
                    let permission = store.data().check_permission("websocket-outbound");
                    let timeout_ms = store.data().timeout_ms;
                    let sessions = Arc::clone(&store.data().websocket_sessions);
                    Box::new(async move {
                        if let Err(error) = permission {
                            return Ok((Err::<String, String>(error),));
                        }
                        let result =
                            crate::plugins::host::websocket_receive_with(sessions, timeout_ms, session_id).await;
                        Ok((result,))
                    })
                },
            )
            .map_err(|e| PluginError::Instantiation(format!("link {iface}.receive: {e}")))?;

        ws_inst
            .func_wrap_async(
                "close",
                |store: wasmtime::StoreContextMut<'_, HostState>, (session_id,): (u64,)| {
                    let permission = store.data().check_permission("websocket-outbound");
                    let timeout_ms = store.data().timeout_ms;
                    let sessions = Arc::clone(&store.data().websocket_sessions);
                    Box::new(async move {
                        if let Err(error) = permission {
                            return Ok((Err::<(), String>(error),));
                        }
                        let result = crate::plugins::host::websocket_close_with(sessions, timeout_ms, session_id).await;
                        Ok((result,))
                    })
                },
            )
            .map_err(|e| PluginError::Instantiation(format!("link {iface}.close: {e}")))?;
    }

    Ok(())
}

/// Register the complete canonical PDK import surface in a single call.
pub fn register_common_host_functions(linker: &mut wasmtime::component::Linker<HostState>) -> PluginResult<()> {
    wasmtime_wasi::p2::add_to_linker_async(linker)
        .map_err(|e| PluginError::Instantiation(format!("WASI link: {e}")))?;
    register_log_host_functions(linker)?;
    register_config_host_functions(linker)?;
    register_kv_host_functions(linker)?;
    register_event_host_functions(linker)?;
    register_http_host_functions(linker)?;
    register_memory_host_functions(linker)?;
    register_websocket_host_functions(linker)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::traits::{Memory, MemoryCategory, MemoryEntry, MemoryStoreMetadata};

    /// A backend that refuses every context-bearing write, the way
    /// `PostgresMemory` refuses one that carries no workspace or owner, and
    /// records the metadata of the writes it accepts.
    #[derive(Default)]
    struct RefusesContextWrites {
        channels: parking_lot::Mutex<Vec<Option<String>>>,
    }

    #[async_trait::async_trait]
    impl Memory for RefusesContextWrites {
        fn name(&self) -> &str {
            "refuses-context-writes"
        }
        async fn store(&self, _: &str, _: &str, _: MemoryCategory, _: Option<&str>) -> anyhow::Result<()> {
            self.channels.lock().push(None);
            Ok(())
        }
        async fn store_with_context(
            &self,
            _: &str,
            _: &str,
            _: MemoryCategory,
            _: Option<&str>,
            _: Option<&crate::memory::MemoryWriteContext>,
        ) -> anyhow::Result<()> {
            anyhow::bail!("PostgreSQL vector isolation requires a trusted workspace context")
        }
        async fn store_with_metadata(
            &self,
            _: &str,
            _: &str,
            _: MemoryCategory,
            _: Option<&str>,
            metadata: MemoryStoreMetadata,
        ) -> anyhow::Result<()> {
            self.channels.lock().push(metadata.channel);
            Ok(())
        }
        async fn recall(&self, _: &str, _: usize, _: Option<&str>) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }
        async fn get(&self, _: &str) -> anyhow::Result<Option<MemoryEntry>> {
            Ok(None)
        }
        async fn list(&self, _: Option<&MemoryCategory>, _: Option<&str>) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }
        async fn forget(&self, _: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        async fn count(&self) -> anyhow::Result<usize> {
            Ok(0)
        }
        async fn health_check(&self) -> bool {
            true
        }
    }

    /// R-INT2A F1: a plugin write carries its origin as metadata, never as a
    /// caller context. `PostgresMemory` refuses a context with no workspace
    /// or owner, so the context path made every plugin `memory.store` fail
    /// on PostgreSQL; the metadata path keeps its system-scoped write.
    ///
    /// MUTATION GUARD: route the helper through `store_with_context` and the
    /// write fails here.
    #[tokio::test]
    async fn a_plugin_memory_write_goes_through_the_metadata_path() {
        let memory = RefusesContextWrites::default();
        store_plugin_memory(&memory, "sample-plugin", "plugin-authored text", "core")
            .await
            .expect("test: a plugin write never takes the context path");
        assert_eq!(
            *memory.channels.lock(),
            vec![Some("plugin:sample-plugin".to_string())],
            "exactly one write, tagged with the plugin as its channel"
        );
    }

    /// R-MCPBh L-1: a plugin's `memory.store` write is the plugin's, not the
    /// operator's. Written without a context it matched the snapshot's
    /// "no caller context at all" arm and was exported to the Git-visible
    /// `MEMORY_SNAPSHOT.md` as an operator row.
    #[tokio::test]
    async fn a_plugin_memory_write_is_not_exported_as_an_operator_row() {
        let workspace = tempfile::tempdir().expect("test: workspace");
        let memory = crate::memory::sqlite::SqliteMemory::new(workspace.path()).expect("test: sqlite memory");
        let key = store_plugin_memory(&memory, "sample-plugin", "plugin-authored text", "core")
            .await
            .expect("test: plugin write");
        assert!(key.starts_with("plugin:sample-plugin:"), "{key}");
        let stored = crate::memory::traits::Memory::get(&memory, &key)
            .await
            .expect("test: read back");
        assert!(stored.is_some(), "the plugin write is stored");
        drop(memory);

        crate::memory::snapshot::export_snapshot(workspace.path()).expect("test: export");
        let snapshot = std::fs::read_to_string(workspace.path().join(crate::memory::snapshot::SNAPSHOT_FILENAME))
            .unwrap_or_default();
        assert!(
            !snapshot.contains("plugin-authored text"),
            "a plugin row must stay out of the operator snapshot: {snapshot}"
        );
    }
}
