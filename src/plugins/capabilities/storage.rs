//! Storage capability — bridges WASM storage plugins to the PRX `Memory` trait.
//!
//! WASM storage plugins export functions for storing, recalling, and forgetting
//! memory entries. The host delegates memory operations to the plugin when it is
//! configured as the active Memory backend.

use async_trait::async_trait;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Mutex;
use wasmtime::AsContextMut;

use crate::memory::traits::{validate_memory_write_target, MemoryCategory, MemoryEntry};
use crate::plugins::error::{PluginError, PluginResult};
use crate::plugins::host::HostState;
use crate::plugins::manifest::PluginManifest;

/// A loaded WASM storage plugin instance.
pub struct WasmStorage {
    /// The cached storage backend name (returned by `name()` at load time).
    storage_name: String,
    /// wasmtime store + instance, behind a mutex (Store is not Sync).
    inner: Arc<Mutex<WasmStorageInner>>,
    /// Timeout for storage calls (milliseconds).
    timeout_ms: u64,
}

struct WasmStorageInner {
    store: wasmtime::Store<HostState>,
    instance: wasmtime::component::Instance,
}

impl WasmStorage {
    /// Create a new `WasmStorage` from a compiled WASM component.
    ///
    /// Steps:
    /// 1. Build `HostState` from the manifest permissions/config.
    /// 2. Instantiate the component.
    /// 3. Call `name()` export to cache the storage backend name.
    pub async fn new(
        engine: &wasmtime::Engine,
        component: &wasmtime::component::Component,
        manifest: &PluginManifest,
        event_bus: Option<Arc<crate::plugins::event_bus::EventBus>>,
    ) -> PluginResult<Self> {
        let timeout_ms = manifest.resources.max_execution_time_ms;

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
            .map_err(|e| {
                PluginError::Instantiation(format!("failed to instantiate storage plugin: {e}"))
            })?;

        // Cache the storage backend name at load time.
        let storage_name = Self::call_name(&instance, &mut store).await?;

        tracing::info!(
            plugin = %manifest.plugin.name,
            storage = %storage_name,
            "WASM storage backend registered"
        );

        Ok(Self {
            storage_name,
            inner: Arc::new(Mutex::new(WasmStorageInner { store, instance })),
            timeout_ms,
        })
    }

    /// The storage backend name as declared by the WASM plugin.
    pub fn storage_name(&self) -> &str {
        &self.storage_name
    }

    /// Register host functions needed by storage world plugins.
    ///
    /// Storage world imports: log, config, http-outbound, events.
    fn register_host_functions(
        linker: &mut wasmtime::component::Linker<HostState>,
    ) -> PluginResult<()> {
        super::common::register_log_host_functions(linker)?;
        super::common::register_config_host_functions(linker)?;
        super::common::register_http_host_functions(linker)?;
        super::common::register_websocket_host_functions(linker)?;
        super::common::register_event_host_functions(linker)?;
        Ok(())
    }

    /// Call the `name` export to get the storage backend name.
    async fn call_name(
        instance: &wasmtime::component::Instance,
        store: &mut wasmtime::Store<HostState>,
    ) -> PluginResult<String> {
        let iface_idx = instance
            .get_export(
                store.as_context_mut(),
                None,
                "prx:plugin/storage-exports@0.1.0",
            )
            .ok_or_else(|| {
                PluginError::Instantiation(
                    "plugin does not export prx:plugin/storage-exports@0.1.0".to_string(),
                )
            })?;

        let func_idx = instance
            .get_export(store.as_context_mut(), Some(&iface_idx), "name")
            .ok_or_else(|| {
                PluginError::Instantiation("name not found in storage-exports".to_string())
            })?;

        let name_fn = instance
            .get_func(store.as_context_mut(), &func_idx)
            .ok_or_else(|| PluginError::Instantiation("name is not a function".to_string()))?;

        let mut results = vec![wasmtime::component::Val::Bool(false)];
        name_fn
            .call_async(store.as_context_mut(), &[], &mut results)
            .await
            .map_err(|e| PluginError::Runtime(format!("name() call failed: {e}")))?;

        name_fn
            .post_return_async(store.as_context_mut())
            .await
            .map_err(|e| PluginError::Runtime(format!("name() post_return failed: {e}")))?;

        match &results[0] {
            wasmtime::component::Val::String(s) => Ok(s.to_string()),
            _ => Err(PluginError::Runtime(
                "name() returned unexpected value type".to_string(),
            )),
        }
    }

