# PRX Rust PDK

The official Rust Plugin Development Kit for the PRX WASM Plugin System.

Build type-safe, ergonomic plugins for PRX using idiomatic Rust and the WIT
Component Model. The PDK wraps all host functions in clean modules so you can
focus on plugin logic rather than low-level ABI details.

## Quick Start

### 1. Install cargo-component

```sh
cargo install cargo-component
rustup target add wasm32-wasip2
```

### 2. Create a new plugin

```sh
# Tool plugin (LLM-callable)
cargo new --lib my-tool
cd my-tool
```

Edit `Cargo.toml`:

```toml
[package]
name = "my-tool"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib", "rlib"]

[dependencies]
prx-pdk = { git = "https://github.com/openprx/openprx", subdirectory = "pdk/rust/prx-pdk" }
wit-bindgen = { version = "0.51", default-features = false, features = ["macros"] }

[package.metadata.component]
package = "prx:plugin@0.1.0"
```

Edit `src/lib.rs`:

```rust
use prx_pdk::prelude::*;

pub struct MyTool;

impl MyTool {
    pub fn get_spec_impl() -> ToolSpec {
        ToolSpec {
            name: "my_tool".to_string(),
            description: "What this tool does".to_string(),
            parameters_schema: r#"{"type":"object","properties":{"input":{"type":"string"}},"required":["input"]}"#.to_string(),
        }
    }

    pub fn execute_impl(args_json: &str) -> PluginResult {
        let args: JsonValue = serde_json::from_str(args_json)
            .unwrap_or_default();
        let input = args["input"].as_str().unwrap_or("");

        log::info(&format!("Processing: {input}"));
        let count = kv::increment("call_count", 1).unwrap_or(0);
        log::debug(&format!("Call #{count}"));

        PluginResult::ok(format!("Processed: {input}"))
    }
}

// Wire up WIT exports for cargo-component builds
#[cfg(target_arch = "wasm32")]
mod wasm_exports {
    use super::MyTool;
    use bindings::Guest;

    impl Guest for MyTool {
        fn get_spec() -> bindings::ToolSpec {
            let s = MyTool::get_spec_impl();
            bindings::ToolSpec {
                name: s.name,
                description: s.description,
                parameters_schema: s.parameters_schema,
            }
        }
        fn execute(args: String) -> bindings::PluginResult {
            let r = MyTool::execute_impl(&args);
            bindings::PluginResult { success: r.success, output: r.output, error: r.error }
        }
    }
    bindings::export!(MyTool with_types_in bindings);
}
```

### 3. Build and install

```sh
# Build WASM component
cargo component build --release

# Copy to PRX plugins directory
cp target/wasm32-wasip2/release/my_tool.wasm /path/to/plugins/my-tool/plugin.wasm
cp plugin.toml /path/to/plugins/my-tool/
```

---

## Host Function Reference

### `prx_pdk::log`

```rust
log::trace("low-level trace");
log::debug("debugging info");
log::info("informational");
log::warn("warning");
log::error("error occurred");
```

### `prx_pdk::config`

Read-only access to `[config]` values from `plugin.toml`.

```rust
let timeout = config::get_or("timeout_ms", "5000");
let all = config::get_all(); // Vec<(String, String)>
```

### `prx_pdk::kv`

Isolated per-plugin persistent key-value store.

```rust
// Bytes
kv::set("key", b"value").unwrap();
let bytes: Option<Vec<u8>> = kv::get("key");

// Strings
kv::set_str("name", "Alice").unwrap();
let name = kv::get_str("name");

// JSON
kv::set_json("config", &my_struct).unwrap();
let cfg: MyStruct = kv::get_json("config").unwrap();

// Atomic counter
let count = kv::increment("calls", 1).unwrap();

// List keys
let keys = kv::list_keys("prefix:");
```

### `prx_pdk::events`

Fire-and-forget publish/subscribe event bus.

```rust
// Publish (requires "events" permission)
events::publish("my.event", r#"{"status":"ok"}"#).unwrap();
events::publish_json("my.event", &my_struct).unwrap();

// Subscribe (hook plugins)
let sub_id = events::subscribe("prx.lifecycle.*").unwrap();
events::unsubscribe(sub_id).unwrap();
```

