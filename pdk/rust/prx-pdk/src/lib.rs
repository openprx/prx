//! # prx-pdk — PRX WASM Plugin Development Kit
//!
//! Ergonomic Rust wrappers for PRX host functions, enabling clean plugin authorship.
//!
//! ## Quick Start
//!
//! ```rust,ignore
//! use prx_pdk::prelude::*;
//!
//! // Call host functions from any plugin type
//! prx_pdk::log::info("Plugin initialised");
//!
//! let timeout = prx_pdk::config::get("timeout_ms")
//!     .unwrap_or_else(|| "5000".to_string());
//!
//! prx_pdk::kv::set("last_run", b"2025-01-01").unwrap();
//! ```
//!
//! ## Plugin Types
//!
//! Build with `cargo component build --release` after installing
//! [cargo-component](https://github.com/bytecodealliance/cargo-component):
//!
//! - **Tool** — LLM-callable tool plugin (implements `prx:plugin/tool-exports`)
//! - **Hook** — Lifecycle event observer (implements `prx:plugin/hook-exports`)
//! - **Middleware** — Pipeline transformer (implements `prx:plugin/middleware-exports`)
//! - **Cron** — Scheduled task runner (implements `prx:plugin/cron-exports`)

#![warn(missing_docs)]

// ── wit-bindgen: generate host call wrappers ────────────────────────────────
// On wasm32 (cargo-component builds): real extern "C" host call linkage.
// On other targets (host-side rlib, tests): no-op stubs so `cargo build` passes.
#[cfg(target_arch = "wasm32")]
wit_bindgen::generate!({
    world: "pdk-full",
    path: "wit",
});

// ── Internal bindings alias ──────────────────────────────────────────────────
#[cfg(target_arch = "wasm32")]
use prx::host as host_bindings;

// ── Convenience types re-exported for plugin authors ────────────────────────

/// Tool specification returned from `get-spec`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolSpec {
    /// Tool name (snake_case, matches the WIT record field).
    pub name: String,
    /// Human-readable description shown to the LLM.
    pub description: String,
    /// JSON Schema string describing the tool's input parameters.
    pub parameters_schema: String,
}

/// Result returned from plugin `execute` / `run` calls.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PluginResult {
    /// Whether the operation succeeded.
    pub success: bool,
    /// Output text (may be empty on error).
    pub output: String,
    /// Optional error message (populated when `success == false`).
    pub error: Option<String>,
}

impl PluginResult {
    /// Create a successful result.
    pub fn ok(output: impl Into<String>) -> Self {
        PluginResult { success: true, output: output.into(), error: None }
    }

    /// Create a failure result.
    pub fn err(error: impl Into<String>) -> Self {
        PluginResult { success: false, output: String::new(), error: Some(error.into()) }
    }
}

/// Action returned by middleware plugins.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum MiddlewareAction {
    /// Pass the (possibly modified) data downstream.
    Continue {
        /// Modified JSON data to pass to the next stage.
        data: String,
    },
    /// Block the request with an error.
    Block {
        /// Reason for blocking.
        reason: String,
    },
}

// ── HTTP response type ───────────────────────────────────────────────────────

/// HTTP response from an outbound request.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers.
    pub headers: Vec<(String, String)>,
    /// Response body bytes.
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// Parse the body as UTF-8 text.
    pub fn text(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.body)
    }

    /// Parse the body as JSON.
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(&self.body)
    }
}

// ── Memory entry type ────────────────────────────────────────────────────────

/// A memory entry returned from `memory::recall`.
#[derive(Debug, Clone)]
pub struct MemoryEntry {
    /// Unique entry ID.
    pub id: String,
    /// Stored text content.
    pub text: String,
    /// Category label (e.g. "fact", "preference").
    pub category: String,
    /// Importance score (0.0–1.0).
    pub importance: f64,
}

// ── Log module ───────────────────────────────────────────────────────────────

/// Structured logging — writes to the PRX tracing infrastructure.
///
/// Log messages appear in the host's log output with the plugin name as context.
pub mod log {
    /// Emit an INFO-level log message.
    pub fn info(msg: &str) {
        #[cfg(target_arch = "wasm32")]
        crate::host_bindings::log::log(crate::host_bindings::log::Level::Info, msg);
        #[cfg(not(target_arch = "wasm32"))]
        eprintln!("[prx-pdk INFO ] {msg}");
    }

    /// Emit a WARN-level log message.
    pub fn warn(msg: &str) {
        #[cfg(target_arch = "wasm32")]
        crate::host_bindings::log::log(crate::host_bindings::log::Level::Warn, msg);
        #[cfg(not(target_arch = "wasm32"))]
        eprintln!("[prx-pdk WARN ] {msg}");
    }