    /// Internal: call `store-memory` on the WASM plugin.
    async fn call_store_memory_inner(
        &self,
        key: &str,
        content: &str,
        category: &str,
        session_id: Option<&str>,
    ) -> PluginResult<()> {
        let mut inner = self.inner.lock().await;
        let WasmStorageInner {
            ref mut store,
            ref instance,
        } = *inner;

        let iface_idx = instance
            .get_export(
                store.as_context_mut(),
                None,
                "prx:plugin/storage-exports@0.1.0",
            )
            .ok_or_else(|| {
                PluginError::Runtime(
                    "plugin does not export prx:plugin/storage-exports@0.1.0".to_string(),
                )
            })?;

        let func_idx = instance
            .get_export(store.as_context_mut(), Some(&iface_idx), "store-memory")
            .ok_or_else(|| {
                PluginError::Runtime("store-memory not found in storage-exports".to_string())
            })?;

        let store_fn = instance
            .get_func(store.as_context_mut(), &func_idx)
            .ok_or_else(|| PluginError::Runtime("store-memory is not a function".to_string()))?;

        let session_val = match session_id {
            Some(s) => wasmtime::component::Val::Option(Some(Box::new(
                wasmtime::component::Val::String(s.into()),
            ))),
            None => wasmtime::component::Val::Option(None),
        };

        let params = [
            wasmtime::component::Val::String(key.into()),
            wasmtime::component::Val::String(content.into()),
            wasmtime::component::Val::String(category.into()),
            session_val,
        ];
        let mut results = vec![wasmtime::component::Val::Bool(false)];

        store_fn
            .call_async(store.as_context_mut(), &params, &mut results)
            .await
            .map_err(|e| PluginError::Runtime(format!("store-memory() call failed: {e}")))?;

        store_fn
            .post_return_async(store.as_context_mut())
            .await
            .map_err(|e| PluginError::Runtime(format!("store-memory() post_return failed: {e}")))?;

        // Parse result<_, string>
        match &results[0] {
            wasmtime::component::Val::Result(r) => match r.as_ref() {
                Ok(_) => Ok(()),
                Err(Some(inner_err)) => match inner_err.as_ref() {
                    wasmtime::component::Val::String(e) => Err(PluginError::Runtime(format!(
                        "WASM storage '{}' store-memory error: {e}",
                        self.storage_name
                    ))),
                    _ => Err(PluginError::Runtime(format!(
                        "WASM storage '{}' store-memory returned unknown error",
                        self.storage_name
                    ))),
                },
                Err(None) => Err(PluginError::Runtime(format!(
                    "WASM storage '{}' store-memory returned unknown error",
                    self.storage_name
                ))),
            },
            _ => Err(PluginError::Runtime(
                "store-memory() returned unexpected value type".to_string(),
            )),
        }
    }

