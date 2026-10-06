# PRX WASM Plugin Developer Guide

> **Version:** 0.1  
> **Stack:** wasmtime + Component Model + WIT  
> **Status:** Stable

---

## Table of Contents

1. [Overview](#1-overview)
2. [Quick Start](#2-quick-start)
3. [Plugin Structure](#3-plugin-structure)
4. [Capability Types](#4-capability-types)
5. [Host Functions](#5-host-functions)
6. [Permissions](#6-permissions)
7. [Resource Limits](#7-resource-limits)
8. [Multi-language Support](#8-multi-language-support)
9. [CLI Tool](#9-cli-tool-prx-plugin)
10. [Event Bus](#10-event-bus)
11. [Hot Reload](#11-hot-reload)
12. [Troubleshooting](#12-troubleshooting)

---

## 1. Overview

PRX WASM plugins are **WebAssembly components** that run inside the PRX process. They extend PRX without modifying its source code — no recompilation, no process restarts (for most changes).

The agent-facing `wasm_plugins_manage` tool owns runtime lifecycle operations.
It installs or updates only from a source directory inside the configured
workspace, and supports status/get/refresh/enable/disable/recoverable-remove.
The root `wasm_plugin_call` tool remains available even when a plugin alias is
outside the bounded provider-visible alias window.

### Why WASM?

| Property | Benefit |
|----------|---------|
| **Sandboxed** | Plugins cannot access the filesystem, network, or other plugins except via declared permissions |
| **Polyglot** | Write plugins in Rust, Python, JavaScript/TypeScript, or Go |
| **Fast** | Microsecond-level function call overhead; no IPC, no serialization over network |
| **Safe** | Resource limits (fuel, memory, timeout) per plugin; a crashing plugin cannot crash PRX |
| **Hot-reloadable** | Drop a new `.wasm` file to update without restarting |

### Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        PRX Process                               │
│                                                                  │
│  ┌──────────────┐   ┌──────────────┐   ┌──────────────────────┐ │
│  │  Agent Loop  │   │  Tool Router │   │  Provider Router     │ │
│  └──────┬───────┘   └──────┬───────┘   └──────────┬───────────┘ │
│         │                  │                       │             │
│  ┌──────▼───────────────────▼───────────────────────▼──────────┐ │
│  │                  Plugin Manager (host)                       │ │
│  │                                                              │ │
│  │  ┌──────────────────────────────────────────────────────┐   │ │
│  │  │           wasmtime Component Model Runtime            │   │ │
│  │  │                                                       │   │ │
│  │  │  ┌──────────┐  ┌──────────┐  ┌──────────────────┐   │   │ │
│  │  │  │ my-tool  │  │ my-hook  │  │  my-middleware   │   │   │ │
│  │  │  │ .wasm    │  │ .wasm    │  │  .wasm           │   │   │ │
│  │  │  └──────────┘  └──────────┘  └──────────────────┘   │   │ │
│  │  │         │            │               │               │   │ │
│  │  │  ┌──────▼────────────▼───────────────▼─────────────┐ │   │ │
│  │  │  │          Host Functions (WIT interfaces)          │ │   │ │
│  │  │  │  log · config · kv · http · memory · events      │ │   │ │
│  │  │  └───────────────────────────────────────────────── ┘ │   │ │
│  │  └──────────────────────────────────────────────────────┘   │ │
│  └──────────────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────────┘
```

### Capability Types

| Type | Description | Key exports |
|------|-------------|-------------|
| **tool** | LLM-callable tool | `get-spec`, `execute` |
| **hook** | Lifecycle event observer (read-only) | `on-event` |
| **middleware** | Pipeline transformer | `process` |
| **cron** | Scheduled task | `run` |
| **provider** | Custom LLM backend | `name`, `chat` |
| **storage** | Custom memory backend | `name`, `store-memory`, `recall-memory`, `forget-memory` |

---

## 2. Quick Start

Build a simple tool plugin in Rust that echoes its input and counts calls.

### Prerequisites

```bash
# Install Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Install WASM component tooling
cargo install cargo-component
rustup target add wasm32-wasip2
```

### Step 1: Create the project

```bash
cargo new --lib echo-tool
cd echo-tool
```

### Step 2: Configure `Cargo.toml`

```toml
[package]
name = "echo-tool"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib", "rlib"]  # cdylib for WASM, rlib for tests

[dependencies]
prx-pdk = { git = "https://github.com/openprx/openprx", subdirectory = "pdk/rust/prx-pdk" }
serde_json = "1"

[package.metadata.component]
package = "prx:plugin@0.1.0"
```

### Step 3: Implement `src/lib.rs`

```rust
use prx_pdk::prelude::*;

pub struct EchoTool;

impl EchoTool {
    pub fn get_spec_impl() -> ToolSpec {
        ToolSpec {
            name: "echo".to_string(),
            description: "Echo the input text back to the caller.".to_string(),
            parameters_schema: r#"{
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "Text to echo" }
                },
                "required": ["text"]
            }"#.to_string(),
        }
    }

    pub fn execute_impl(args_json: &str) -> PluginResult {
        let args: serde_json::Value = match serde_json::from_str(args_json) {
            Ok(v) => v,
            Err(e) => return PluginResult::err(format!("Bad args: {e}")),
        };

        let text = match args["text"].as_str() {
            Some(t) => t,
            None => return PluginResult::err("Missing 'text' parameter"),
        };

        // Log and count calls
        log::info(&format!("echo called with: {text}"));
        let count = kv::increment("call_count", 1).unwrap_or(0);
        log::debug(&format!("Total calls: {count}"));

        PluginResult::ok(text)
    }
}

// WASM export wiring (only compiled for wasm32 targets)
#[cfg(target_arch = "wasm32")]
mod wasm_exports {
    use super::EchoTool;
    use bindings::Guest;

    impl Guest for EchoTool {
        fn get_spec() -> bindings::ToolSpec {
            let s = EchoTool::get_spec_impl();
            bindings::ToolSpec {
                name: s.name,
                description: s.description,
                parameters_schema: s.parameters_schema,
            }
        }
        fn execute(args: String) -> bindings::PluginResult {
            let r = EchoTool::execute_impl(&args);
            bindings::PluginResult { success: r.success, output: r.output, error: r.error }
        }
    }
    bindings::export!(EchoTool with_types_in bindings);
}
```

### Step 4: Create `plugin.toml`

```toml
[plugin]
name = "echo-tool"
version = "0.1.0"
description = "Echo input text"
author = "Your Name"
wasm = "plugin.wasm"

[[capabilities]]
type = "tool"
name = "echo"
description = "Echo the input text"

[permissions]
required = ["log", "kv"]
optional = []

[resources]
max_fuel = 10_000_000
max_memory_mb = 8
max_execution_time_ms = 1000
```

### Step 5: Build and deploy

```bash
# Build the WASM component
cargo component build --release

# Deploy to PRX plugins directory
mkdir -p /path/to/prx/plugins/echo-tool
cp target/wasm32-wasip2/release/echo_tool.wasm /path/to/prx/plugins/echo-tool/plugin.wasm
cp plugin.toml /path/to/prx/plugins/echo-tool/
```

### Step 6: Run tests locally (no WASM runtime needed)

```rust
// src/lib.rs — add at the bottom
#[cfg(test)]
mod tests {
    use super::EchoTool;

    #[test]
    fn test_echo() {
        let result = EchoTool::execute_impl(r#"{"text":"hello"}"#);
        assert!(result.success);
        assert_eq!(result.output, "hello");
    }

    #[test]
    fn test_missing_param() {
        let result = EchoTool::execute_impl(r#"{}"#);
        assert!(!result.success);
    }
}
```

```bash
cargo test   # compiles as rlib, runs on host
```

---

## 3. Plugin Structure

Every plugin consists of:

```
my-plugin/
├── plugin.wasm     ← compiled WASM component
└── plugin.toml     ← manifest (required)
```

### `plugin.toml` Format

```toml
# ── Required ──────────────────────────────────────────────────────────────────

[plugin]
# Unique plugin identifier (kebab-case)
name = "my-plugin"

# Semantic version
version = "0.1.0"

# Human-readable description
description = "Does something useful"

# Path to compiled WASM file (relative to plugin.toml)
wasm = "plugin.wasm"

# Optional: author name or org
author = "Your Name <you@example.com>"

# ── Capabilities ──────────────────────────────────────────────────────────────
# At least one capability is required. A plugin can declare multiple.

[[capabilities]]
# Capability type: tool | hook | middleware | cron | provider | storage
type = "tool"

# For tool: the snake_case name the LLM will use to invoke this tool
name = "my_tool"

# Human-readable description of this capability
description = "Optional description"

# For hook: list of event patterns to subscribe to
# [[capabilities.events]]
# pattern = "prx.lifecycle.*"

# For middleware: processing priority (lower = runs first, 0–100)
# priority = 50

# For cron: cron expression (standard 5-field cron)
# schedule = "0 * * * *"   # every hour

# ── Permissions ───────────────────────────────────────────────────────────────

[permissions]
# Permissions the plugin MUST have to function. PRX will refuse to load the
# plugin if any required permission is denied.
required = ["log", "kv"]

# Permissions the plugin uses if available but can work without.
optional = ["http-outbound", "memory", "events"]

# For http-outbound permission: explicit URL allowlist (required)
# http_allowlist = [
#     "https://api.example.com",
#     "https://api.other.com/v1/",
# ]

# For filesystem permission (future): allowed paths
# filesystem_allowlist = ["/tmp/my-plugin/"]

# ── Resource Limits ───────────────────────────────────────────────────────────

[resources]
# Compute budget in wasmtime "fuel" units.
# 1M fuel ≈ 1–5ms of CPU depending on workload.
# Default: 100_000_000 (100M)
max_fuel = 100_000_000

# Linear memory limit in megabytes.
# Default: 16
max_memory_mb = 16

# Wall-clock timeout in milliseconds.
# Default: 5000
max_execution_time_ms = 5000

# Maximum number of outbound HTTP requests per invocation.
# Default: 10
max_http_requests = 10

# Maximum total KV storage in kilobytes (across all keys for this plugin).
# Default: 1024
max_kv_storage_kb = 1024

# ── Static Configuration ──────────────────────────────────────────────────────

[config]
# Arbitrary key-value pairs injected as read-only config.
# Access via prx_pdk::config::get("key") in plugin code.
# api_base_url = "https://api.example.com/v1"
# max_items = "100"
# debug = "false"
```

### Field Reference

| Field | Required | Type | Description |
|-------|----------|------|-------------|
| `plugin.name` | ✅ | string | Unique identifier (kebab-case) |
| `plugin.version` | ✅ | string | SemVer version |
| `plugin.description` | ✅ | string | Short description |
| `plugin.wasm` | ✅ | string | Path to `.wasm` file |
| `plugin.author` | ❌ | string | Author name |
| `capabilities[].type` | ✅ | enum | `tool\|hook\|middleware\|cron\|provider\|storage` |
| `capabilities[].name` | ✅* | string | Capability name (*required for tool/provider/storage) |
| `capabilities[].description` | ❌ | string | Capability description |
| `capabilities[].events[].pattern` | ✅* | string | Event pattern (*required for hook) |
| `capabilities[].priority` | ❌ | integer | Middleware priority (0–100, default 50) |
| `capabilities[].schedule` | ✅* | string | Cron expression (*required for cron) |
| `permissions.required` | ❌ | list | Required permissions |
| `permissions.optional` | ❌ | list | Optional permissions |
| `permissions.http_allowlist` | ✅* | list | Allowed HTTP origins (*required with `http-outbound`) |
| `resources.max_fuel` | ❌ | integer | Compute budget (default 100M) |
| `resources.max_memory_mb` | ❌ | integer | Memory limit MB (default 16) |
| `resources.max_execution_time_ms` | ❌ | integer | Timeout ms (default 5000) |
| `resources.max_http_requests` | ❌ | integer | HTTP request limit (default 10) |
| `resources.max_kv_storage_kb` | ❌ | integer | KV storage limit KB (default 1024) |
| `resources.max_payload_bytes` | ❌ | integer | Payload cap in bytes for every WASM capability call that carries input (default: an eighth of `max_memory_mb`, floor 64 KiB). `0` is rejected; a value above `max_memory_mb` is clamped to it |
| `config.*` | ❌ | string | Static configuration key-value pairs |

---

## 4. Capability Types

### 4.1 Tool

Tool plugins expose a single function to the LLM. When the LLM decides to call a tool, PRX routes the call to the matching plugin.

**WIT Interface:** `prx:plugin/tool-exports`  
**World:** `tool`

```wit
interface tool-exports {
    record tool-spec {
        name: string,
        description: string,
        parameters-schema: string,
    }

    record plugin-result {
        success: bool,
        output: string,
        error: option<string>,
    }

    get-spec: func() -> tool-spec;
    execute: func(args: string) -> plugin-result;
}
```

**Pattern:**

- `get-spec` is called **once** at load time. Return your tool's name, description, and JSON Schema for parameters.
- `execute` is called each time the LLM invokes the tool. `args` is a JSON string matching your schema.

**Stop does not stop your guest.** Every tool call runs on its own task, so a user pressing Stop ends the wait, not the call: an HTTP request, a `kv` write or a memory write your guest makes still happens, up to `max_execution_time_ms`. Make side-effecting tools idempotent where you can.

**When a call fails.** A tool adapter owns one wasmtime store and recovers from a trap exactly like middleware (see *Trap recovery, restart budget and quarantine* under 4.3), but every failure reaches the model as a failed tool result, never as a success:

| Event | What the model sees |
|-------|---------------------|
| `execute` returns `success: false` | Your result, unchanged. This is an answer, not a failure of the adapter. |
| The call traps, runs out of fuel or memory | A tool error. The next call runs on a rebuilt store. |
| The call runs past `max_execution_time_ms` | `WASM plugin timed out after …ms`. Charged to the restart budget like a trap. Waiting for the store behind other calls is bounded by the same value and not charged, so one call can take up to about three times `max_execution_time_ms` in the worst case (wait, rebuild, call). Provider and storage calls have the same bound. |
| Arguments larger than the payload cap | A tool error naming `max_payload_bytes`; your guest is not called. |
| The tool is quarantined | A tool error naming the quarantine, on every call until the next plugin generation. The tool stays listed. |

A trapped call is never retried for you: it may already have made an HTTP request. `wasm_plugins_status` lists each tool under `tool_adapters` with `plugin`, `tool` and the health fields described under 4.3, and a quarantine publishes `prx.plugin.tool_quarantined`.

**`plugin.toml`:**

```toml
[[capabilities]]
type = "tool"
name = "my_tool"
description = "What my tool does"
```

**Example (Rust):**

```rust
pub fn get_spec_impl() -> ToolSpec {
    ToolSpec {
        name: "weather".to_string(),
        description: "Get current weather for a city".to_string(),
        parameters_schema: r#"{
            "type": "object",
            "properties": {
                "city": { "type": "string" }
            },
            "required": ["city"]
        }"#.to_string(),
    }
}

pub fn execute_impl(args_json: &str) -> PluginResult {
    let args: serde_json::Value = serde_json::from_str(args_json).unwrap();
    let city = args["city"].as_str().unwrap_or("London");

    let resp = http::get(
        &format!("https://wttr.in/{}?format=3", city),
        &[],
    ).unwrap();

    PluginResult::ok(resp.body_text())
}
```

---

### 4.2 Hook

Hook plugins observe lifecycle events. They cannot modify the event data — use middleware for that. Hooks are ideal for logging, metrics, audit trails, and side effects.

**WIT Interface:** `prx:plugin/hook-exports`  
**World:** `hook`

```wit
interface hook-exports {
    on-event: func(event: string, payload-json: string) -> result<_, string>;
}
```

**`plugin.toml`:**

```toml
[[capabilities]]
type = "hook"
name = "my_audit_hook"

[[capabilities.events]]
pattern = "prx.lifecycle.*"
description = "All lifecycle events"

[[capabilities.events]]
pattern = "tool.call"
description = "Tool invocations only"
```

**When a delivery fails.** A hook adapter recovers from a trap exactly like middleware (see *Trap recovery, restart budget and quarantine* under 4.3). Hooks are observe-only, so every failure fails open: a hook that traps, times out, is being rebuilt or is quarantined simply does not see that event, and a payload larger than the payload cap is skipped (`skipped_oversize`). Your `on-event` result is recorded, never acted on. Counters you keep through `kv` — as the audit example does — survive a rebuild; anything kept in guest memory does not. Event subscriptions survive a rebuild, and `subscribe` is idempotent per pattern: calling it again after a rebuild returns the existing subscription instead of adding a second delivery. `wasm_plugins_status` lists each hook under `hook_adapters` with `invocation_count` and `last_event` (deliveries that reached your guest; one refused while the hook is quarantined or awaiting a rebuild is not counted), `last_error` (the most recent failure, kept after later successes) and the health fields; a quarantine publishes `prx.plugin.hook_quarantined`.

**Event patterns:**

- Exact: `"tool.call"` — matches only that event
- Wildcard: `"prx.lifecycle.*"` — matches all events under `prx.lifecycle.`
- All: `"*"` — matches every event (use sparingly)

**Common event names:**

| Event | Description | Payload fields |
|-------|-------------|---------------|
| `prx.lifecycle.agent_start` | Agent started | `agent_id` |
| `prx.lifecycle.agent_stop` | Agent stopped | `agent_id`, `reason` |
| `prx.plugin.middleware_quarantined` | A middleware adapter spent its restart budget and is no longer called | `plugin`, `capability`, `reason` |
| `prx.plugin.tool_quarantined` | A tool adapter spent its restart budget; its calls return a tool error | `plugin`, `capability`, `reason` |
| `prx.plugin.hook_quarantined` | A hook adapter spent its restart budget and no longer observes events | `plugin`, `capability`, `reason` |
| `prx.plugin.cron_quarantined` | A cron adapter spent its restart budget and its occurrences are skipped | `plugin`, `capability`, `reason` |
| `prx.plugin.provider_quarantined` | A provider adapter spent its restart budget; its requests return an error | `plugin`, `capability`, `reason` |
| `prx.plugin.storage_quarantined` | A storage adapter spent its restart budget; its operations return an error | `plugin`, `capability`, `reason` |
| `prx.plugin.storage_restarted` | A storage adapter was rebuilt on a fresh store; entries kept in guest memory are gone | `plugin`, `capability`, `reason`, `restart_count` |
| `tool.call` | Tool invoked | `tool_name`, `args`, `session_id` |
| `tool.result` | Tool completed | `tool_name`, `success`, `duration_ms` |
| `llm.request` | LLM request sent | `model`, `message_count` |
| `llm.response` | LLM response received | `model`, `tokens_used` |
| `error` | Error occurred | `message`, `context` |

**Example (Rust):**

```rust
pub fn on_event_impl(event: &str, payload_json: &str) -> Result<(), String> {
    log::info(&format!("Event: {event}"));

    // Count events by type
    let key = format!("count:{event}");
    let _ = kv::increment(&key, 1);

    // Alert on errors
    if event == "error" {
        let payload: serde_json::Value = serde_json::from_str(payload_json)
            .unwrap_or_default();
        log::error(&format!("Error: {}", payload["message"]));
    }

    Ok(())
}
```

---

### 4.3 Middleware

Middleware plugins intercept and transform data at specific pipeline stages. Multiple middleware plugins are ordered by `priority` (lower number = runs first).

**WIT Interface:** `prx:plugin/middleware-exports`  
**World:** `middleware`

```wit
interface middleware-exports {
    process: func(stage: string, data-json: string) -> result<string, string>;
}
```

**Pipeline stages:**

| Stage | When called | Invocations per turn | `data-json` shape |
|-------|-------------|----------------------|-------------------|
| `inbound` | After receiving a message, before agent loop | Once per admitted turn | `{ text, channel, user_id, session_id }` |
| `outbound` | After the agent loop has accepted the final response, before delivery and history insertion | Once per delivered turn; zero for tool-call iterations, for responses a corrective retry replaces, and for silent turns | `{ channel, text, has_tool_calls }` |
| `llm_request` | Before sending to LLM | Once per provider call | `{ messages: [...], model, temperature }` |
| `llm_response` | After receiving LLM response | Once per provider response | `{ text, tool_calls: [...], model }` |

**`outbound` runs exactly once per delivered turn.** It is invoked on the
terminal text after every retry, repair and marker guard has accepted the
response, and immediately before that text is streamed, stored in history and
returned. It is not invoked on a tool-call iteration (those keep their
pre-outbound assistant representation in history), on a response that a
corrective retry then replaces, or on a turn that delivers nothing
(`stay_silent`).

A turn that fails before its terminal response is accepted never reaches the
stage. A turn cancelled while the accepted reply is being streamed has already
run it: the stage runs before the first chunk leaves the loop, so the plugin
executed once for a turn the caller is told failed. If your `outbound` path has
side effects, treat an execution as "a reply was accepted", not as "a reply was
delivered in full".

On entrypoints that stream provider tokens live — chat and TUI provider
streaming — the user sees the provider's own text as it arrives, before
`outbound` runs on that turn. The reply stored in history and returned to the
caller is the post-outbound text, and so are the chunks the tool loop itself
emits; only the live provider deltas of those entrypoints are pre-outbound. A
middleware plugin that redacts text cannot rely on `outbound` alone to keep the
redacted string off those screens.

> **Compatibility warning.** Before this contract was enforced, outbound was
> invoked once per loop iteration instead of once per accepted delivered turn:
> a turn with N tool rounds ran the stage N+1 times, and every corrective retry
> added one more. Middleware whose `outbound` path has side effects — counters,
> audit records, rate budgets, billing, notifications, external writes — was
> therefore executed several times for one delivered reply, and should be
> re-checked against the exactly-once contract. Pure text transformations are
> unaffected in their delivered output.
>
> `has_tool_calls` is now always `false` at the single call site, because the
> stage runs only on a terminal response. The field is kept in the envelope so
> existing plugins that read it still parse the payload; do not use it to decide
> whether a response is final.

**`plugin.toml`:**

```toml
[[capabilities]]
type = "middleware"
priority = 10   # lower number = runs first
```

**Example — content filter (Rust):**

```rust
pub fn process_impl(stage: &str, data_json: &str) -> Result<String, String> {
    if stage != "inbound" {
        // Pass through stages we don't care about
        return Ok(data_json.to_string());
    }

    let mut data: serde_json::Value = serde_json::from_str(data_json)
        .map_err(|e| e.to_string())?;

    let text = data["text"].as_str().unwrap_or("").to_string();

    // Filter sensitive content
    if text.contains("BLOCKED_WORD") {
        return Err("Content policy violation".to_string());
    }

    // Enrich with metadata
    data["processed_by"] = serde_json::json!("content-filter/0.1.0");

    Ok(data.to_string())
}
```

**Returning an error from `process` does not block the turn:** the envelope passes through unchanged (fail open). Do not rely on middleware for enforcement.

**Every middleware stage fails open.** A plugin that returns an error, traps, times out, is being rebuilt, is quarantined, or is skipped for an oversized payload leaves the envelope unchanged; the turn continues. This is the same for trusted and untrusted plugins, so `outbound` alone cannot guarantee that a redaction reaches the user.

#### Trap recovery, restart budget and quarantine

A middleware adapter owns one wasmtime `Store`. Wasmtime poisons the whole store when anything unwinds out of wasm — a trap, exhausted fuel, a failed `memory.grow`, or the host timeout — and a poisoned store refuses every later call. PRX therefore rebuilds the adapter on a brand-new store:

| Event | What PRX does |
|-------|---------------|
| The call traps or times out (a guest still computing in wasm at `max_execution_time_ms` is interrupted by the epoch deadline, and that counts as a timeout) | The adapter is marked dead and the envelope passes through unchanged. One `error` log line carries `plugin`, `capability` (`middleware`), `operation` (the stage), `reason`, `restart_count`. |
| The caller gives up mid-call (a stopped turn, a disconnected client, a caller deadline shorter than yours) | Nothing happens to your guest: every call runs on its own task, so it finishes, or reaches `max_execution_time_ms` and is charged as a timeout, exactly as if the caller had waited. Its side effects (HTTP requests, `kv` and memory writes) therefore still happen after the user pressed Stop, and a caller that retries a call it gave up on can run it twice. Only a runtime shutdown can drop a call mid-guest; that marks the adapter dead without charging the restart budget, and `cancelled_count` records it. |
| The next call arrives | The adapter is rebuilt on a fresh store — new limits, fuel, epoch deadline and instantiation — and the call runs against it. |
| The rebuild itself fails to instantiate | The attempt is charged to the restart budget, the adapter stays dead, the envelope passes through unchanged and the next call retries. One `warn` line. |
| A fourth charged rebuild attempt inside one rolling window of `max(60 s, 4 × max_execution_time_ms)` (the window grows with the timeout so that a guest that hangs on every call, or on three calls in four, is caught while it is called back to back — less than about a third of the timeout idle between calls, for timeouts of 15 s or more (a shorter timeout gets a 60 s window, which tolerates more idle); faults further apart, such as an hourly job whose guest traps through an outage, keep being retried), or a fourth `max_execution_time_ms` timeout in a row with no call returning from your guest in between, at any pace — whether the guest was waiting in a host call or computing in wasm, where the epoch deadline interrupts it (other traps do not count; any result or guest-returned error resets it) | The adapter is quarantined: it is skipped entirely, one `error` line and one `prx.plugin.middleware_quarantined` event are emitted, every skipped call afterwards increments `skipped_quarantined`, and `wasm_plugins_status` reports it. |
| Any reload, refresh, install, enable or disable | A new plugin generation builds new adapters, so quarantine ends. |

**A rebuilt instance loses guest state.** Anything a plugin kept in its own linear memory — a counter, a cache, a compiled regex — starts empty after a rebuild. Data written through the `kv` host functions lives in host state and survives. Keep anything that has to outlive a trap in `kv`.

`wasm_plugins_status` reports per middleware adapter: `state` (`live`, `restarting`, `quarantined`), `restart_count`, `failure_count`, `cancelled_count`, `skipped_oversize`, `skipped_quarantined`, `consecutive_timeouts`, `last_error`, `last_error_at`, and `quarantined_for_ms` and `quarantine_rule` (`restart_window` or `timeout_streak`) while quarantined. `restart_count` counts rebuild attempts, successful or not, including the uncharged ones that follow a cancellation. `last_error` may contain the error string your guest returned, so it is truncated to 512 characters, followed by a `" ... (truncated)"` marker (quotes delimit the string and are not appended).

#### Payload size

Before a call is made, PRX compares the envelope against the adapter's payload cap. Over the cap the plugin is skipped for that call, the envelope passes through unchanged and `skipped_oversize` is incremented. The cap defaults to an eighth of `max_memory_mb` (with a 64 KiB floor), because a guest that parses its input into a DOM holds three live copies of the text at once. Raise it with `resources.max_payload_bytes` if the guest streams its input instead of parsing it whole.

Two values are not accepted as written. `max_payload_bytes = 0` is rejected at load: it does not mean "unlimited", it skips every call for the adapter's whole life. A value above `max_memory_mb` is clamped down to the guest's linear memory with a warning, because a cap the guest could never hold re-arms the trap-rebuild-quarantine loop this guard exists to close.

---

### 4.4 Cron

Cron plugins run on a schedule. They have no input and return a status string.

**WIT Interface:** `prx:plugin/cron-exports`  
**World:** `cron`

```wit
interface cron-exports {
    run: func() -> result<string, string>;
}
```

**`plugin.toml`:**

```toml
[[capabilities]]
type = "cron"
schedule = "0 * * * *"   # every hour at :00
```

**Cron expression format (5 fields):**

```
┌──────── minute (0-59)
│ ┌────── hour (0-23)
│ │ ┌──── day of month (1-31)
│ │ │ ┌── month (1-12)
│ │ │ │ ┌ day of week (0-7, 0=Sun)
│ │ │ │ │
* * * * *
```

| Expression | Meaning |
|------------|---------|
| `* * * * *` | Every minute |
| `0 * * * *` | Every hour |
| `0 9 * * *` | Every day at 09:00 UTC |
| `0 9 * * 1` | Every Monday at 09:00 UTC |
| `*/5 * * * *` | Every 5 minutes |
| `0 0 1 * *` | First of every month at midnight |

**Example (Rust):**

```rust
pub fn run_impl() -> Result<String, String> {
    log::info("Cron job starting");

    let count: u64 = kv::get_str("processed")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    // Do some periodic work...
    events::publish("my.cron.ran", r#"{"status":"ok"}"#)
        .map_err(|e| e.to_string())?;

    Ok(format!("Done. Total processed: {count}"))
}
```

**When a run fails.** A cron adapter recovers from a trap exactly like middleware (see *Trap recovery, restart budget and quarantine* under 4.3). A run that traps, times out or is refused while the job is quarantined returns an error — `cron_run` shows it, the scheduler records it — and is not retried before the job's next scheduled time. Keep state that must outlive a trap in `kv`, as the example above does: a rebuilt job starts with empty guest memory. `wasm_plugins_status` lists each job under `cron_adapters` with `plugin`, `schedule` and the health fields; a quarantine publishes `prx.plugin.cron_quarantined`.

---

### 4.5 Provider

Provider plugins serve as custom LLM backends. When PRX selects a provider matching this plugin's name, it delegates `chat` requests to the plugin.

**WIT Interface:** `prx:plugin/provider-exports`  
**World:** `provider`

```wit
interface provider-exports {
    record chat-message {
        role: string,
        content: string,
    }

    record tool-call {
        id: string,
        name: string,
        arguments: string,
    }

    record chat-response {
        text: option<string>,
        tool-calls: list<tool-call>,
    }

    name: func() -> string;
    chat: func(messages: list<chat-message>, model: string, temperature: f64)
              -> result<chat-response, string>;
}
```

**`plugin.toml`:**

```toml
[[capabilities]]
type = "provider"
name = "my-llm"
description = "Custom LLM via my-api.com"

[permissions]
required = ["log", "http-outbound"]

[permissions]
http_allowlist = ["https://my-api.com"]
```

**Example (Rust) — proxy to a custom API:**

```rust
pub fn name_impl() -> String {
    "my-llm".to_string()
}

pub fn chat_impl(
    messages: Vec<ChatMessage>,
    model: &str,
    temperature: f64,
) -> Result<ChatResponse, String> {
    let body = serde_json::json!({
        "model": model,
        "messages": messages,
        "temperature": temperature,
    });

    let resp = http::post_json(
        "https://my-api.com/v1/chat",
        &[("Authorization", &format!("Bearer {}", config::get_or("api_key", "")))],
        &body,
    ).map_err(|e| e.to_string())?;

    let json: serde_json::Value = serde_json::from_slice(&resp.body)
        .map_err(|e| e.to_string())?;

    Ok(ChatResponse {
        text: json["choices"][0]["message"]["content"].as_str().map(str::to_string),
        tool_calls: vec![],
    })
}
```

**When a request fails.** A provider adapter recovers from a trap exactly like middleware (see *Trap recovery, restart budget and quarantine* under 4.3). Every failure is returned to PRX as an error — a trap, a timeout, a refusal while quarantined, or a request larger than the payload cap (the sum of all message roles and contents plus the model name) — so the caller's retry and fallback layer decides what happens next. Nothing is truncated to fit and no response is invented. Long conversations grow with every turn: raise `max_memory_mb` and `max_payload_bytes` together if your provider must accept large histories. `wasm_plugins_status` lists each provider under `provider_adapter_status` with `plugin`, `provider` and the health fields; a quarantine publishes `prx.plugin.provider_quarantined`.

---

### 4.6 Storage

Storage plugins serve as custom memory backends (e.g., Pinecone, Qdrant, custom databases). PRX delegates all memory operations to the plugin when it is selected as the active storage backend.

**WIT Interface:** `prx:plugin/storage-exports`  
**World:** `storage`

```wit
interface storage-exports {
    record memory-entry {
        id: string,
        key: string,
        content: string,
        category: string,
        timestamp: string,
        score: option<f64>,
    }

    name: func() -> string;
    store-memory: func(key: string, content: string, category: string,
                       session-id: option<string>) -> result<_, string>;
    recall-memory: func(query: string, limit: u32, session-id: option<string>)
                       -> result<list<memory-entry>, string>;
    forget-memory: func(key: string) -> result<bool, string>;
    count-memories: func() -> result<u32, string>;
    health-check: func() -> bool;
}
```

**`plugin.toml`:**

```toml
[[capabilities]]
type = "storage"
name = "pinecone"
description = "Pinecone vector database backend"

[permissions]
required = ["log", "http-outbound"]
http_allowlist = ["https://controller.us-east1-gcp.pinecone.io"]

[config]
api_key = ""        # set at deploy time
index_name = "prx-memory"
namespace = "default"
```

**When an operation fails.** Like every guest call, storage operations run detached from their caller: a caller that gives up does not cancel an operation already handed to your guest, so it completes (or hits `max_execution_time_ms`) as if the caller had waited. A storage adapter recovers from a trap exactly like middleware (see *Trap recovery, restart budget and quarantine* under 4.3), and every failure is an error: a write that traps, times out, is refused while quarantined, or is larger than the payload cap (key, content, category and session together) is reported as failed and never retried for you; a read that fails is an error, never an empty result; `health-check` failures read as unhealthy. **Entries kept in your guest's own memory do not survive a rebuild** — the trap already made them unreachable, and the rebuilt guest starts empty. The reset is reported in band: the first read (`recall`, `get`, `list`, `count`) made by the call that rebuilt your backend fails with an error saying the store was rebuilt and its guest-memory entries are gone, rather than answering "no memories"; the next read answers from the rebuilt store. Every storage rebuild is also logged at `warn` with the storage name and published on `prx.plugin.storage_restarted` for operators; nothing in PRX itself subscribes to it. Keep durable data outside the guest: in an external service, or in `kv`, which survives a rebuild but not a plugin reload. `wasm_plugins_status` lists each backend under `storage_adapter_status` with `plugin`, `storage` and the health fields; a quarantine publishes `prx.plugin.storage_quarantined`.

---

## 5. Host Functions

Plugins call host functions via the WIT interfaces imported by their world. All host functions are available through the PDK wrappers.

### 5.1 `log` — Structured Logging

**Permission:** Always granted (no declaration needed)  
**WIT:** `prx:host/log`

Emit log messages that appear in PRX's structured logs with the plugin name as context.

```rust
// Rust
log::trace("verbose detail");
log::debug("debugging info");
log::info("normal operation");
log::warn("something unexpected");
log::error("operation failed");
```

```python
# Python
host.log.trace("verbose detail")
host.log.info("normal operation")
host.log.error("failed: " + str(e))
```

```typescript
// TypeScript
import { log } from "@prx/pdk";
log.info("normal operation");
log.error("operation failed");
```

| Level | Use for |
|-------|---------|
| `trace` | Very verbose internal steps (disabled in production by default) |
| `debug` | Development debugging |
| `info` | Normal operational messages |
| `warn` | Unexpected but recoverable situations |
| `error` | Failures that affect functionality |

---

### 5.2 `config` — Plugin Configuration

**Permission:** `"config"` (always granted)  
**WIT:** `prx:host/config`

Read-only access to values defined in `plugin.toml [config]`.

```rust
// Rust
let api_key = config::get("api_key");             // Option<String>
let timeout = config::get_or("timeout_ms", "5000"); // String (with default)
let all = config::get_all();                       // Vec<(String, String)>
```

```python
# Python
value = host.config.get("api_key")           # Optional[str]
value = host.config.get_or("timeout_ms", "5000")
pairs = host.config.get_all()                # list[tuple[str, str]]
```

Config values are set at deploy time and never change at runtime. Use them for API keys, base URLs, feature flags, and other deployment-time parameters.

---

### 5.3 `kv` — Key-Value Storage

**Permission:** `"kv"`  
**WIT:** `prx:host/kv`

Isolated persistent key-value store. Each plugin has its own namespace — plugins cannot read each other's keys.

```rust
// Rust — raw bytes
kv::set("key", b"value").unwrap();
let bytes: Option<Vec<u8>> = kv::get("key");
let existed = kv::delete("key").unwrap();   // bool
let keys = kv::list_keys("prefix:");         // Vec<String>

// PDK convenience helpers
kv::set_str("name", "Alice").unwrap();
let name: Option<String> = kv::get_str("name");

kv::set_json("settings", &my_struct).unwrap();
let settings: MyStruct = kv::get_json("settings").unwrap();

let count: i64 = kv::increment("calls", 1).unwrap();
```

```python
# Python
host.kv.set("key", b"bytes")
host.kv.set_str("key", "text")
host.kv.set_json("key", {"x": 1})

data   = host.kv.get("key")        # bytes | None
text   = host.kv.get_str("key")    # str | None
obj    = host.kv.get_json("key")   # any | None

host.kv.delete("key")
keys  = host.kv.list_keys("prefix:")
count = host.kv.increment("counter", delta=1)
```

**Key design guidelines:**

- Use namespaced keys: `"user:{id}:prefs"`, `"count:tool_calls"`, etc.
- `list-keys("")` returns all keys for the plugin.
- Values are raw bytes; use PDK helpers for strings and JSON.
- Storage is limited by `resources.max_kv_storage_kb`.

---

### 5.4 `http-outbound` — HTTP Requests

**Permission:** `"http-outbound"` + `http_allowlist`  
**WIT:** `prx:host/http-outbound`

Make outbound HTTP requests. URLs are validated against the `http_allowlist` declared in `plugin.toml` before the request is sent.

```rust
// Rust
let resp = http::get("https://api.example.com/data", &[]).unwrap();
let json: serde_json::Value = resp.json().unwrap();
println!("Status: {}", resp.status);   // u16

let resp = http::post_json(
    "https://api.example.com/submit",
    &[("Authorization", "Bearer token123")],
    &payload,
).unwrap();

// Generic request
let resp = http::request(
    "DELETE",
    "https://api.example.com/items/42",
    &[("X-Api-Key", "secret")],
    None,
).unwrap();
```

```python
# Python
resp = host.http.get("https://api.example.com/data")
print(resp.status)          # int
print(resp.text())          # str
print(resp.json())          # any

resp = host.http.post_json("https://api.example.com/post", {"key": "val"})
resp = host.http.request("PUT", url, headers=[("X-Token", "abc")], body=b"...")
```

**`plugin.toml` — declare allowed origins:**

```toml
[permissions]
required = ["http-outbound"]
http_allowlist = [
    "https://api.openweathermap.org",
    "https://api.ipify.org",
]
```

A request to a URL not matching the allowlist returns an error immediately (no network connection is made).

---

### 5.5 `memory` — Long-Term Memory

**Permission:** `"memory"`  
**WIT:** `prx:host/memory`

Store and recall memories from PRX's memory system. Memories are semantically indexed and searchable.

```rust
// Rust
let id = memory::store("Paris is the capital of France", "fact").unwrap();

let entries = memory::recall("capital of France", 5).unwrap();
for entry in &entries {
    println!("{}: {} (importance: {:.2})", entry.id, entry.text, entry.importance);
}
```

```python
# Python
entry_id = host.memory.store("Important fact", category="fact")

entries = host.memory.recall("search query", limit=5)
for e in entries:
    print(e.id, e.text, e.category, e.importance)
```

**Categories** (examples): `"fact"`, `"preference"`, `"decision"`, `"entity"`, `"other"`

---

### 5.6 `events` — Event Bus

**Permission:** `"events"`  
**WIT:** `prx:host/events`

Publish and subscribe to the PRX internal event bus. Enables plugin-to-plugin communication and integration with the PRX lifecycle event stream.

```rust
// Rust — publish
events::publish("my.plugin.result", r#"{"status":"ok","count":42}"#).unwrap();
events::publish_json("my.plugin.result", &my_struct).unwrap();

// Subscribe (useful in hook plugins)
let sub_id = events::subscribe("prx.lifecycle.*").unwrap();
// ... later
events::unsubscribe(sub_id).unwrap();
```

```python
# Python
host.events.publish("my.plugin.result", '{"status":"ok"}')
host.events.publish_json("my.plugin.result", {"status": "ok"})

sub_id = host.events.subscribe("weather.*")
host.events.unsubscribe(sub_id)
```

**Rules:**

- Payload must be valid JSON, max 64 KB.
- Publishing is fire-and-forget (no delivery confirmation).
- Recursion is protected: a plugin's `on-event` handler cannot trigger itself via event publish.

---

## 6. Permissions

Permissions are declared in `plugin.toml` and enforced by the PRX host. Attempting to call a host function without the corresponding permission results in a trap.

### Permission Reference

| Permission | Host interface | Granted by default | Notes |
|------------|---------------|--------------------|-------|
| `log` | `prx:host/log` | ✅ Yes | Always available |
| `config` | `prx:host/config` | ✅ Yes | Read-only plugin config |
| `kv` | `prx:host/kv` | ❌ No | Must declare |
| `events` | `prx:host/events` | ❌ No | Must declare |
| `http-outbound` | `prx:host/http-outbound` | ❌ No | Requires `http_allowlist` |
| `memory` | `prx:host/memory` | ❌ No | Must declare |

### `required` vs `optional`

```toml
[permissions]
# Plugin refuses to load if these are denied
required = ["log", "kv"]

# Plugin loads and degrades gracefully if these are unavailable
optional = ["http-outbound", "memory"]
```

Check optional permissions at runtime:

```rust
// Rust — try optional permission, fall back gracefully
let weather = match http::get("https://wttr.in/London?format=3", &[]) {
    Ok(resp) => resp.body_text(),
    Err(e) => {
        log::warn(&format!("HTTP unavailable: {e}"));
        "Weather unavailable".to_string()
    }
};
```

### HTTP Allowlist

The `http_allowlist` is required when declaring `http-outbound` permission. URLs are matched by origin (scheme + host + optional path prefix):

```toml
[permissions]
required = ["http-outbound"]
http_allowlist = [
    "https://api.openweathermap.org",     # all paths under this origin
    "https://api.example.com/v2/",        # only paths under /v2/
]
```

Requests to origins not in the allowlist are rejected before any network connection is made.

---

## 7. Resource Limits

Resource limits protect the PRX host from misbehaving plugins.

| Limit | `plugin.toml` key | Default | Description |
|-------|-------------------|---------|-------------|
| Compute | `max_fuel` | 100,000,000 | wasmtime fuel units; ~100M ≈ 100–500ms of CPU |
| Memory | `max_memory_mb` | 16 | WASM linear memory in MB |
| Wall time | `max_execution_time_ms` | 5000 | Per-invocation timeout |
| HTTP calls | `max_http_requests` | 10 | Outbound requests per invocation |
| KV storage | `max_kv_storage_kb` | 1024 | Total KV data for this plugin |
| Payload | `max_payload_bytes` | an eighth of `max_memory_mb`, floor 64 KiB | Largest payload handed to a guest in one call (middleware, tool, hook, provider, storage); `0` is rejected and a value above `max_memory_mb` is clamped to it |

### Fuel Budget Guide

| Workload | Typical fuel usage |
|----------|--------------------|
| String manipulation (1KB) | ~100K |
| JSON parse/serialize | ~500K–2M |
| Crypto (SHA-256 hash) | ~5M |
| Large loop (1M iterations) | ~50M |
| Default limit | 100M |

When fuel is exhausted, the plugin call returns an error: `"plugin ran out of fuel"`. Increase `max_fuel` if you hit this limit in production.

**Fuel is metered per invocation for every capability kind.** `max_fuel` is re-armed on the adapter's store before every call (and before the load-time `get-spec` or `name()` probe), alongside the epoch deadline, so it means "the budget for one call". It used to be armed only when the adapter was built, which made it a process-lifetime allowance that every call drained until the store trapped and stayed poisoned. This is a widening: a long-running plugin now gets more total compute than before, and a `max_fuel` value chosen to cover a whole process has to be re-read as a per-call figure.

### Memory Guide

Python and JavaScript plugins embed their runtime interpreter:

- Rust plugins: typically 1–4 MB
- Go/TinyGo plugins: typically 1–3 MB
- Python plugins: typically 8–15 MB (embeds Python interpreter)
- JavaScript plugins: typically 5–10 MB (embeds SpiderMonkey)

Set `max_memory_mb` accordingly.

---

## 8. Multi-language Support

PRX supports four languages for plugin development. All languages compile to the WASM Component Model and use identical WIT interfaces.

### Rust — `pdk/rust/prx-pdk/`

The primary, most ergonomic PDK. Recommended for new plugins.

- **Guide:** [`pdk/rust/README.md`](../pdk/rust/README.md)
- **Examples:** [`pdk/rust/examples/`](../pdk/rust/examples/)
- **Build tool:** `cargo-component`
- **Target:** `wasm32-wasip2`

```bash
cargo install cargo-component
rustup target add wasm32-wasip2
cargo component build --release
```

### Python — `pdk/python/`

Write plugins in Python 3.10+. The Python interpreter is embedded in the WASM binary (~8–15 MB).

- **Guide:** [`pdk/python/README.md`](../pdk/python/README.md)
- **Examples:** [`pdk/python/examples/`](../pdk/python/examples/)
- **Build tool:** `componentize-py`
- **Target:** `wasm32-wasip2`

```bash
pip install componentize-py>=0.16
componentize-py --wit-path /path/to/prx/wit --world tool componentize plugin.py -o plugin.wasm
```

### JavaScript/TypeScript — `pdk/javascript/`

Write plugins in TypeScript 5.0+. Uses SpiderMonkey JS engine embedded in WASM (~5–10 MB).

- **Guide:** [`pdk/javascript/README.md`](../pdk/javascript/README.md)
- **Examples:** [`pdk/javascript/examples/`](../pdk/javascript/examples/)
- **Build tool:** `jco` + `componentize-js`
- **Target:** WASM Component Model

```bash
npm install --save-dev @bytecodealliance/jco @bytecodealliance/componentize-js typescript
npx tsc && npx jco componentize dist/plugin.js --wit /path/to/prx/wit --world tool -o plugin.wasm
```

### Go — `pdk/go/`

Write plugins in Go/TinyGo. TinyGo is required for WASM compilation (standard Go does not support `wasm32-wasip2`).

- **Guide:** [`pdk/go/README.md`](../pdk/go/README.md)
- **Examples:** [`pdk/go/examples/`](../pdk/go/examples/)
- **Build tool:** TinyGo ≥ 0.34
- **Target:** `wasm32-wasip2`

```bash
tinygo build -target wasm32-wasip2 -scheduler none -no-debug -opt 2 -o plugin.wasm .
```

> **Note:** Go plugins use `//go:wasmexport` with manual pointer passing. For production plugins, use `wit-bindgen-go` to generate proper Component Model bindings. See [`pdk/go/README.md`](../pdk/go/README.md) for details.

### Language Comparison

| Feature | Rust | Python | JavaScript/TS | Go/TinyGo |
|---------|------|--------|---------------|-----------|
| Binary size | 0.5–2 MB | 8–15 MB | 5–10 MB | 1–3 MB |
| Cold-start | ~1ms | ~50ms | ~30ms | ~5ms |
| Type safety | ✅ Full | Partial (3.10+) | ✅ TypeScript | ✅ Full |
| Ecosystem | crates.io | PyPI | npm | pkg.go.dev |
| Goroutines | N/A | N/A | N/A | ❌ (no scheduler) |
| `reflect` | ✅ | ✅ | ✅ | ❌ |
| Recommended for | Performance-critical | Data/ML, scripting | Web APIs | Hashing, crypto |

---

## 9. CLI Tool: `prx-plugin`

The `prx-plugin` CLI manages the full plugin lifecycle. Install it:

```bash
cd pdk/cli
cargo build --release
cp target/release/prx-plugin /usr/local/bin/
```

### `prx-plugin new`

Scaffold a new plugin from a template.

```bash
prx-plugin new <name> [--lang <lang>] [--capability <cap>]
```

| Option | Values | Default |
|--------|--------|---------|
| `--lang` | `rust`, `python`, `javascript`, `go` | `rust` |
| `--capability` | `tool`, `hook`, `middleware`, `cron` | `tool` |

```bash
prx-plugin new weather-tool --lang rust --capability tool
prx-plugin new audit-hook --lang python --capability hook
prx-plugin new rate-limiter --lang javascript --capability middleware
prx-plugin new daily-report --lang go --capability cron
```

### `prx-plugin build`

Build the plugin in the current directory. Language is auto-detected.

```bash
prx-plugin build            # debug build
prx-plugin build --release  # release build (Rust)
```

| Detected file | Language | Build command |
|---------------|----------|---------------|
| `Cargo.toml` | Rust | `cargo component build [--release]` |
| `go.mod` | Go | `tinygo build -target wasm32-wasip2 -scheduler none -o plugin.wasm .` |
| `package.json` | JavaScript | `npx tsc && npx jco componentize ...` |
| `pyproject.toml` / `setup.py` | Python | `componentize-py componentize plugin.py -o plugin.wasm` |

### `prx-plugin validate`

Validate a compiled `.wasm` file against its `plugin.toml`.

```bash
prx-plugin validate             # validates ./plugin.wasm
prx-plugin validate my.wasm     # validates specific file
```

Checks performed:

- Valid WASM magic bytes
- WASM component (not plain module) detection
- `plugin.toml` parse and schema validation
- Required exports present for declared capability type

### `prx-plugin test`

Run the plugin's language-specific test suite.

```bash
prx-plugin test
```

| Language | Command run |
|----------|-------------|
| Rust | `cargo test` |
| Go | `go test ./...` |
| JavaScript | `npm test` |
| Python | `pytest` |

Also performs a basic WASM load check if `plugin.wasm` exists.

### `prx-plugin pack`

Pack the plugin into a `.prxplugin` archive (tar.gz) for distribution.

```bash
prx-plugin pack
prx-plugin pack --output dist/my-plugin-v1.0.prxplugin
```

Archive contents:

```
my-plugin-0.1.0.prxplugin
├── plugin.wasm
├── plugin.toml
├── README.md         (if present)
├── LICENSE           (if present)
├── CHANGELOG.md      (if present)
└── checksums.sha256
```

---

## 10. Event Bus

The event bus enables fire-and-forget communication between plugins and integration with PRX lifecycle events.

### Topics

Topics are dot-separated strings. Use a hierarchy that reflects your domain:

```
prx.lifecycle.agent_start
prx.lifecycle.agent_stop
prx.plugin.middleware_quarantined
tool.call
tool.result
llm.request
llm.response
error
my-plugin.custom.event
```

### Wildcards

| Pattern | Matches |
|---------|---------|
| `"prx.lifecycle.*"` | `prx.lifecycle.agent_start`, `prx.lifecycle.agent_stop`, etc. |
| `"tool.*"` | `tool.call`, `tool.result` |
| `"*"` | Everything (use carefully) |
| `"my-plugin.status"` | Exact match only |

### Payload

- Must be valid JSON.
- Maximum 64 KB per event.
- Delivered asynchronously to all subscribers.

### Publish/Subscribe

```rust
// Publish from any plugin type
events::publish("my.topic", r#"{"key":"value"}"#).unwrap();

// Subscribe (typically in a hook plugin)
let sub_id = events::subscribe("my.topic").unwrap();
// Subscription is cancelled when sub_id is dropped / unsubscribe() called
events::unsubscribe(sub_id).unwrap();
```

### Recursion Protection

A plugin's `on-event` handler that publishes an event to a topic the same plugin subscribes to will not receive that event (no infinite loops). PRX tracks event depth and refuses to re-enter the same plugin.

### Payload Limit

Events exceeding 64 KB are rejected at `publish()` time with an error. Break large payloads into multiple smaller events or use the KV store + event as notification.

---

## 11. Hot Reload

PRX watches the plugins directory for changes. When a `.wasm` or `plugin.toml` file changes, the plugin is gracefully replaced:

1. The new `plugin.wasm` is loaded into a fresh wasmtime store.
2. The new plugin's exports are validated.
3. If validation passes, the old plugin is atomically replaced.
4. In-flight calls on the old plugin complete before it is unloaded.

### File Watching

```
plugins/
└── my-tool/
    ├── plugin.wasm     ← change this to trigger reload
    └── plugin.toml     ← change this to trigger reload
```

PRX uses inotify (Linux) or FSEvents (macOS) for low-latency change detection. The typical reload time is **50–200ms** from file write to new plugin active.

### Zero-Downtime Deployment

```bash
# Build new version
cargo component build --release

# Atomic file replacement (avoids partial reads)
cp target/wasm32-wasip2/release/my_tool.wasm /tmp/plugin.wasm.new
mv /tmp/plugin.wasm.new /path/to/plugins/my-tool/plugin.wasm
```

`mv` is atomic on the same filesystem, ensuring PRX either reads the old or new `.wasm` — never a partially-written file.

### KV State Persistence

Plugin KV state persists across hot reloads. The new plugin version sees the same KV data as the old one, enabling zero-state-loss upgrades.

---

## 12. Troubleshooting

### "plugin ran out of fuel"

The plugin exceeded `max_fuel`. Increase it in `plugin.toml`:

```toml
[resources]
max_fuel = 500_000_000   # try 5x the previous value
```

If fuel consumption seems unreasonably high, check for infinite loops or excessive JSON serialization.

---

### "permission denied: http-outbound"

The plugin tried to make an HTTP request without the `http-outbound` permission, or the URL is not in `http_allowlist`.

1. Add `"http-outbound"` to `permissions.required`.
2. Add the target origin to `http_allowlist`:

   ```toml
   http_allowlist = ["https://api.target.com"]
   ```

---

### "URL not in allowlist"

The URL matches the permission declaration but not the allowlist pattern. Check that:

- The scheme matches (`https://` vs `http://`).
- The host matches exactly (no wildcard support; sub-paths are prefix-matched).
- There are no trailing slashes mismatches.

---

### "missing export: get-spec"

The plugin's capability type is `tool` but the `get-spec` export is missing from the compiled WASM. Common causes:

- The WASM-specific export wiring (`#[cfg(target_arch = "wasm32")]`) was not implemented.
- The wrong world was selected at build time.
- The cargo-component `export!` macro was not invoked.

Use `wasm_plugins_manage` with `action: "install"` or `"update"`; the staged
candidate is compiled and every declared runtime adapter is constructed before
the active generation is accepted. Failures are returned by the tool and are
also visible in `wasm_plugins_status`.

---

### "invalid plugin.toml"

Parse error in the manifest. Common mistakes:

- Missing `[[capabilities]]` section.
- `type` is not one of `tool|hook|middleware|cron|provider|storage`.
- `schedule` missing for a `cron` capability.

---

### "cargo component build" fails with "no world found"

Ensure `[package.metadata.component]` is present in `Cargo.toml`:

```toml
[package.metadata.component]
package = "prx:plugin@0.1.0"
```

---

### Python plugin: "componentize-py: WIT world not found"

The `--world` flag must match one of the worlds in `wit/worlds.wit`:
`tool`, `hook`, `middleware`, `cron`, `provider`, `storage`.

Check that `--wit-path` points to the PRX `wit/` directory (not a subdirectory).

---

### JavaScript plugin is too large

`componentize-js` bundles SpiderMonkey (~5–10 MB). This is expected. PRX pre-compiles and caches WASM modules at startup to amortize the cold-start cost. If binary size is a hard constraint, prefer Rust or Go.

---

### Plugin silently fails (no error, no output)

Enable debug logging in PRX and check the logs for plugin-level messages. Ensure the plugin calls `log::error()` on all error paths. Check that `success: false` returns include a populated `error` field.

---

### KV data not persisting across restarts

KV storage is backed by PRX's persistent store. If PRX is configured in ephemeral/test mode, KV data is in-memory only. Check `config/config.toml` for the storage backend configuration.

---

## Further Reading

- [Host Function Reference](host-function-reference.md) — detailed API reference with WIT signatures
- [Plugin Runtime Lifecycle](plugin-runtime-lifecycle.md) — runtime architecture and lifecycle guarantees
- [Rust PDK](../pdk/rust/README.md) — Rust plugin development
- [Python PDK](../pdk/python/README.md) — Python plugin development
- [JavaScript PDK](../pdk/javascript/README.md) — TypeScript/JavaScript plugin development
- [Go PDK](../pdk/go/README.md) — Go/TinyGo plugin development
