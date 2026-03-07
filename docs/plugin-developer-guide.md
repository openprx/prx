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

**Event patterns:**
- Exact: `"tool.call"` — matches only that event
- Wildcard: `"prx.lifecycle.*"` — matches all events under `prx.lifecycle.`
- All: `"*"` — matches every event (use sparingly)

**Common event names:**

| Event | Description | Payload fields |
|-------|-------------|---------------|
| `prx.lifecycle.agent_start` | Agent started | `agent_id` |
| `prx.lifecycle.agent_stop` | Agent stopped | `agent_id`, `reason` |
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

| Stage | When called | `data-json` shape |
|-------|-------------|-------------------|
| `inbound` | After receiving a message, before agent loop | `{ text, channel, user_id, session_id }` |
| `outbound` | After agent loop, before sending reply | `{ text, channel, session_id }` |
| `llm_request` | Before sending to LLM | `{ messages: [...], model, temperature }` |
| `llm_response` | After receiving LLM response | `{ text, tool_calls: [...], model }` |

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

**Returning an error from `process` blocks the pipeline.** Use this for enforcement (content filters, auth checks). The error message is returned to the caller.

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

### Fuel Budget Guide

| Workload | Typical fuel usage |
|----------|--------------------|
| String manipulation (1KB) | ~100K |
| JSON parse/serialize | ~500K–2M |
| Crypto (SHA-256 hash) | ~5M |
| Large loop (1M iterations) | ~50M |
| Default limit | 100M |

When fuel is exhausted, the plugin call returns an error: `"plugin ran out of fuel"`. Increase `max_fuel` if you hit this limit in production.

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

Run `prx-plugin validate plugin.wasm` for a detailed export check.

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
- [WASM Plugin Specification](wasm-plugin-spec.md) — architecture and design decisions
- [Rust PDK](../pdk/rust/README.md) — Rust plugin development
- [Python PDK](../pdk/python/README.md) — Python plugin development
- [JavaScript PDK](../pdk/javascript/README.md) — TypeScript/JavaScript plugin development
- [Go PDK](../pdk/go/README.md) — Go/TinyGo plugin development
- [P5 Implementation Plan](P5-implementation-plan.md) — what was built in each phase