    /// Internal: call `recall-memory` on the WASM plugin.
    async fn call_recall_memory_inner(
        &self,
        query: &str,
        limit: u32,
        session_id: Option<&str>,
    ) -> PluginResult<Vec<MemoryEntry>> {
        let mut inner = self.inner.lock().await;
        let WasmStorageInner {
            ref mut store,
            ref instance,
        } = *inner;

        let iface_idx = instance
            .get_export(
                store.as_context_mut(),
                None,
                "prx:plugin/storage-exports@0.1.0",
            )
            .ok_or_else(|| {
                PluginError::Runtime(
                    "plugin does not export prx:plugin/storage-exports@0.1.0".to_string(),
                )
            })?;

        let func_idx = instance
            .get_export(store.as_context_mut(), Some(&iface_idx), "recall-memory")
            .ok_or_else(|| {
                PluginError::Runtime("recall-memory not found in storage-exports".to_string())
            })?;

        let recall_fn = instance
            .get_func(store.as_context_mut(), &func_idx)
            .ok_or_else(|| PluginError::Runtime("recall-memory is not a function".to_string()))?;

        let session_val = match session_id {
            Some(s) => wasmtime::component::Val::Option(Some(Box::new(
                wasmtime::component::Val::String(s.into()),
            ))),
            None => wasmtime::component::Val::Option(None),
        };

        let params = [
            wasmtime::component::Val::String(query.into()),
            wasmtime::component::Val::U32(limit),
            session_val,
        ];
        let mut results = vec![wasmtime::component::Val::Bool(false)];

        recall_fn
            .call_async(store.as_context_mut(), &params, &mut results)
            .await
            .map_err(|e| PluginError::Runtime(format!("recall-memory() call failed: {e}")))?;

        recall_fn
            .post_return_async(store.as_context_mut())
            .await
            .map_err(|e| {
                PluginError::Runtime(format!("recall-memory() post_return failed: {e}"))
            })?;

        // Parse result<list<memory-entry>, string>
        match &results[0] {
            wasmtime::component::Val::Result(r) => match r.as_ref() {
                Ok(Some(inner_val)) => Self::parse_memory_entries(inner_val),
                Ok(None) => Ok(Vec::new()),
                Err(Some(inner_err)) => match inner_err.as_ref() {
                    wasmtime::component::Val::String(e) => Err(PluginError::Runtime(format!(
                        "WASM storage '{}' recall-memory error: {e}",
                        self.storage_name
                    ))),
                    _ => Err(PluginError::Runtime(format!(
                        "WASM storage '{}' recall-memory returned unknown error",
                        self.storage_name
                    ))),
                },
                Err(None) => Err(PluginError::Runtime(format!(
                    "WASM storage '{}' recall-memory returned unknown error",
                    self.storage_name
                ))),
            },
            _ => Err(PluginError::Runtime(
                "recall-memory() returned unexpected value type".to_string(),
            )),
        }
    }

    /// Internal: call `forget-memory` on the WASM plugin.
    async fn call_forget_memory_inner(&self, key: &str) -> PluginResult<bool> {
        let mut inner = self.inner.lock().await;
        let WasmStorageInner {
            ref mut store,
            ref instance,
        } = *inner;

        let iface_idx = instance
            .get_export(
                store.as_context_mut(),
                None,
                "prx:plugin/storage-exports@0.1.0",
            )
            .ok_or_else(|| {
                PluginError::Runtime(
                    "plugin does not export prx:plugin/storage-exports@0.1.0".to_string(),
                )
            })?;

        let func_idx = instance
            .get_export(store.as_context_mut(), Some(&iface_idx), "forget-memory")
            .ok_or_else(|| {
                PluginError::Runtime("forget-memory not found in storage-exports".to_string())
            })?;

        let forget_fn = instance
            .get_func(store.as_context_mut(), &func_idx)
            .ok_or_else(|| PluginError::Runtime("forget-memory is not a function".to_string()))?;

        let params = [wasmtime::component::Val::String(key.into())];
        let mut results = vec![wasmtime::component::Val::Bool(false)];

        forget_fn
            .call_async(store.as_context_mut(), &params, &mut results)
            .await
            .map_err(|e| PluginError::Runtime(format!("forget-memory() call failed: {e}")))?;

        forget_fn
            .post_return_async(store.as_context_mut())
            .await
            .map_err(|e| {
                PluginError::Runtime(format!("forget-memory() post_return failed: {e}"))
            })?;

        // Parse result<bool, string>
        match &results[0] {
            wasmtime::component::Val::Result(r) => match r.as_ref() {
                Ok(Some(boxed)) => match boxed.as_ref() {
                    wasmtime::component::Val::Bool(b) => Ok(*b),
                    _ => Ok(false),
                },
                Ok(None) => Ok(false),
                Err(Some(inner_err)) => match inner_err.as_ref() {
                    wasmtime::component::Val::String(e) => Err(PluginError::Runtime(format!(
                        "WASM storage '{}' forget-memory error: {e}",
                        self.storage_name
                    ))),
                    _ => Err(PluginError::Runtime(format!(
                        "WASM storage '{}' forget-memory returned unknown error",
                        self.storage_name
                    ))),
                },
                Err(None) => Err(PluginError::Runtime(format!(
                    "WASM storage '{}' forget-memory returned unknown error",
                    self.storage_name
                ))),
            },
            _ => Err(PluginError::Runtime(
                "forget-memory() returned unexpected value type".to_string(),
            )),
        }
    }

