//! Storage capability — bridges WASM storage plugins to the PRX `Memory` trait.
//!
//! WASM storage plugins export functions for storing, recalling, and forgetting
//! memory entries. The host delegates memory operations to the plugin when it is
//! configured as the active Memory backend.
//!
//! # Trap recovery
//!
//! The adapter's store is owned by a [`Supervisor`]: a trapped backend is
//! rebuilt on a brand-new `Store` by the next operation, rebuilds are budgeted
//! inside a rolling window, and once the budget is spent the backend is
//! quarantined until the next plugin generation replaces it. Every failure is
//! an error: a write is never acknowledged unless the guest completed it, a
//! read never turns a failure into an empty result, and `health_check` reports
//! `false`. A rebuilt guest starts with empty linear memory, so a backend that
//! keeps entries there comes back empty; every storage rebuild is therefore
//! announced on `prx.plugin.storage_restarted`.

use async_trait::async_trait;
use std::collections::HashSet;
use std::sync::Arc;
use wasmtime::AsContextMut;

use super::recovery::{CallOutcome, CapabilityKind, GuestOp, Respawn, Supervisor};
use crate::memory::traits::{MemoryCategory, MemoryEntry, validate_memory_write_target};
use crate::plugins::error::{PluginError, PluginResult};
use crate::plugins::host::{HostState, apply_store_epoch_deadline};
use crate::plugins::manifest::PluginManifest;

const STORAGE_INTERFACE: &str = "prx:plugin/storage-exports@0.1.0";

/// A loaded WASM storage plugin instance.
#[derive(Clone)]
pub struct WasmStorage {
    /// The cached storage backend name (returned by `name()` at load time).
    storage_name: String,
    /// The store, its instance and their recovery state.
    supervisor: Arc<Supervisor>,
    /// Timeout for storage calls (milliseconds).
    timeout_ms: u64,
}

/// Look up one export of the storage interface.
fn storage_export(
    store: &mut wasmtime::Store<HostState>,
    instance: &wasmtime::component::Instance,
    export: &str,
) -> Result<wasmtime::component::Func, PluginError> {
    let iface_idx = instance
        .get_export_index(store.as_context_mut(), None, STORAGE_INTERFACE)
        .ok_or_else(|| PluginError::Runtime(format!("plugin does not export {STORAGE_INTERFACE}")))?;
    let func_idx = instance
        .get_export_index(store.as_context_mut(), Some(&iface_idx), export)
        .ok_or_else(|| PluginError::Runtime(format!("{export} not found in storage-exports")))?;
    instance
        .get_func(store.as_context_mut(), &func_idx)
        .ok_or_else(|| PluginError::Runtime(format!("{export} is not a function")))
}

/// One export of the storage interface: its parameters and how to read its
/// single result value. Every storage operation reaches the guest through
/// [`StorageOp`]. Exports own their arguments because the operation runs on a
/// task of its own, detached from the caller (see `WasmStorage::request`).
trait StorageExport: Send + 'static {
    type Output: Send;
    const EXPORT: &'static str;
    /// Whether the export answers from stored entries. A read made against a
    /// store rebuilt by that very call would answer from an empty guest.
    const READS_ENTRIES: bool;

    fn params(&self) -> Vec<wasmtime::component::Val>;

    fn parse(&self, value: &wasmtime::component::Val) -> PluginResult<Self::Output>;
}

/// Adapts one storage export to the supervisor's [`GuestOp`].
struct StorageOp<T>(T);

impl<T: StorageExport> GuestOp for StorageOp<T> {
    type Output = T::Output;

    fn operation(&self) -> &'static str {
        T::EXPORT
    }

    fn invoke<'a>(
        self,
        store: &'a mut wasmtime::Store<HostState>,
        instance: &'a wasmtime::component::Instance,
    ) -> futures_util::future::BoxFuture<'a, CallOutcome<T::Output>>
    where
        Self: 'a,
    {
        Box::pin(async move {
            let func = match storage_export(store, instance, T::EXPORT) {
                Ok(func) => func,
                Err(error) => return CallOutcome::GuestError(error),
            };
            let params = self.0.params();
            let mut results = [wasmtime::component::Val::Bool(false)];
            if let Err(e) = func.call_async(store.as_context_mut(), &params, &mut results).await {
                return super::recovery::unwound(store, &e, || format!("{}() call failed: {e}", T::EXPORT));
            }
            let [value] = results;
            match self.0.parse(&value) {
                Ok(output) => CallOutcome::Completed(output),
                Err(error) => CallOutcome::GuestError(error),
            }
        })
    }
}

