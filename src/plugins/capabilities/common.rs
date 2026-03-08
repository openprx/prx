//! Common host function registration for WASM plugins.
//!
//! Provides shared linker setup for the `prx:host/log`, `prx:host/config`,
//! and `prx:host/kv` interfaces. All capability adapters (middleware, hook,
//! cron) call `register_common_host_functions` to avoid duplicating ~200 lines
//! of identical boilerplate.
//!
//! `WasmToolAdapter` uses `register_log_host_functions` and
//! `register_config_host_functions` directly because its kv interface exposes
//! `result<T, string>` return types that differ from the simpler variants used
//! by the other capability types.

use crate::plugins::error::{PluginError, PluginResult};
use crate::plugins::host::HostState;

/// Register `prx:host/log@0.1.0` host functions into the linker.
pub fn register_log_host_functions(
    linker: &mut wasmtime::component::Linker<HostState>,
) -> PluginResult<()> {
    let mut log_inst = linker
        .instance("prx:host/log@0.1.0")
        .map_err(|e| PluginError::Instantiation(format!("linker error (log): {e}")))?;
    log_inst
        .func_wrap(
            "log",
            |store: wasmtime::StoreContextMut<'_, HostState>,
             (level, message): (String, String)| {
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
            },
        )
        .map_err(|e| PluginError::Instantiation(format!("link log.log: {e}")))?;
    Ok(())
}

/// Register `prx:host/config@0.1.0` host functions into the linker.
pub fn register_config_host_functions(
    linker: &mut wasmtime::component::Linker<HostState>,
) -> PluginResult<()> {
    let mut config_inst = linker
        .instance("prx:host/config@0.1.0")
        .map_err(|e| PluginError::Instantiation(format!("linker error (config): {e}")))?;
    config_inst
        .func_wrap(
            "get",
            |store: wasmtime::StoreContextMut<'_, HostState>, (key,): (String,)| {
                let value = store.data().config.get(&key).cloned();
                Ok((value,))
            },
        )
        .map_err(|e| PluginError::Instantiation(format!("link config.get: {e}")))?;
    config_inst
        .func_wrap(
            "get-all",
            |store: wasmtime::StoreContextMut<'_, HostState>, (): ()| {
                let pairs: Vec<(String, String)> = store
                    .data()
                    .config
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                Ok((pairs,))
            },
        )
        .map_err(|e| PluginError::Instantiation(format!("link config.get-all: {e}")))?;
    Ok(())
}

/// Register `prx:host/kv@0.1.0` host functions into the linker.
///
/// This variant uses simple (non-`result`) return types as required by the
/// middleware, hook, and cron WIT worlds. Permission is checked on every call;
/// violations are logged and silently ignored (get → `None`, set → no-op,
/// delete → `false`).
pub fn register_kv_host_functions(
    linker: &mut wasmtime::component::Linker<HostState>,
) -> PluginResult<()> {
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
                    let value = guard.get(&key).cloned();
                    Ok((value,))
                })
            },
        )
        .map_err(|e| PluginError::Instantiation(format!("link kv.get: {e}")))?;

    kv_inst
        .func_wrap_async(
            "set",
            |store: wasmtime::StoreContextMut<'_, HostState>, (key, value): (String, Vec<u8>)| {
                Box::new(async move {
                    if let Err(e) = store.data().check_permission("kv") {
                        tracing::warn!("{e}");
                        return Ok(());
                    }
                    let kv = store.data().kv_store.clone();
                    let mut guard = kv.write().await;
                    guard.insert(key, value);
                    Ok(())
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
                        tracing::warn!("{e}");
                        return Ok((false,));
                    }
                    let kv = store.data().kv_store.clone();
                    let mut guard = kv.write().await;
                    let existed = guard.remove(&key).is_some();
                    Ok((existed,))
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

    Ok(())
}

/// Register `prx:host/events@0.1.0` host functions into the linker.
///
/// Exposes `publish`, `subscribe`, and `unsubscribe` to WASM plugins.
/// All calls require the `"events"` permission; violations return an error
/// string to the plugin rather than panicking.
pub fn register_event_host_functions(
    linker: &mut wasmtime::component::Linker<HostState>,
) -> PluginResult<()> {
    use crate::plugins::event_bus::MAX_PAYLOAD_BYTES;

    let mut inst = linker
        .instance("prx:host/events@0.1.0")
        .map_err(|e| PluginError::Instantiation(format!("linker error (events): {e}")))?;

    // ── publish ──
    inst.func_wrap_async(
        "publish",
        |store: wasmtime::StoreContextMut<'_, HostState>,
         (topic, payload): (String, String)| {
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
    .map_err(|e| PluginError::Instantiation(format!("link events.publish: {e}")))?;

    // ── subscribe ──
    inst.func_wrap_async(
        "subscribe",
        |store: wasmtime::StoreContextMut<'_, HostState>, (pattern,): (String,)| {
            Box::new(async move {
                if let Err(e) = store.data().check_permission("events") {
                    tracing::warn!("{e}");
                    return Ok((Err::<u64, String>(e),));
                }
                match &store.data().event_bus {
                    None => {
                        tracing::debug!(pattern = %pattern, "event bus not configured, subscribe no-op");
                        Ok((Ok(0u64),))
                    }
                    Some(bus) => {
                        let plugin_name = store.data().plugin_name.clone();
                        let bus = bus.clone();
                        match bus.subscribe(&plugin_name, &pattern).await {
                            Ok((id, _rx)) => {
                                // Note: the receiver is intentionally dropped here for the
                                // host-function path. Full receiver wiring (dispatching back
                                // into guest on-event) is deferred to the PDK integration layer.
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
    .map_err(|e| PluginError::Instantiation(format!("link events.subscribe: {e}")))?;

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
                        match bus.unsubscribe(sub_id).await {
                            Ok(()) => Ok((Ok(()),)),
                            Err(e) => Ok((Err(e),)),
                        }
                    }
                }
            })
        },
    )
    .map_err(|e| PluginError::Instantiation(format!("link events.unsubscribe: {e}")))?;

    Ok(())
}

/// Register `prx:host/http-outbound@0.1.0` host functions into the linker.
///
/// Exposes an HTTP `request` function to WASM plugins. Calls are guarded by
/// the `"http-outbound"` permission and the configured URL allowlist.
pub fn register_http_host_functions(
    linker: &mut wasmtime::component::Linker<HostState>,
) -> PluginResult<()> {
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
                                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
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

    Ok(())
}

/// Register all common host functions (log + config + kv) in a single call.
///
/// Used by middleware, hook, and cron capability adapters. Tool adapters have
/// a different kv interface (`result<T, string>` return types) and call the
/// individual helpers instead.
pub fn register_common_host_functions(
    linker: &mut wasmtime::component::Linker<HostState>,
) -> PluginResult<()> {
    register_log_host_functions(linker)?;
    register_config_host_functions(linker)?;
    register_kv_host_functions(linker)?;
    register_event_host_functions(linker)?;
    Ok(())
}
