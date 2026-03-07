# audit-hook

Example PRX hook plugin that audits lifecycle events. Demonstrates:

- Implementing the `prx:plugin/hook-exports` WIT interface
- Wildcard event subscription (`prx.lifecycle.*`)
- KV-based persistent counters
- Event bus publishing
- Structured JSON payload parsing

## What It Does

Every time a PRX lifecycle event fires, this hook:

1. Increments a per-event counter in KV (`count:<event_name>`)
2. Increments the global total counter (`count:total`)
3. Records the last-seen timestamp (`last_ts:<event_name>`)
4. For `tool_call` events: records per-tool invocation counts
5. Every 100 events: publishes a `prx.audit.milestone` event with a summary

## Event Subscription

The plugin subscribes to `prx.lifecycle.*`, which matches:

| Event | Meaning |
|-------|---------|
| `prx.lifecycle.agent_start` | Agent session started |
| `prx.lifecycle.agent_stop`  | Agent session ended |
| `prx.lifecycle.tool_call`   | LLM invoked a tool |
| `prx.lifecycle.error`       | Runtime error occurred |

## KV Schema

| Key | Type | Description |
|-----|------|-------------|
| `count:total` | `i64` | Total events handled |
| `count:<event>` | `i64` | Count per event name |
| `count:tool:<name>` | `i64` | Count per tool name |
| `last_ts:<event>` | `u64` | Unix ms of last event |

## Building

```sh
cargo install cargo-component
rustup target add wasm32-wasip2
cargo component build --release
cp target/wasm32-wasip2/release/audit_hook.wasm plugin.wasm
```

## Development

```sh
cargo build   # host rlib (no WASM)
cargo test    # unit tests
```

## Querying Counts

From another plugin or tool, read the audit data:

```rust
use prx_pdk::kv;

let total: i64 = kv::get_json("count:total").unwrap_or(0);
let tool_calls: i64 = kv::get_json("count:prx.lifecycle.tool_call").unwrap_or(0);
```

Note: KV namespaces are isolated per plugin. Only this plugin can read its own KV data.