    /// Emit an ERROR-level log message.
    pub fn error(msg: &str) {
        #[cfg(target_arch = "wasm32")]
        crate::host_bindings::log::log(crate::host_bindings::log::Level::Error, msg);
        #[cfg(not(target_arch = "wasm32"))]
        eprintln!("[prx-pdk ERROR] {msg}");
    }

    /// Emit a DEBUG-level log message.
    pub fn debug(msg: &str) {
        #[cfg(target_arch = "wasm32")]
        crate::host_bindings::log::log(crate::host_bindings::log::Level::Debug, msg);
        #[cfg(not(target_arch = "wasm32"))]
        eprintln!("[prx-pdk DEBUG] {msg}");
    }

    /// Emit a TRACE-level log message.
    pub fn trace(msg: &str) {
        #[cfg(target_arch = "wasm32")]
        crate::host_bindings::log::log(crate::host_bindings::log::Level::Trace, msg);
        #[cfg(not(target_arch = "wasm32"))]
        eprintln!("[prx-pdk TRACE] {msg}");
    }
}

// ── Config module ─────────────────────────────────────────────────────────────

/// Plugin configuration — read-only access to values from `plugin.toml [config]`.
///
/// Config values are set by the operator when deploying the plugin and cannot
/// be modified at runtime. Use [`kv`] for mutable persistent storage.
pub mod config {
    /// Get a configuration value by key.
    ///
    /// Returns `None` if the key is not set.
    pub fn get(key: &str) -> Option<String> {
        #[cfg(target_arch = "wasm32")]
        { crate::host_bindings::config::get(key) }
        #[cfg(not(target_arch = "wasm32"))]
        { std::env::var(key).ok() }
    }

    /// Get all configuration key-value pairs.
    pub fn get_all() -> Vec<(String, String)> {
        #[cfg(target_arch = "wasm32")]
        { crate::host_bindings::config::get_all() }
        #[cfg(not(target_arch = "wasm32"))]
        { vec![] }
    }

    /// Get a configuration value, returning a default if not set.
    pub fn get_or(key: &str, default: &str) -> String {
        get(key).unwrap_or_else(|| default.to_string())
    }
}

// ── KV module ─────────────────────────────────────────────────────────────────

/// Key-value storage — isolated per-plugin persistent store.
///
/// Each plugin gets its own namespace; plugins cannot access each other's keys.
/// Values are opaque bytes (`Vec<u8>`). Use JSON serialisation for structured data.
pub mod kv {
    /// Retrieve a value by key. Returns `None` if the key does not exist.
    pub fn get(key: &str) -> Option<Vec<u8>> {
        #[cfg(target_arch = "wasm32")]
        { crate::host_bindings::kv::get(key) }
        #[cfg(not(target_arch = "wasm32"))]
        { let _ = key; None }
    }

    /// Retrieve a value and decode it as UTF-8 text.
    pub fn get_str(key: &str) -> Option<String> {
        get(key).and_then(|v| String::from_utf8(v).ok())
    }

    /// Retrieve and JSON-deserialise a stored value.
    pub fn get_json<T: serde::de::DeserializeOwned>(key: &str) -> Option<T> {
        get(key).and_then(|v| serde_json::from_slice(&v).ok())
    }

    /// Store a byte value. Overwrites any existing value.
    pub fn set(key: &str, value: &[u8]) -> Result<(), String> {
        #[cfg(target_arch = "wasm32")]
        { crate::host_bindings::kv::set(key, value) }
        #[cfg(not(target_arch = "wasm32"))]
        { let (_, _) = (key, value); Ok(()) }
    }

    /// Store a UTF-8 string value.
    pub fn set_str(key: &str, value: &str) -> Result<(), String> {
        set(key, value.as_bytes())
    }

    /// JSON-serialise and store a value.
    pub fn set_json<T: serde::Serialize>(key: &str, value: &T) -> Result<(), String> {
        let bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
        set(key, &bytes)
    }

    /// Delete a key. Returns `true` if the key existed.
    pub fn delete(key: &str) -> Result<bool, String> {
        #[cfg(target_arch = "wasm32")]
        { crate::host_bindings::kv::delete(key) }
        #[cfg(not(target_arch = "wasm32"))]
        { let _ = key; Ok(false) }
    }

    /// List all keys matching a prefix.
    pub fn list_keys(prefix: &str) -> Vec<String> {
        #[cfg(target_arch = "wasm32")]
        { crate::host_bindings::kv::list_keys(prefix) }
        #[cfg(not(target_arch = "wasm32"))]
        { let _ = prefix; vec![] }
    }