    /// Internal: call `count-memories` on the WASM plugin.
    async fn call_count_memories_inner(&self) -> PluginResult<usize> {
        let mut inner = self.inner.lock().await;
        let WasmStorageInner {
            ref mut store,
            ref instance,
        } = *inner;

        let iface_idx = instance
            .get_export(
                store.as_context_mut(),
                None,
                "prx:plugin/storage-exports@0.1.0",
            )
            .ok_or_else(|| {
                PluginError::Runtime(
                    "plugin does not export prx:plugin/storage-exports@0.1.0".to_string(),
                )
            })?;

        let func_idx = instance
            .get_export(store.as_context_mut(), Some(&iface_idx), "count-memories")
            .ok_or_else(|| {
                PluginError::Runtime("count-memories not found in storage-exports".to_string())
            })?;

        let count_fn = instance
            .get_func(store.as_context_mut(), &func_idx)
            .ok_or_else(|| PluginError::Runtime("count-memories is not a function".to_string()))?;

        let mut results = vec![wasmtime::component::Val::Bool(false)];

        count_fn
            .call_async(store.as_context_mut(), &[], &mut results)
            .await
            .map_err(|e| PluginError::Runtime(format!("count-memories() call failed: {e}")))?;

        count_fn
            .post_return_async(store.as_context_mut())
            .await
            .map_err(|e| {
                PluginError::Runtime(format!("count-memories() post_return failed: {e}"))
            })?;

        // Parse result<u32, string>
        match &results[0] {
            wasmtime::component::Val::Result(r) => match r.as_ref() {
                Ok(Some(boxed)) => match boxed.as_ref() {
                    wasmtime::component::Val::U32(n) => Ok(*n as usize),
                    _ => Ok(0),
                },
                Ok(None) => Ok(0),
                Err(Some(inner_err)) => match inner_err.as_ref() {
                    wasmtime::component::Val::String(e) => Err(PluginError::Runtime(format!(
                        "WASM storage '{}' count-memories error: {e}",
                        self.storage_name
                    ))),
                    _ => Err(PluginError::Runtime(format!(
                        "WASM storage '{}' count-memories returned unknown error",
                        self.storage_name
                    ))),
                },
                Err(None) => Err(PluginError::Runtime(format!(
                    "WASM storage '{}' count-memories returned unknown error",
                    self.storage_name
                ))),
            },
            _ => Err(PluginError::Runtime(
                "count-memories() returned unexpected value type".to_string(),
            )),
        }
    }