/// Turn a guest `result<_, string>` error arm into the adapter's error.
fn guest_error(storage_name: &str, export: &str, error: Option<&wasmtime::component::Val>) -> PluginError {
    match error {
        Some(wasmtime::component::Val::String(e)) => {
            PluginError::Runtime(format!("WASM storage '{storage_name}' {export} error: {e}"))
        }
        _ => PluginError::Runtime(format!("WASM storage '{storage_name}' {export} returned unknown error")),
    }
}

fn unexpected(export: &str) -> PluginError {
    PluginError::Runtime(format!("{export}() returned unexpected value type"))
}

fn optional_string(value: Option<&str>) -> wasmtime::component::Val {
    wasmtime::component::Val::Option(value.map(|s| Box::new(wasmtime::component::Val::String(s.into()))))
}

/// `store-memory(key, content, category, session-id) -> result<_, string>`.
struct StoreMemoryCall {
    storage_name: String,
    key: String,
    content: String,
    category: String,
    session_id: Option<String>,
}

impl StorageExport for StoreMemoryCall {
    type Output = ();
    const EXPORT: &'static str = "store-memory";
    const READS_ENTRIES: bool = false;

    fn params(&self) -> Vec<wasmtime::component::Val> {
        vec![
            wasmtime::component::Val::String(self.key.as_str().into()),
            wasmtime::component::Val::String(self.content.as_str().into()),
            wasmtime::component::Val::String(self.category.as_str().into()),
            optional_string(self.session_id.as_deref()),
        ]
    }

    fn parse(&self, value: &wasmtime::component::Val) -> PluginResult<()> {
        match value {
            wasmtime::component::Val::Result(Ok(_)) => Ok(()),
            wasmtime::component::Val::Result(Err(error)) => {
                Err(guest_error(&self.storage_name, Self::EXPORT, error.as_deref()))
            }
            _ => Err(unexpected(Self::EXPORT)),
        }
    }
}

/// `recall-memory(query, limit, session-id) -> result<list<memory-entry>, string>`.
struct RecallMemoryCall {
    storage_name: String,
    query: String,
    limit: u32,
    session_id: Option<String>,
}

impl StorageExport for RecallMemoryCall {
    type Output = Vec<MemoryEntry>;
    const EXPORT: &'static str = "recall-memory";
    const READS_ENTRIES: bool = true;

    fn params(&self) -> Vec<wasmtime::component::Val> {
        vec![
            wasmtime::component::Val::String(self.query.as_str().into()),
            wasmtime::component::Val::U32(self.limit),
            optional_string(self.session_id.as_deref()),
        ]
    }

    fn parse(&self, value: &wasmtime::component::Val) -> PluginResult<Vec<MemoryEntry>> {
        match value {
            wasmtime::component::Val::Result(Ok(Some(entries))) => WasmStorage::parse_memory_entries(entries),
            wasmtime::component::Val::Result(Ok(None)) => Ok(Vec::new()),
            wasmtime::component::Val::Result(Err(error)) => {
                Err(guest_error(&self.storage_name, Self::EXPORT, error.as_deref()))
            }
            _ => Err(unexpected(Self::EXPORT)),
        }
    }
}

/// `forget-memory(key) -> result<bool, string>`.
struct ForgetMemoryCall {
    storage_name: String,
    key: String,
}

impl StorageExport for ForgetMemoryCall {
    type Output = bool;
    const EXPORT: &'static str = "forget-memory";
    const READS_ENTRIES: bool = false;

    fn params(&self) -> Vec<wasmtime::component::Val> {
        vec![wasmtime::component::Val::String(self.key.as_str().into())]
    }