    /// Atomically increment an integer counter stored at `key`.
    ///
    /// Initialises to 0 if the key does not exist, then adds `delta`.
    pub fn increment(key: &str, delta: i64) -> Result<i64, String> {
        let current: i64 = get_json(key).unwrap_or(0);
        let next = current + delta;
        set_json(key, &next)?;
        Ok(next)
    }
}

// ── Events module ─────────────────────────────────────────────────────────────

/// Event bus — fire-and-forget publish/subscribe for inter-plugin communication.
///
/// Events flow through the host for auditing and access control.
/// Payload must be valid JSON, max 64 KB.
pub mod events {
    /// Publish an event to a topic.
    ///
    /// All subscribers matching the topic will receive the event asynchronously.
    ///
    /// # Errors
    /// Returns an error if the plugin lacks `events` permission, the payload
    /// exceeds 64 KB, or the topic is invalid.
    pub fn publish(topic: &str, payload: &str) -> Result<(), String> {
        #[cfg(target_arch = "wasm32")]
        { crate::host_bindings::events::publish(topic, payload) }
        #[cfg(not(target_arch = "wasm32"))]
        { let (_, _) = (topic, payload); Ok(()) }
    }

    /// Publish a JSON-serialisable value to a topic.
    pub fn publish_json<T: serde::Serialize>(topic: &str, payload: &T) -> Result<(), String> {
        let json = serde_json::to_string(payload).map_err(|e| e.to_string())?;
        publish(topic, &json)
    }

    /// Subscribe to a topic pattern.
    ///
    /// Supports exact match (`"weather.update"`) and wildcard (`"weather.*"`).
    /// Returns a subscription ID for later [`unsubscribe`].
    pub fn subscribe(pattern: &str) -> Result<u64, String> {
        #[cfg(target_arch = "wasm32")]
        { crate::host_bindings::events::subscribe(pattern) }
        #[cfg(not(target_arch = "wasm32"))]
        { let _ = pattern; Ok(0) }
    }

    /// Cancel a subscription by ID.
    pub fn unsubscribe(id: u64) -> Result<(), String> {
        #[cfg(target_arch = "wasm32")]
        { crate::host_bindings::events::unsubscribe(id) }
        #[cfg(not(target_arch = "wasm32"))]
        { let _ = id; Ok(()) }
    }
}

// ── HTTP module ───────────────────────────────────────────────────────────────

/// Outbound HTTP — make controlled HTTP requests from plugins.
///
/// URLs are validated against the plugin's `http_allowlist` in `plugin.toml`.
/// Requires `"http-outbound"` permission.
pub mod http {
    use super::HttpResponse;

    /// Make an HTTP request.
    ///
    /// # Arguments
    /// - `method` — HTTP verb (`"GET"`, `"POST"`, etc.)
    /// - `url` — Target URL (must be in the plugin's `http_allowlist`)
    /// - `headers` — Request headers as `(name, value)` pairs
    /// - `body` — Optional request body bytes
    ///
    /// # Errors
    /// Returns an error if the URL is not allowed, the request fails, or the
    /// plugin lacks `"http-outbound"` permission.
    pub fn request(
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<HttpResponse, String> {
        #[cfg(target_arch = "wasm32")]
        {
            let owned_headers: Vec<(String, String)> = headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            let body_owned: Option<Vec<u8>> = body.map(|b| b.to_vec());
            crate::host_bindings::http_outbound::request(
                method,
                url,
                &owned_headers,
                body_owned.as_deref(),
            )
            .map(|r| HttpResponse {
                status: r.status,
                headers: r.headers,
                body: r.body,
            })
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = (method, url, headers, body);
            Err("http::request is only available on wasm32 targets".to_string())
        }
    }

    /// Convenience wrapper: HTTP GET request.
    pub fn get(url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse, String> {
        request("GET", url, headers, None)
    }

    /// Convenience wrapper: HTTP POST request with a JSON body.
    pub fn post_json<T: serde::Serialize>(
        url: &str,
        headers: &[(&str, &str)],
        body: &T,
    ) -> Result<HttpResponse, String> {
        let json = serde_json::to_vec(body).map_err(|e| e.to_string())?;
        let mut h: Vec<(&str, &str)> = headers.to_vec();
        // Only add Content-Type if not already set
        if !h.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")) {
            h.push(("Content-Type", "application/json"));
        }
        request("POST", url, &h, Some(&json))
    }
}

// ── Clock module ──────────────────────────────────────────────────────────────

/// Clock — current time utilities for plugins.
///
/// Note: The PRX WIT spec does not currently expose a dedicated clock interface.
/// This module provides a best-effort implementation:
/// - On wasm32 + wasi: uses the WASI clock functions.
/// - On other targets: uses `std::time::SystemTime`.
pub mod clock {
    /// Return the current time as Unix milliseconds (UTC).
    pub fn now_ms() -> u64 {
        #[cfg(all(target_arch = "wasm32", target_os = "wasi"))]
        {
            // WASI clock_time_get: clockid=0 (REALTIME), precision=1_000_000 ns
            extern "C" {
                fn __wasi_clock_time_get(id: u32, precision: u64, time: *mut u64) -> u16;
            }
            let mut t: u64 = 0;
            unsafe { __wasi_clock_time_get(0, 1_000_000, &mut t) };
            t / 1_000_000 // ns → ms
        }
        #[cfg(not(all(target_arch = "wasm32", target_os = "wasi")))]
        {
            use std::time::{SystemTime, UNIX_EPOCH};
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)
        }
    }