### `prx_pdk::http`

Outbound HTTP (requires `"http-outbound"` permission + `http_allowlist`).

```rust
let resp = http::get("https://api.example.com/data", &[]).unwrap();
let json: serde_json::Value = resp.json().unwrap();

let resp = http::post_json(
    "https://api.example.com/submit",
    &[("Authorization", "Bearer token")],
    &payload,
).unwrap();
println!("Status: {}", resp.status);
```

### `prx_pdk::clock`

```rust
let ts_ms: u64 = clock::now_ms(); // Unix milliseconds
let tz: &str = clock::timezone(); // "UTC"
```

### `prx_pdk::memory`

Long-term memory (requires `"memory"` permission).

```rust
let id = memory::store("important fact", "fact").unwrap();
let entries = memory::recall("query about facts", 5).unwrap();
for entry in entries {
    println!("{}: {} (importance: {})", entry.id, entry.text, entry.importance);
}
```

---

## Plugin Types

### Tool Plugin

Responds to LLM tool calls. Implements `prx:plugin/tool-exports`.

```toml
[[capabilities]]
type = "tool"
name = "my_tool"
```

### Hook Plugin

Observes lifecycle events without modifying them. Implements `prx:plugin/hook-exports`.

```toml
[[capabilities]]
type = "hook"

[[capabilities.events]]
pattern = "prx.lifecycle.*"
```

### Middleware Plugin

Transforms data at pipeline stages. Implements `prx:plugin/middleware-exports`.

```toml
[[capabilities]]
type = "middleware"
priority = 50
```

Receives `stage` (one of `"inbound"`, `"outbound"`, `"llm_request"`, `"llm_response"`)
and `data_json`. Returns modified JSON.

### Cron Plugin

Runs on a schedule. Implements `prx:plugin/cron-exports`.

```toml
[[capabilities]]
type = "cron"
schedule = "0 * * * *"  # every hour
```

---

## Permissions

Declare required/optional permissions in `plugin.toml`:

```toml
[permissions]
required = ["log", "kv", "events"]
optional = ["http-outbound", "memory"]
```

| Permission | Enables |
|------------|---------|
| `log` | `log::*` functions |
| `config` | `config::*` functions |
| `kv` | `kv::*` functions |
| `events` | `events::*` functions |
| `http-outbound` | `http::*` functions |
| `memory` | `memory::*` functions |

---

## Version Compatibility

| prx-pdk | wit-bindgen | wasmtime (host) |
|---------|-------------|-----------------|
| 0.1.x | 0.51.x | 31.x |

The PDK uses `wit-bindgen = "0.51"`, which aligns with the wasmtime 31.x runtime
embedded in PRX. Always use matching versions to ensure ABI compatibility.

---

## Examples

| Example | Plugin Type | Demonstrates |
|---------|------------|--------------|
| [`base64-tool`](examples/base64-tool/) | Tool | JSON args, KV counters, pure-Rust logic |
| [`audit-hook`](examples/audit-hook/) | Hook | Event subscriptions, KV state, event publishing |

---

## Development Without cargo-component

All crates compile as `rlib` on the host for development and testing:

```sh
# No cargo-component needed for these commands:
cargo build          # compile as rlib
cargo test           # run unit tests
cargo doc --no-deps  # generate docs
```

The WASM-specific export wiring (wit-bindgen `Guest` trait, `export!` macro) is
guarded by `#[cfg(target_arch = "wasm32")]` and excluded from host builds.

---

## Directory Structure

```
pdk/rust/
├── README.md              ← this file
├── prx-pdk/
│   ├── Cargo.toml
│   ├── src/lib.rs         ← all wrapper modules + types + macros
│   └── wit/               ← WIT definitions (copied from main project)
│       ├── package.wit    ← prx:host@0.1.0
│       ├── log.wit
│       ├── config.wit
│       ├── kv.wit
│       ├── event.wit
│       ├── http.wit
│       ├── memory.wit
│       └── pdk-worlds.wit ← pdk-full world for bindgen
└── examples/
    ├── base64-tool/       ← Tool plugin example
    └── audit-hook/        ← Hook plugin example
```