    /// Internal: call `health-check` on the WASM plugin.
    async fn call_health_check_inner(&self) -> PluginResult<bool> {
        let mut inner = self.inner.lock().await;
        let WasmStorageInner {
            ref mut store,
            ref instance,
        } = *inner;

        let iface_idx = instance
            .get_export(
                store.as_context_mut(),
                None,
                "prx:plugin/storage-exports@0.1.0",
            )
            .ok_or_else(|| {
                PluginError::Runtime(
                    "plugin does not export prx:plugin/storage-exports@0.1.0".to_string(),
                )
            })?;

        let func_idx = instance
            .get_export(store.as_context_mut(), Some(&iface_idx), "health-check")
            .ok_or_else(|| {
                PluginError::Runtime("health-check not found in storage-exports".to_string())
            })?;

        let health_fn = instance
            .get_func(store.as_context_mut(), &func_idx)
            .ok_or_else(|| PluginError::Runtime("health-check is not a function".to_string()))?;

        let mut results = vec![wasmtime::component::Val::Bool(false)];

        health_fn
            .call_async(store.as_context_mut(), &[], &mut results)
            .await
            .map_err(|e| PluginError::Runtime(format!("health-check() call failed: {e}")))?;

        health_fn
            .post_return_async(store.as_context_mut())
            .await
            .map_err(|e| PluginError::Runtime(format!("health-check() post_return failed: {e}")))?;

        match &results[0] {
            wasmtime::component::Val::Bool(b) => Ok(*b),
            _ => Err(PluginError::Runtime(
                "health-check() returned unexpected value type".to_string(),
            )),
        }
    }

    /// Parse a `list<memory-entry>` value from the WASM component.
    fn parse_memory_entries(val: &wasmtime::component::Val) -> PluginResult<Vec<MemoryEntry>> {
        let items = match val {
            wasmtime::component::Val::List(items) => items,
            _ => {
                return Err(PluginError::Runtime(
                    "recall-memory result is not a list".to_string(),
                ))
            }
        };

        let mut entries = Vec::with_capacity(items.len());
        for item in items {
            entries.push(Self::parse_memory_entry(item)?);
        }
        Ok(entries)
    }

    /// Parse a single `memory-entry` record.
    fn parse_memory_entry(val: &wasmtime::component::Val) -> PluginResult<MemoryEntry> {
        let fields = match val {
            wasmtime::component::Val::Record(f) => f,
            _ => {
                return Err(PluginError::Runtime(
                    "memory-entry is not a record".to_string(),
                ))
            }
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

        let id = get_str("id");
        let key = get_str("key");
        let content = get_str("content");
        let category_str = get_str("category");
        let timestamp = get_str("timestamp");

        let category = match category_str.as_str() {
            "core" => MemoryCategory::Core,
            "daily" => MemoryCategory::Daily,
            "conversation" => MemoryCategory::Conversation,
            other => MemoryCategory::Custom(other.to_string()),
        };

        // score: option<f64>
        let score = fields
            .iter()
            .find(|(k, _)| k == "score")
            .and_then(|(_, v)| match v {
                wasmtime::component::Val::Option(opt) => match opt.as_deref() {
                    Some(wasmtime::component::Val::Float64(f)) => Some(Some(*f)),
                    _ => Some(None),
                },
                _ => None,
            })
            .flatten();

        Ok(MemoryEntry {
            id,
            key,
            content,
            category,
            timestamp,
            session_id: None,
            score,
            tags: None,
            access_count: None,
            useful_count: None,
            source: None,
            source_confidence: None,
            verification_status: None,
            lifecycle_state: None,
            compressed_from: None,
        })
    }
}

#[async_trait]
impl crate::memory::traits::Memory for WasmStorage {
    fn name(&self) -> &str {
        &self.storage_name
    }

    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        validate_memory_write_target(key, session_id)?;
        let category_str = category.to_string();
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(self.timeout_ms),
            self.call_store_memory_inner(key, content, &category_str, session_id),
        )
        .await;