    fn parse(&self, value: &wasmtime::component::Val) -> PluginResult<bool> {
        match value {
            wasmtime::component::Val::Result(Ok(Some(boxed))) => match boxed.as_ref() {
                wasmtime::component::Val::Bool(b) => Ok(*b),
                _ => Ok(false),
            },
            wasmtime::component::Val::Result(Ok(None)) => Ok(false),
            wasmtime::component::Val::Result(Err(error)) => {
                Err(guest_error(&self.storage_name, Self::EXPORT, error.as_deref()))
            }
            _ => Err(unexpected(Self::EXPORT)),
        }
    }
}

/// `count-memories() -> result<u32, string>`.
struct CountMemoriesCall {
    storage_name: String,
}

impl StorageExport for CountMemoriesCall {
    type Output = usize;
    const EXPORT: &'static str = "count-memories";
    const READS_ENTRIES: bool = true;

    fn params(&self) -> Vec<wasmtime::component::Val> {
        Vec::new()
    }

    fn parse(&self, value: &wasmtime::component::Val) -> PluginResult<usize> {
        match value {
            wasmtime::component::Val::Result(Ok(Some(boxed))) => match boxed.as_ref() {
                wasmtime::component::Val::U32(n) => Ok(*n as usize),
                _ => Ok(0),
            },
            wasmtime::component::Val::Result(Ok(None)) => Ok(0),
            wasmtime::component::Val::Result(Err(error)) => {
                Err(guest_error(&self.storage_name, Self::EXPORT, error.as_deref()))
            }
            _ => Err(unexpected(Self::EXPORT)),
        }
    }
}

/// `health-check() -> bool`.
struct HealthCheckCall;

impl StorageExport for HealthCheckCall {
    type Output = bool;
    const EXPORT: &'static str = "health-check";
    const READS_ENTRIES: bool = false;

    fn params(&self) -> Vec<wasmtime::component::Val> {
        Vec::new()
    }

    fn parse(&self, value: &wasmtime::component::Val) -> PluginResult<bool> {
        match value {
            wasmtime::component::Val::Bool(b) => Ok(*b),
            _ => Err(unexpected(Self::EXPORT)),
        }
    }
}

