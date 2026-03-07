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
            |store: wasmtime::StoreContextMut<'_, HostState>,
             (key, value): (String, Vec<u8>)| {
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
    Ok(())
}