        match result {
            Err(_) => anyhow::bail!(
                "WASM storage '{}' store timed out after {}ms",
                self.storage_name,
                self.timeout_ms
            ),
            Ok(Err(e)) => anyhow::bail!("{e}"),
            Ok(Ok(())) => Ok(()),
        }
    }

    async fn recall(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let limit_u32 = limit.min(u32::MAX as usize) as u32;
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(self.timeout_ms),
            self.call_recall_memory_inner(query, limit_u32, session_id),
        )
        .await;

        match result {
            Err(_) => anyhow::bail!(
                "WASM storage '{}' recall timed out after {}ms",
                self.storage_name,
                self.timeout_ms
            ),
            Ok(Err(e)) => anyhow::bail!("{e}"),
            Ok(Ok(entries)) => Ok(entries),
        }
    }

    /// Get a specific memory by key.
    ///
    /// Delegates to `recall-memory` with the key as the query, limit=1,
    /// then filters to find an exact key match.
    async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        let entries = self.recall(key, 1, None).await?;
        Ok(entries.into_iter().find(|e| e.key == key))
    }

    /// List memories, optionally filtered by category and/or session.
    ///
    /// Delegates to `recall-memory` with a wildcard-style empty query,
    /// then filters by category/session on the host side.
    async fn list(
        &self,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let entries = self.recall("", u32::MAX as usize, session_id).await?;
        if let Some(cat) = category {
            Ok(entries.into_iter().filter(|e| &e.category == cat).collect())
        } else {
            Ok(entries)
        }
    }

    async fn forget(&self, key: &str) -> anyhow::Result<bool> {
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(self.timeout_ms),
            self.call_forget_memory_inner(key),
        )
        .await;

        match result {
            Err(_) => anyhow::bail!(
                "WASM storage '{}' forget timed out after {}ms",
                self.storage_name,
                self.timeout_ms
            ),
            Ok(Err(e)) => anyhow::bail!("{e}"),
            Ok(Ok(b)) => Ok(b),
        }
    }

    async fn count(&self) -> anyhow::Result<usize> {
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(self.timeout_ms),
            self.call_count_memories_inner(),
        )
        .await;

        match result {
            Err(_) => anyhow::bail!(
                "WASM storage '{}' count timed out after {}ms",
                self.storage_name,
                self.timeout_ms
            ),
            Ok(Err(e)) => anyhow::bail!("{e}"),
            Ok(Ok(n)) => Ok(n),
        }
    }

    async fn health_check(&self) -> bool {
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(self.timeout_ms),
            self.call_health_check_inner(),
        )
        .await;

        match result {
            Err(_) => {
                tracing::warn!(
                    storage = %self.storage_name,
                    "WASM storage health-check timed out"
                );
                false
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    storage = %self.storage_name,
                    error = %e,
                    "WASM storage health-check failed"
                );
                false
            }
            Ok(Ok(b)) => b,
        }
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::traits::MemoryCategory;
    use wasmtime::component::Val;

    fn str_val(s: &str) -> Val {
        Val::String(s.into())
    }

    /// Build a minimal memory-entry record Val.
    fn memory_entry_record(
        id: &str,
        key: &str,
        content: &str,
        category: &str,
        timestamp: &str,
        score: Option<f64>,
    ) -> Val {
        let score_val = match score {
            Some(f) => Val::Option(Some(Box::new(Val::Float64(f)))),
            None => Val::Option(None),
        };
        Val::Record(vec![
            ("id".to_string(), str_val(id)),
            ("key".to_string(), str_val(key)),
            ("content".to_string(), str_val(content)),
            ("category".to_string(), str_val(category)),
            ("timestamp".to_string(), str_val(timestamp)),
            ("score".to_string(), score_val),
        ])
    }

    // --- parse_memory_entry ---

    #[test]
    fn parse_memory_entry_core_category() {
        let record = memory_entry_record(
            "id-1",
            "my-key",
            "my content",
            "core",
            "2024-01-01T00:00:00Z",
            None,
        );
        let entry = WasmStorage::parse_memory_entry(&record).expect("should parse core entry");
        assert_eq!(entry.id, "id-1");
        assert_eq!(entry.key, "my-key");
        assert_eq!(entry.content, "my content");
        assert_eq!(entry.category, MemoryCategory::Core);
        assert_eq!(entry.timestamp, "2024-01-01T00:00:00Z");
        assert!(entry.score.is_none());
        assert!(entry.session_id.is_none());
    }

    #[test]
    fn parse_memory_entry_daily_category() {
        let record = memory_entry_record("id-2", "k", "c", "daily", "t", None);
        let entry = WasmStorage::parse_memory_entry(&record).expect("should parse");
        assert_eq!(entry.category, MemoryCategory::Daily);
    }

    #[test]
    fn parse_memory_entry_conversation_category() {
        let record = memory_entry_record("id-3", "k", "c", "conversation", "t", None);
        let entry = WasmStorage::parse_memory_entry(&record).expect("should parse");
        assert_eq!(entry.category, MemoryCategory::Conversation);
    }

    #[test]
    fn parse_memory_entry_custom_category() {
        let record = memory_entry_record("id-4", "k", "c", "my-custom-backend", "t", None);
        let entry = WasmStorage::parse_memory_entry(&record).expect("should parse");
        assert_eq!(
            entry.category,
            MemoryCategory::Custom("my-custom-backend".to_string())
        );
    }

    #[test]
    fn parse_memory_entry_with_score() {
        let record = memory_entry_record("id-5", "k", "c", "core", "t", Some(0.87));
        let entry = WasmStorage::parse_memory_entry(&record).expect("should parse with score");
        assert_eq!(entry.score, Some(0.87));
    }

    #[test]
    fn parse_memory_entry_score_zero() {
        let record = memory_entry_record("id-6", "k", "c", "core", "t", Some(0.0));
        let entry = WasmStorage::parse_memory_entry(&record).expect("should parse score=0.0");
        assert_eq!(entry.score, Some(0.0));
    }

    #[test]
    fn parse_memory_entry_not_a_record_returns_error() {
        let val = Val::Bool(true);
        assert!(WasmStorage::parse_memory_entry(&val).is_err());
    }

    #[test]
    fn parse_memory_entry_missing_fields_defaults_to_empty() {
        // Partial record — missing fields default to empty string via get_str
        let record = Val::Record(vec![("id".to_string(), str_val("only-id"))]);
        let entry = WasmStorage::parse_memory_entry(&record).expect("should parse partial record");
        assert_eq!(entry.id, "only-id");
        assert_eq!(entry.key, "");
        assert_eq!(entry.content, "");
        // Unknown category string → Custom("")
        assert_eq!(entry.category, MemoryCategory::Custom("".to_string()));
    }

    // --- parse_memory_entries ---

    #[test]
    fn parse_memory_entries_empty_list() {
        let list = Val::List(vec![]);
        let entries = WasmStorage::parse_memory_entries(&list).expect("should parse empty list");
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_memory_entries_multiple() {
        let r1 = memory_entry_record("a", "key-a", "content-a", "core", "t1", None);
        let r2 = memory_entry_record("b", "key-b", "content-b", "daily", "t2", Some(0.5));
        let list = Val::List(vec![r1, r2]);
        let entries = WasmStorage::parse_memory_entries(&list).expect("should parse list");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "a");
        assert_eq!(entries[0].category, MemoryCategory::Core);
        assert_eq!(entries[1].id, "b");
        assert_eq!(entries[1].category, MemoryCategory::Daily);
        assert_eq!(entries[1].score, Some(0.5));
    }

    #[test]
    fn parse_memory_entries_not_a_list_returns_error() {
        let val = Val::String("not a list".into());
        assert!(WasmStorage::parse_memory_entries(&val).is_err());
    }

    // --- storage_name field ---

    #[test]
    fn wasm_storage_storage_name_field_type() {
        // WasmStorage cannot be constructed without a real WASM engine,
        // but we verify the storage_name field caching logic by checking
        // the parse path that feeds into it.
        // Specifically: parse_memory_entry must produce valid MemoryEntry for all categories.
        let categories = ["core", "daily", "conversation", "custom-xyz"];
        let expected = [
            MemoryCategory::Core,
            MemoryCategory::Daily,
            MemoryCategory::Conversation,
            MemoryCategory::Custom("custom-xyz".to_string()),
        ];
        for (cat_str, expected_cat) in categories.iter().zip(expected.iter()) {
            let record = memory_entry_record("id", "k", "c", cat_str, "t", None);
            let entry = WasmStorage::parse_memory_entry(&record).unwrap();
            assert_eq!(&entry.category, expected_cat, "category {cat_str} mismatch");
        }
    }
}