// SAFETY: All results[N] accesses index a Vec that was just constructed with
// exactly the right number of elements (e.g. `vec![Val::Bool(false)]`),
// so the index is always valid.
#[allow(clippy::indexing_slicing)]
impl WasmStorage {
    /// Create a new `WasmStorage` from a compiled WASM component.
    ///
    /// Steps:
    /// 1. Build `HostState` from the manifest permissions/config.
    /// 2. Instantiate the component.
    /// 3. Call `name()` export to cache the storage backend name.
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
            CapabilityKind::Storage,
            engine,
            component,
            manifest,
            granted_permissions,
            None,
            event_bus,
            Self::register_host_functions,
        );
        let mut live = respawn.instantiate().await?;

        // Cache the storage backend name at load time. The probe gets its own
        // full fuel budget, like every later call.
        apply_store_epoch_deadline(&mut live.store, timeout_ms);
        live.store
            .set_fuel(manifest.resources.max_fuel)
            .map_err(|e| PluginError::Instantiation(format!("failed to set fuel: {e}")))?;
        let storage_name = Self::call_name(&live.instance, &mut live.store).await?;

        tracing::info!(
            plugin = %manifest.plugin.name,
            storage = %storage_name,
            "WASM storage backend registered"
        );

        Ok(Self {
            storage_name,
            supervisor: Arc::new(
                Supervisor::new(respawn, live)
                    .with_bounded_lock_wait()
                    .with_restart_announcements(),
            ),
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
    fn register_host_functions(linker: &mut wasmtime::component::Linker<HostState>) -> PluginResult<()> {
        super::common::register_common_host_functions(linker)
    }

    /// Call the `name` export to get the storage backend name.
    async fn call_name(
        instance: &wasmtime::component::Instance,
        store: &mut wasmtime::Store<HostState>,
    ) -> PluginResult<String> {
        let iface_idx = instance
            .get_export_index(store.as_context_mut(), None, "prx:plugin/storage-exports@0.1.0")
            .ok_or_else(|| {
                PluginError::Instantiation("plugin does not export prx:plugin/storage-exports@0.1.0".to_string())
            })?;

        let func_idx = instance
            .get_export_index(store.as_context_mut(), Some(&iface_idx), "name")
            .ok_or_else(|| PluginError::Instantiation("name not found in storage-exports".to_string()))?;

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

    /// Run one storage operation through the supervisor. Every failure is an
    /// error: a write is never acknowledged unless the guest completed it and
    /// a read never turns a failure into an empty answer.
    async fn request<T: StorageExport>(
        &self,
        verb: &str,
        payload_bytes: usize,
        export: T,
    ) -> anyhow::Result<T::Output> {
        if !self.supervisor.admit_payload(T::EXPORT, payload_bytes) {
            return Err(anyhow::Error::from(PluginError::ResourceLimit(format!(
                "WASM storage '{}' {verb} of {payload_bytes} bytes exceeds the {}-byte payload cap \
                     (resources.max_payload_bytes)",
                self.storage_name,
                self.supervisor.max_payload_bytes()
            ))));
        }
        // Detached (`Supervisor::call_detached_noting_rebuild`): a caller that
        // goes away — an aborted turn, another plugin's host call timed out
        // while it waited on this memory backend, a caller-side deadline — no
        // longer drops the guest call mid-run, which would poison the store
        // and, through the rebuild that follows, wipe every entry a
        // guest-memory backend holds.
        let (outcome, rebuilt) = self.supervisor.call_detached_noting_rebuild(StorageOp(export)).await;
        if rebuilt {
            tracing::warn!(
                storage = %self.storage_name,
                plugin = %self.supervisor.plugin_name(),
                operation = %verb,
                "WASM storage backend rebuilt on a fresh store; entries the guest kept in its own memory are gone"
            );
            // The memory layer has no other way to tell "no memories" from
            // "the memories were just lost": the first read after a rebuild
            // says so instead of answering from the empty guest.
            if T::READS_ENTRIES && outcome.is_ok() {
                anyhow::bail!(
                    "WASM storage '{}' was rebuilt on a fresh store before this {verb}: entries the guest kept \
                     in its own memory are gone and the store only holds what was written since; retry to read \
                     the rebuilt store",
                    self.storage_name
                );
            }
        }
        match outcome {
            Ok(output) => Ok(output),
            // A guest call that ran past its budget and an operation that could
            // not get the store within that budget read the same, as they
            // always have; only the first is charged to the plugin.
            Err(error @ PluginError::Timeout(_)) => Err(anyhow::Error::from(error).context(format!(
                "WASM storage '{}' {verb} timed out after {}ms",
                self.storage_name, self.timeout_ms
            ))),
            // Keep the type, so callers such as the reliability layer can
            // tell a quarantine or a refused payload apart without parsing text.
            Err(error) => Err(error.into()),
        }
    }

    /// Plugin that exports this storage backend.
    pub fn plugin_name(&self) -> &str {
        self.supervisor.plugin_name()
    }

    /// Per-adapter health for `wasm_plugins_status`.
    pub fn diagnostics(&self) -> serde_json::Value {
        self.supervisor.diagnostics(serde_json::json!({
            "plugin": self.supervisor.plugin_name(),
            "storage": self.storage_name,
        }))
    }

    pub fn is_quarantined(&self) -> bool {
        self.supervisor.is_quarantined()
    }

    /// Parse a `list<memory-entry>` value from the WASM component.
    fn parse_memory_entries(val: &wasmtime::component::Val) -> PluginResult<Vec<MemoryEntry>> {
        let items = match val {
            wasmtime::component::Val::List(items) => items,
            _ => {
                return Err(PluginError::Runtime("recall-memory result is not a list".to_string()));
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
                return Err(PluginError::Runtime("memory-entry is not a record".to_string()));
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
        let payload_bytes = key
            .len()
            .saturating_add(content.len())
            .saturating_add(category_str.len())
            .saturating_add(session_id.map_or(0, str::len));
        self.request(
            "store",
            payload_bytes,
            StoreMemoryCall {
                storage_name: self.storage_name.clone(),
                key: key.to_string(),
                content: content.to_string(),
                category: category_str,
                session_id: session_id.map(str::to_string),
            },
        )
        .await
    }

    async fn recall(&self, query: &str, limit: usize, session_id: Option<&str>) -> anyhow::Result<Vec<MemoryEntry>> {
        let limit_u32 = limit.min(u32::MAX as usize) as u32;
        let payload_bytes = query.len().saturating_add(session_id.map_or(0, str::len));
        self.request(
            "recall",
            payload_bytes,
            RecallMemoryCall {
                storage_name: self.storage_name.clone(),
                query: query.to_string(),
                limit: limit_u32,
                session_id: session_id.map(str::to_string),
            },
        )
        .await
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
        self.request(
            "forget",
            key.len(),
            ForgetMemoryCall {
                storage_name: self.storage_name.clone(),
                key: key.to_string(),
            },
        )
        .await
    }

    async fn count(&self) -> anyhow::Result<usize> {
        self.request(
            "count",
            0,
            CountMemoriesCall {
                storage_name: self.storage_name.clone(),
            },
        )
        .await
    }

    async fn health_check(&self) -> bool {
        match self.request("health-check", 0, HealthCheckCall).await {
            Ok(healthy) => healthy,
            Err(error) => {
                tracing::warn!(
                    storage = %self.storage_name,
                    error = %error,
                    "WASM storage health-check failed"
                );
                false
            }
        }
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        clippy::unwrap_used,
        clippy::print_stdout,
        clippy::print_stderr,
        clippy::disallowed_types,
        clippy::disallowed_methods,
        clippy::needless_collect,
        clippy::unreadable_literal,
        clippy::option_if_let_else
    )]
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
        let record = memory_entry_record("id-1", "my-key", "my content", "core", "2024-01-01T00:00:00Z", None);
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
        assert_eq!(entry.category, MemoryCategory::Custom("my-custom-backend".to_string()));
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

    const MAP_STORAGE_MANIFEST: &str = r#"
[plugin]
name = "map-storage"
version = "0.1.0"
wasm = "plugin.wasm"

[[capabilities]]
type = "storage"
name = "wasm-map"

[permissions]
required = []
optional = []

[resources]
max_fuel = 1000000000
max_memory_mb = 8
max_execution_time_ms = 30000
"#;

    /// The committed `map-storage` component, which keeps every entry in its
    /// own linear memory.
    async fn map_storage() -> WasmStorage {
        let engine = crate::plugins::shared_wasm_engine().expect("test: shared wasmtime engine");
        let wasm = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("pdk/rust/examples/map-storage/plugin.wasm");
        let bytes = std::fs::read(wasm).expect("test: the committed storage fixture is readable");
        let component =
            wasmtime::component::Component::from_binary(&engine, &bytes).expect("test: the fixture is a component");
        let manifest: PluginManifest = toml::from_str(MAP_STORAGE_MANIFEST).expect("test: fixture manifest parses");
        WasmStorage::new(&engine, &component, &manifest, HashSet::new(), None)
            .await
            .expect("test: the fixture instantiates")
    }

    /// R-C3X F3: a caller that gives up on a storage operation — an aborted
    /// turn, a nested host call timed out by another plugin, a caller-side
    /// deadline — must not decide the operation's fate. The operation runs to
    /// completion (or to its own host timeout) on its own, so a dropped caller
    /// can neither poison the store mid-call nor lose a write it had already
    /// handed over.
    #[tokio::test]
    async fn a_dropped_caller_does_not_cancel_a_storage_operation() {
        use crate::memory::traits::Memory;
        use std::future::Future;
        use std::task::{Context, Waker};

        let storage = map_storage().await;
        {
            // Queue the write behind a held store lock, then abandon it.
            let guard = storage.supervisor.lock().await;
            {
                let mut write = std::pin::pin!(storage.store("handed-over", "kept", MemoryCategory::Core, None));
                assert!(
                    write
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop()))
                        .is_pending(),
                    "the write must be waiting for the store"
                );
            }
            // Let an operation that no longer has a caller reach the store queue.
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            drop(guard);
        }

        let mut entries = 0;
        for _ in 0..64 {
            entries = storage.count().await.expect("test: count");
            if entries == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(entries, 1, "a write whose caller went away must still land");
        let diagnostics = storage.diagnostics();
        assert_eq!(
            diagnostics.get("cancelled_count").and_then(serde_json::Value::as_u64),
            Some(0),
            "{diagnostics}"
        );
        assert_eq!(
            diagnostics.get("restart_count").and_then(serde_json::Value::as_u64),
            Some(0),
            "{diagnostics}"
        );
    }
}
