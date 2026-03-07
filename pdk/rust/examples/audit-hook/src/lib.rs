//! # audit-hook
//!
//! Example PRX hook plugin: lifecycle event auditing.
//!
//! This plugin demonstrates:
//! - Implementing the `prx:plugin/hook-exports` WIT interface
//! - Listening to `prx.lifecycle.*` events (agent_start, agent_stop, tool_call, error)
//! - Using KV storage to maintain per-event counters
//! - Publishing summary events to the event bus
//! - Structured JSON payload parsing
//!
//! ## Build
//!
//! ```sh
//! cargo install cargo-component
//! rustup target add wasm32-wasip2
//! cargo component build --release
//! cp target/wasm32-wasip2/release/audit_hook.wasm plugin.wasm
//! ```

use prx_pdk::prelude::*;

// ── Plugin implementation ─────────────────────────────────────────────────────

/// Audit hook plugin. Stateless struct; all state lives in KV.
pub struct AuditHook;

impl AuditHook {
    /// Handle a lifecycle event notification from the PRX runtime.
    ///
    /// Called by the host when an event matching the subscribed topic fires.
    /// Returns `Ok(())` on success; errors are logged by the host.
    pub fn on_event_impl(event: &str, payload_json: &str) -> Result<(), String> {
        log::debug(&format!("audit-hook: received event '{event}'"));

        // Increment per-event counter in KV
        let counter_key = format!("count:{event}");
        let new_count = kv::increment(&counter_key, 1)
            .map_err(|e| format!("KV increment failed for '{counter_key}': {e}"))?;

        // Increment global total
        let total = kv::increment("count:total", 1)
            .map_err(|e| format!("KV increment failed for 'count:total': {e}"))?;

        log::info(&format!(
            "audit-hook: event='{event}' count={new_count} total={total}"
        ));

        // Record the last seen timestamp (best-effort)
        let ts = clock::now_ms();
        let ts_key = format!("last_ts:{event}");
        let _ = kv::set_json(&ts_key, &ts);

        // For tool_call events, also record the tool name
        if event.starts_with("tool_call") || event == "prx.lifecycle.tool_call" {
            if let Ok(payload) = serde_json::from_str::<JsonValue>(payload_json) {
                if let Some(tool_name) = payload["tool"].as_str() {
                    let tool_key = format!("count:tool:{tool_name}");
                    let _ = kv::increment(&tool_key, 1);
                    log::debug(&format!("audit-hook: tool '{tool_name}' invoked"));
                }
            }
        }

        // Every 100 total events, publish a summary to the event bus
        if total % 100 == 0 {
            let summary = json!({
                "total": total,
                "timestamp_ms": ts,
                "milestone": true
            });
            let _ = events::publish_json("prx.audit.milestone", &summary);
            log::info(&format!("audit-hook: published milestone summary at total={total}"));
        }

        Ok(())
    }

    /// Return the current event counts as a JSON object.
    ///
    /// Utility function for the cron or external inspection.
    pub fn get_summary() -> JsonValue {
        let total: i64 = kv::get_json("count:total").unwrap_or(0);
        let events_json: Vec<JsonValue> = kv::list_keys("count:")
            .into_iter()
            .filter(|k| k != "count:total")
            .map(|k| {
                let count: i64 = kv::get_json(&k).unwrap_or(0);
                let event_name = k.trim_start_matches("count:").to_string();
                json!({ "event": event_name, "count": count })
            })
            .collect();

        json!({
            "total": total,
            "breakdown": events_json,
            "timestamp_ms": clock::now_ms()
        })
    }
}

// ── WIT guest trait implementation (cargo-component / wasm32 only) ────────────
//
// cargo-component generates `bindings::Guest` for the `prx:plugin/hook` world.

#[cfg(target_arch = "wasm32")]
mod wasm_exports {
    use super::AuditHook;
    use bindings::Guest;

    impl Guest for AuditHook {
        fn on_event(event: String, payload_json: String) -> Result<(), String> {
            AuditHook::on_event_impl(&event, &payload_json)
        }
    }

    bindings::export!(AuditHook with_types_in bindings);
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_agent_start_event() {
        let payload = r#"{"agent": "openprx", "session": "abc123"}"#;
        let result = AuditHook::on_event_impl("prx.lifecycle.agent_start", payload);
        assert!(result.is_ok(), "expected Ok, got: {result:?}");
    }

    #[test]
    fn handles_tool_call_event_with_tool_name() {
        let payload = r#"{"tool": "weather_lookup", "args": {"city": "Tokyo"}}"#;
        let result = AuditHook::on_event_impl("prx.lifecycle.tool_call", payload);
        assert!(result.is_ok());
    }

    #[test]
    fn handles_error_event() {
        let payload = r#"{"error": "connection timeout", "code": 408}"#;
        let result = AuditHook::on_event_impl("prx.lifecycle.error", payload);
        assert!(result.is_ok());
    }

    #[test]
    fn handles_unknown_event_gracefully() {
        // Unknown events should not cause errors
        let result = AuditHook::on_event_impl("prx.custom.unknown_event", "{}");
        assert!(result.is_ok());
    }

    #[test]
    fn handles_empty_payload() {
        let result = AuditHook::on_event_impl("prx.lifecycle.agent_stop", "");
        // Should not panic even with empty payload
        assert!(result.is_ok());
    }

    #[test]
    fn get_summary_returns_valid_json() {
        let summary = AuditHook::get_summary();
        assert!(summary["total"].is_number());
        assert!(summary["breakdown"].is_array());
    }
}