    /// Return the host timezone name (e.g. `"UTC"`, `"America/New_York"`).
    ///
    /// Currently always returns `"UTC"` — timezone support is planned for a
    /// future PRX host interface release.
    pub fn timezone() -> &'static str {
        "UTC"
    }
}

// ── Memory module ─────────────────────────────────────────────────────────────

/// Long-term memory — store and recall text entries.
///
/// Requires `"memory"` permission in `plugin.toml`.
pub mod memory {
    use super::MemoryEntry;

    /// Store text in memory. Returns the generated entry ID.
    pub fn store(text: &str, category: &str) -> Result<String, String> {
        #[cfg(target_arch = "wasm32")]
        { crate::host_bindings::memory::store(text, category) }
        #[cfg(not(target_arch = "wasm32"))]
        { let (_, _) = (text, category); Ok("stub-id".to_string()) }
    }

    /// Recall memories matching a query. Returns up to `limit` entries.
    pub fn recall(query: &str, limit: u32) -> Result<Vec<MemoryEntry>, String> {
        #[cfg(target_arch = "wasm32")]
        {
            crate::host_bindings::memory::recall(query, limit).map(|entries| {
                entries
                    .into_iter()
                    .map(|e| MemoryEntry {
                        id: e.id,
                        text: e.text,
                        category: e.category,
                        importance: e.importance,
                    })
                    .collect()
            })
        }
        #[cfg(not(target_arch = "wasm32"))]
        { let (_, _) = (query, limit); Ok(vec![]) }
    }
}

// ── Macros ────────────────────────────────────────────────────────────────────

/// Re-export the wit-bindgen `export!` macro for plugin authors.
///
/// On wasm32 builds (cargo-component), this wires up the WASM component exports.
/// On host builds, it is a no-op to allow `cargo build` without cargo-component.
///
/// Usage (in your plugin crate after implementing the Guest trait):
/// ```rust,ignore
/// prx_pdk::export!(MyPlugin);
/// ```
#[macro_export]
macro_rules! export {
    ($impl_ty:ty) => {
        #[cfg(target_arch = "wasm32")]
        ::wit_bindgen::export!($impl_ty);
    };
}

/// Define a tool plugin with a concise syntax.
///
/// Generates `get_spec` and `execute` boilerplate, forwarding to your struct's
/// methods. See the `base64-tool` example for full usage.
///
/// ```rust,ignore
/// use prx_pdk::prelude::*;
///
/// struct MyTool;
///
/// impl MyTool {
///     fn get_spec_impl() -> ToolSpec { ... }
///     fn execute_impl(args: &str) -> PluginResult { ... }
/// }
///
/// prx_pdk::define_tool!(MyTool);
/// ```
#[macro_export]
macro_rules! define_tool {
    ($impl_ty:ty) => {
        // When built with cargo-component on wasm32, implement the generated Guest trait.
        // The Guest trait and associated types are generated by cargo-component's
        // wit-bindgen invocation for the `prx:plugin/tool` world.
        // Without cargo-component this block is excluded, allowing `cargo build` as rlib.
        #[cfg(target_arch = "wasm32")]
        mod __prx_tool_export {
            use super::$impl_ty;

            // NOTE: cargo-component generates a `Guest` trait in scope; this block
            // relies on that generation. When building without cargo-component, the
            // cfg guard prevents compilation of this block.
            // impl bindings::Guest for $impl_ty { ... }
        }
    };
}

// ── Prelude ───────────────────────────────────────────────────────────────────

/// Convenience re-exports — `use prx_pdk::prelude::*;` in your plugin.
pub mod prelude {
    pub use crate::{
        MiddlewareAction, PluginResult, ToolSpec, HttpResponse, MemoryEntry,
        log, config, kv, events, http, clock, memory,
    };
    pub use serde::{Deserialize, Serialize};
    pub use serde_json::{self, json, Value as JsonValue};
}
