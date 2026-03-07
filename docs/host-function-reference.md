# Host Function Reference

> **Package:** `prx:host@0.1.0`  
> **WIT source:** `wit/host/`  
> **Version:** 0.1

This document is the authoritative reference for all host functions available to PRX WASM plugins. Each interface corresponds to a WIT file in `wit/host/`.

---

## Table of Contents

1. [`log` — Structured Logging](#1-log--structured-logging)
2. [`config` — Plugin Configuration](#2-config--plugin-configuration)
3. [`kv` — Key-Value Storage](#3-kv--key-value-storage)
4. [`http-outbound` — Outbound HTTP](#4-http-outbound--outbound-http)
5. [`memory` — Long-Term Memory](#5-memory--long-term-memory)
6. [`events` — Event Bus](#6-events--event-bus)

---

## 1. `log` — Structured Logging

**Interface:** `prx:host/log`  
**WIT file:** `wit/host/log.wit`  
**Permission required:** None (always available)

Emit structured log messages that integrate with PRX's tracing infrastructure. All log messages are tagged with the emitting plugin's name.

### WIT Definition

```wit
interface log {
    enum level {
        trace,
        debug,
        info,
        warn,
        error,
    }

    log: func(level: level, message: string);
}
```

### Functions

#### `log`

Emit a log message at the specified severity level.

| Parameter | Type | Description |
|-----------|------|-------------|
| `level` | `level` | Severity level (see below) |
| `message` | `string` | Log message text |

**Returns:** nothing

**Severity levels:**

| Level | Value | Use case |
|-------|-------|----------|
| `trace` | 0 | Extremely verbose internal steps; disabled in production by default |
| `debug` | 1 | Development-time debugging information |
| `info` | 2 | Normal operational messages (most common) |
| `warn` | 3 | Unexpected but recoverable situations |
| `error` | 4 | Failures that affect plugin functionality |

### PDK Usage

**Rust:**
```rust
use prx_pdk::prelude::*;

log::trace("entering hot path");
log::debug(&format!("processing item: {id}"));
log::info("operation completed successfully");
log::warn(&format!("retrying after error: {e}"));
log::error(&format!("fatal: {e}"));
```

**Python:**
```python
from prx_pdk import host
host.log.trace("entering hot path")
host.log.debug(f"processing item: {id}")
host.log.info("operation completed successfully")
host.log.warn(f"retrying after error: {e}")
host.log.error(f"fatal: {e}")
```

**TypeScript:**
```typescript
import { log } from "@prx/pdk";
log.trace("entering hot path");
log.debug(`processing item: ${id}`);
log.info("operation completed successfully");
log.warn(`retrying after error: ${e}`);
log.error(`fatal: ${e}`);
```

**Go:**
```go
import "github.com/openprx/prx-pdk-go/host/log"
log.Trace("entering hot path")
log.Debug("processing item: " + id)
log.Info("operation completed successfully")
log.Warn("retrying after error: " + e.Error())
log.Error("fatal: " + e.Error())
```

---

## 2. `config` — Plugin Configuration

**Interface:** `prx:host/config`  
**WIT file:** `wit/host/config.wit`  
**Permission required:** `"config"` (always granted)

Provides read-only access to plugin-specific configuration values defined in the `[config]` section of `plugin.toml`. Values are set at deploy time and cannot be changed at runtime.

### WIT Definition

```wit
interface config {
    get: func(key: string) -> option<string>;
    get-all: func() -> list<tuple<string, string>>;
}
```

### Functions

#### `get`

Get a single configuration value by key.

| Parameter | Type | Description |
|-----------|------|-------------|
| `key` | `string` | Configuration key |

**Returns:** `option<string>` — the value, or `none` if the key is not set

---

#### `get-all`

Get all configuration key-value pairs.

**Returns:** `list<tuple<string, string>>` — all key-value pairs defined in `[config]`

### PDK Usage

**Rust:**
```rust
use prx_pdk::prelude::*;

// Get a value (returns Option<String>)
let api_key = config::get("api_key");

// Get with a default fallback
let timeout = config::get_or("timeout_ms", "5000");

// Get all key-value pairs
let all: Vec<(String, String)> = config::get_all();
for (k, v) in &all {
    log::debug(&format!("config: {k} = {v}"));
}
```

**Python:**
```python
from prx_pdk import host
value = host.config.get("api_key")               # Optional[str]
value = host.config.get_or("timeout_ms", "5000") # str
pairs = host.config.get_all()                    # list[tuple[str, str]]
```

**TypeScript:**
```typescript
import { config } from "@prx/pdk";
const apiKey = config.get("api_key");              // string | undefined
const timeout = config.getOr("timeout_ms", "5000"); // string
const all = config.getAll();                       // [string, string][]
```

**Go:**
```go
import "github.com/openprx/prx-pdk-go/host/config"
val, ok := config.Get("api_key")
timeout := config.GetOr("timeout_ms", "5000")
pairs := config.GetAll()  // [][2]string
```

### Example `plugin.toml`

```toml
[config]
api_key       = "sk-..."
base_url      = "https://api.example.com/v1"
timeout_ms    = "5000"
max_retries   = "3"
debug         = "false"
```

---

## 3. `kv` — Key-Value Storage

**Interface:** `prx:host/kv`  
**WIT file:** `wit/host/kv.wit`  
**Permission required:** `"kv"`

Isolated persistent key-value storage. Each plugin has its own namespace — plugins cannot read or write each other's keys. Data persists across plugin reloads and PRX restarts.

### WIT Definition

```wit
interface kv {
    get: func(key: string) -> option<list<u8>>;
    set: func(key: string, value: list<u8>) -> result<_, string>;
    delete: func(key: string) -> result<bool, string>;
    list-keys: func(prefix: string) -> list<string>;
}
```

### Functions

#### `get`

Retrieve a value by key.

| Parameter | Type | Description |
|-----------|------|-------------|
| `key` | `string` | Key to retrieve |

**Returns:** `option<list<u8>>` — the raw bytes value, or `none` if the key does not exist

---

#### `set`

Store a value. Overwrites any existing value for the key.

| Parameter | Type | Description |
|-----------|------|-------------|
| `key` | `string` | Key to store |
| `value` | `list<u8>` | Raw bytes to store |

**Returns:** `result<_, string>` — `ok(())` on success, `err(message)` on failure (e.g., storage limit exceeded)

---

#### `delete`

Delete a key.

| Parameter | Type | Description |
|-----------|------|-------------|
| `key` | `string` | Key to delete |

**Returns:** `result<bool, string>` — `ok(true)` if the key existed and was deleted, `ok(false)` if the key did not exist, `err(message)` on storage error

---

#### `list-keys`

List all keys matching a prefix.

| Parameter | Type | Description |
|-----------|------|-------------|
| `prefix` | `string` | Key prefix to filter by; use `""` to list all keys |

**Returns:** `list<string>` — all keys with the given prefix, in unspecified order

### Notes

- **Namespace isolation:** Keys are automatically namespaced per plugin. Two plugins can both have a key named `"state"` without conflict.
- **Storage limit:** Controlled by `resources.max_kv_storage_kb` in `plugin.toml` (default: 1024 KB). Writing data that exceeds the limit returns an error.
- **Value type:** Raw bytes (`list<u8>`). Use PDK helpers for string and JSON serialization.
- **Atomicity:** Individual `get`/`set`/`delete` operations are atomic. Multi-key transactions are not supported.

### PDK Convenience Helpers

The PDK adds `has`, `increment`, `get_str`/`set_str`, and `get_json`/`set_json` on top of the core WIT functions:

**Rust:**
```rust
use prx_pdk::prelude::*;

// Raw bytes
kv::set("raw", b"hello world").unwrap();
let bytes: Option<Vec<u8>> = kv::get("raw");
let existed = kv::delete("raw").unwrap();  // bool
let keys = kv::list_keys("prefix:");       // Vec<String>

// String helpers
kv::set_str("name", "Alice").unwrap();
let name: Option<String> = kv::get_str("name");

// JSON helpers (requires serde)
kv::set_json("user", &my_struct).unwrap();
let user: MyStruct = kv::get_json("user").unwrap();

// Atomic counter (PDK wrapper using get+set)
let count: i64 = kv::increment("call_count", 1).unwrap();

// Existence check (PDK wrapper using get)
let exists = kv::has("my_key");
```

**Python:**
```python
from prx_pdk import host

host.kv.set("raw", b"hello")
data: bytes | None = host.kv.get("raw")

host.kv.set_str("name", "Alice")
name: str | None = host.kv.get_str("name")

host.kv.set_json("config", {"debug": True})
obj = host.kv.get_json("config")

existed = host.kv.delete("raw")   # bool
keys = host.kv.list_keys("")       # list[str]
count = host.kv.increment("calls", delta=1)  # int (new value)
```

**TypeScript:**
```typescript
import { kv } from "@prx/pdk";

kv.set("raw", new Uint8Array([104, 101, 108, 108, 111]));
const bytes = kv.get("raw");         // Uint8Array | undefined

kv.setString("name", "Alice");
const name = kv.getString("name");  // string | undefined

kv.setJson("config", { debug: true });
const config = kv.getJson<{ debug: boolean }>("config");

kv.delete("raw");                   // boolean
kv.listKeys("");                    // string[]
kv.increment("calls", 1);          // number (new value)
```

**Go:**
```go
import "github.com/openprx/prx-pdk-go/host/kv"

_ = kv.Set("raw", []byte("hello"))
data, ok := kv.Get("raw")

_ = kv.SetString("name", "Alice")
name, ok := kv.GetString("name")

_ = kv.SetJSON("state", jsonBytes)
data, ok = kv.GetJSON("state")

existed, err := kv.Delete("raw")
keys := kv.ListKeys("")
```

### Example: Persistent Counter

```rust
pub fn execute_impl(args_json: &str) -> PluginResult {
    // Increment call counter
    let calls = kv::increment("total_calls", 1).unwrap_or(0);
    log::info(&format!("Invocation #{calls}"));

    // Store last invocation time
    let now = clock::now_ms().to_string();
    kv::set_str("last_call_ms", &now).unwrap();

    // ... main logic ...
    PluginResult::ok("done")
}
```

---

## 4. `http-outbound` — Outbound HTTP

**Interface:** `prx:host/http-outbound`  
**WIT file:** `wit/host/http.wit`  
**Permission required:** `"http-outbound"` + `http_allowlist` in `plugin.toml`

Make controlled outbound HTTP requests. All URLs are validated against the plugin's `http_allowlist` before any network connection is made.

### WIT Definition

```wit
interface http-outbound {
    record http-response {
        status: u16,
        headers: list<tuple<string, string>>,
        body: list<u8>,
    }

    request: func(
        method: string,
        url: string,
        headers: list<tuple<string, string>>,
        body: option<list<u8>>,
    ) -> result<http-response, string>;
}
```

### Types

#### `http-response`

| Field | Type | Description |
|-------|------|-------------|
| `status` | `u16` | HTTP status code (e.g., `200`, `404`, `500`) |
| `headers` | `list<tuple<string, string>>` | Response headers as name-value pairs |
| `body` | `list<u8>` | Response body as raw bytes |

### Functions

#### `request`

Make an HTTP request.

| Parameter | Type | Description |
|-----------|------|-------------|
| `method` | `string` | HTTP method: `"GET"`, `"POST"`, `"PUT"`, `"PATCH"`, `"DELETE"`, `"HEAD"`, `"OPTIONS"` |
| `url` | `string` | Full URL including scheme, host, and path |
| `headers` | `list<tuple<string, string>>` | Request headers as name-value pairs |
| `body` | `option<list<u8>>` | Request body bytes, or `none` for no body |

**Returns:** `result<http-response, string>` — the response on success, or an error message on failure

**Failure reasons:**
- URL not in `http_allowlist`
- Network error (DNS failure, connection refused, timeout)
- Request count limit reached (`max_http_requests`)

### Declaring the Permission

```toml
[permissions]
required = ["http-outbound"]

# Required when using http-outbound: list of allowed origins
http_allowlist = [
    "https://api.openweathermap.org",
    "https://api.github.com",
    "https://httpbin.org",
]
```

### PDK Usage

**Rust:**
```rust
use prx_pdk::prelude::*;

// GET request
let resp = http::get("https://api.example.com/data", &[]).unwrap();
println!("Status: {}", resp.status);
let body_str = resp.body_text();           // String (UTF-8 decoded)
let json: serde_json::Value = resp.json().unwrap();

// POST with JSON body
let payload = serde_json::json!({ "city": "London" });
let resp = http::post_json(
    "https://api.example.com/weather",
    &[("Authorization", "Bearer my-token")],
    &payload,
).unwrap();

// Generic request
let resp = http::request(
    "DELETE",
    "https://api.example.com/items/42",
    &[
        ("Authorization", "Bearer my-token"),
        ("X-Request-Id", "abc-123"),
    ],
    None,  // no body
).unwrap();

// PUT with raw body
let body = b"raw body bytes";
let resp = http::request(
    "PUT",
    "https://api.example.com/blob/key",
    &[("Content-Type", "application/octet-stream")],
    Some(body),
).unwrap();
```

**Python:**
```python
from prx_pdk import host

# GET
resp = host.http.get("https://api.example.com/data")
print(resp.status)       # int
print(resp.text())       # str
print(resp.json())       # any (parsed JSON)

# POST JSON
resp = host.http.post_json(
    "https://api.example.com/weather",
    {"city": "London"},
    headers=[("Authorization", "Bearer token")],
)

# Generic
resp = host.http.request(
    "DELETE",
    "https://api.example.com/items/42",
    headers=[("Authorization", "Bearer token")],
    body=None,
)
```

**TypeScript:**
```typescript
import { http } from "@prx/pdk";

// GET
const resp = http.get("https://api.example.com/data");
const text = http.bodyText(resp);        // string
const json = http.bodyJson<MyType>(resp); // MyType

// POST JSON
const resp2 = http.postJson(
    "https://api.example.com/submit",
    { key: "value" },
    [["Authorization", "Bearer token"]],
);

// Generic
const resp3 = http.request(
    "DELETE",
    "https://api.example.com/items/42",
    [["Authorization", "Bearer token"]],
    undefined,
);
```

**Go:**
```go
import "github.com/openprx/prx-pdk-go/host/http"

headers := [][2]string{{"Authorization", "Bearer token"}}

// GET
resp, err := http.Get("https://api.example.com/data", headers)
fmt.Println(resp.Status, resp.BodyText())

// POST JSON
resp, err = http.PostJSON("https://api.example.com/submit", headers, jsonBody)

// Generic
resp, err = http.Request("DELETE", url, headers, nil)
```

### Error Handling

```rust
match http::get("https://api.example.com/data", &[]) {
    Ok(resp) if resp.status == 200 => {
        let json = resp.json().unwrap();
        // handle success
    }
    Ok(resp) => {
        log::warn(&format!("API returned status {}", resp.status));
    }
    Err(e) => {
        // URL not in allowlist, network error, or limit exceeded
        log::error(&format!("HTTP error: {e}"));
        return PluginResult::err(format!("Request failed: {e}"));
    }
}
```

---

## 5. `memory` — Long-Term Memory

**Interface:** `prx:host/memory`  
**WIT file:** `wit/host/memory.wit`  
**Permission required:** `"memory"`

Access PRX's semantic memory system. Memories are indexed for similarity search, enabling recall by meaning rather than exact key match.

### WIT Definition

```wit
interface memory {
    record memory-entry {
        id: string,
        text: string,
        category: string,
        importance: f64,
    }

    store: func(text: string, category: string) -> result<string, string>;
    recall: func(query: string, limit: u32) -> result<list<memory-entry>, string>;
}
```

### Types

#### `memory-entry`

| Field | Type | Description |
|-------|------|-------------|
| `id` | `string` | Unique entry identifier (UUID) |
| `text` | `string` | The stored text |
| `category` | `string` | Category label for filtering |
| `importance` | `f64` | Relevance score (0.0–1.0), higher = more relevant to query |

### Functions

#### `store`

Store text in memory.

| Parameter | Type | Description |
|-----------|------|-------------|
| `text` | `string` | Text content to store |
| `category` | `string` | Category label (e.g., `"fact"`, `"preference"`, `"entity"`) |

**Returns:** `result<string, string>` — the new entry's ID on success, error message on failure

**Category conventions:**

| Category | Use for |
|----------|---------|
| `"fact"` | Objective facts |
| `"preference"` | User preferences and settings |
| `"decision"` | Decisions made |
| `"entity"` | People, places, organizations |
| `"other"` | Miscellaneous |

---

#### `recall`

Search memories by semantic similarity.

| Parameter | Type | Description |
|-----------|------|-------------|
| `query` | `string` | Natural language search query |
| `limit` | `u32` | Maximum number of results to return |

**Returns:** `result<list<memory-entry>, string>` — matching entries sorted by relevance descending, or error

### PDK Usage

**Rust:**
```rust
use prx_pdk::prelude::*;

// Store a memory
let id = memory::store("User prefers dark mode", "preference")
    .map_err(|e| PluginResult::err(e))?;
log::info(&format!("Stored memory: {id}"));

// Recall by semantic query
let entries = memory::recall("user interface preferences", 5)
    .unwrap_or_default();

for entry in &entries {
    log::debug(&format!(
        "[{:.2}] {}: {}",
        entry.importance, entry.id, entry.text
    ));
}

// Use top result
if let Some(top) = entries.first() {
    log::info(&format!("Best match: {}", top.text));
}
```

**Python:**
```python
from prx_pdk import host

# Store
entry_id = host.memory.store("User prefers dark mode", category="preference")

# Recall
entries = host.memory.recall("user interface preferences", limit=5)
for e in entries:
    print(f"[{e.importance:.2f}] {e.id}: {e.text}")
```

**TypeScript:**
```typescript
import { memory } from "@prx/pdk";
import type { MemoryEntry } from "@prx/pdk";

// Store
const id = memory.store("User prefers dark mode", "preference");

// Recall
const entries: MemoryEntry[] = memory.recall("user interface preferences", 5);
entries.forEach(e => console.log(`[${e.importance.toFixed(2)}] ${e.text}`));
```

**Go:**
```go
import "github.com/openprx/prx-pdk-go/host/memory"

// Store
id, err := memory.Store("User prefers dark mode", "preference")

// Recall
entries, err := memory.Recall("user interface preferences", 5)
for _, e := range entries {
    // e.ID, e.Text, e.Category, e.Importance
}
```

---

## 6. `events` — Event Bus

**Interface:** `prx:host/events`  
**WIT file:** `wit/host/event.wit`  
**Permission required:** `"events"`

Fire-and-forget publish/subscribe event bus for inter-plugin communication and integration with PRX lifecycle events. All events flow through the host for auditing and access control.

### WIT Definition

```wit
interface events {
    publish: func(topic: string, payload: string) -> result<_, string>;
    subscribe: func(topic-pattern: string) -> result<u64, string>;
    unsubscribe: func(subscription-id: u64) -> result<_, string>;
}
```

### Functions

#### `publish`

Publish an event to a topic.

| Parameter | Type | Description |
|-----------|------|-------------|
| `topic` | `string` | Event topic (dot-separated, e.g., `"weather.update"`) |
| `payload` | `string` | JSON-encoded event payload (max 64 KB) |

**Returns:** `result<_, string>` — `ok(())` on success, `err(message)` on failure

**Failure reasons:**
- Payload is not valid JSON
- Payload exceeds 64 KB
- Recursion limit reached (plugin publishing to itself)

---

#### `subscribe`

Subscribe to a topic pattern.

| Parameter | Type | Description |
|-----------|------|-------------|
| `topic-pattern` | `string` | Topic pattern: exact (`"tool.call"`) or wildcard (`"tool.*"`) |

**Returns:** `result<u64, string>` — subscription ID on success (use with `unsubscribe`), error message on failure

Subscriptions are active for the lifetime of the plugin invocation or until `unsubscribe` is called.

---

#### `unsubscribe`

Cancel a subscription.

| Parameter | Type | Description |
|-----------|------|-------------|
| `subscription-id` | `u64` | Subscription ID returned by `subscribe` |

**Returns:** `result<_, string>` — `ok(())` on success, `err(message)` if the ID is invalid

### Topic Patterns

| Pattern | Matches |
|---------|---------|
| `"weather.update"` | Exactly `weather.update` only |
| `"weather.*"` | `weather.update`, `weather.alert`, `weather.clear`, etc. |
| `"prx.lifecycle.*"` | All PRX lifecycle events |
| `"*"` | All events (use with care) |

### Built-in PRX Events

| Topic | Description | Payload fields |
|-------|-------------|---------------|
| `prx.lifecycle.agent_start` | Agent loop started | `agent_id: string` |
| `prx.lifecycle.agent_stop` | Agent loop stopped | `agent_id: string, reason: string` |
| `tool.call` | Tool invoked by LLM | `tool_name: string, args: object, session_id: string` |
| `tool.result` | Tool completed | `tool_name: string, success: bool, duration_ms: number` |
| `llm.request` | Request sent to LLM | `model: string, message_count: number` |
| `llm.response` | Response from LLM | `model: string, tokens_used: number` |
| `error` | Error occurred | `message: string, context: string` |

### PDK Usage

**Rust:**
```rust
use prx_pdk::prelude::*;

// Publish
events::publish("my.plugin.result", r#"{"status":"ok","count":42}"#).unwrap();

// Publish with auto-serialized JSON
events::publish_json("my.plugin.result", &serde_json::json!({
    "status": "ok",
    "count": 42,
})).unwrap();

// Subscribe and track subscription
let sub_id = events::subscribe("prx.lifecycle.*").unwrap();
// ... plugin logic ...
events::unsubscribe(sub_id).unwrap();
```

**Python:**
```python
from prx_pdk import host

# Publish
host.events.publish("my.plugin.result", '{"status":"ok","count":42}')
host.events.publish_json("my.plugin.result", {"status": "ok", "count": 42})

# Subscribe
sub_id = host.events.subscribe("prx.lifecycle.*")
# ... plugin logic ...
host.events.unsubscribe(sub_id)
```

**TypeScript:**
```typescript
import { events } from "@prx/pdk";

// Publish
events.publish("my.plugin.result", JSON.stringify({ status: "ok", count: 42 }));
events.publishJson("my.plugin.result", { status: "ok", count: 42 });

// Subscribe (returns bigint subscription ID)
const subId = events.subscribe("prx.lifecycle.*");
// ...
events.unsubscribe(subId);
```

**Go:**
```go
import "github.com/openprx/prx-pdk-go/host/events"

// Publish
err := events.Publish("my.plugin.result", `{"status":"ok","count":42}`)
err = events.PublishJSON("my.plugin.result", jsonPayload)

// Subscribe
id, err := events.Subscribe("prx.lifecycle.*")
// ...
err = events.Unsubscribe(id)
```

### Hook Integration

Hook plugins receive events via the `on-event` export rather than `subscribe`. Use `subscribe` for in-plugin dynamic subscriptions during a single invocation.

```rust
// Hook plugin: event patterns are declared in plugin.toml
// The host calls on-event for matching events
pub fn on_event_impl(event: &str, payload_json: &str) -> Result<(), String> {
    match event {
        "tool.call" => {
            let payload: serde_json::Value = serde_json::from_str(payload_json)
                .unwrap_or_default();
            let tool = payload["tool_name"].as_str().unwrap_or("unknown");
            let _ = kv::increment(&format!("count:{tool}"), 1);
        }
        "error" => {
            // Forward errors as a new event for monitoring plugins
            events::publish("audit.error", payload_json).ok();
        }
        _ => {}
    }
    Ok(())
}
```

### Constraints

| Constraint | Value | Notes |
|------------|-------|-------|
| Max payload | 64 KB | Per event; JSON must be valid |
| Delivery | Async, fire-and-forget | No delivery confirmation |
| Ordering | Best-effort | Events may be reordered under load |
| Recursion | Blocked | Plugin cannot receive events it published to itself |

---

## Appendix: Permission Quick Reference

| Permission | Interface | Worlds available in | Default |
|------------|-----------|--------------------|---------| 
| (none) | `prx:host/log` | all | ✅ always granted |
| `config` | `prx:host/config` | all | ✅ always granted |
| `kv` | `prx:host/kv` | tool, middleware, hook, cron | ❌ must declare |
| `events` | `prx:host/events` | all | ❌ must declare |
| `http-outbound` | `prx:host/http-outbound` | tool, provider, storage | ❌ must declare |
| `memory` | `prx:host/memory` | tool | ❌ must declare |

### World × Host Function Matrix

| Host function | `tool` | `middleware` | `hook` | `cron` | `provider` | `storage` |
|---------------|--------|-------------|--------|--------|-----------|---------|
| `log` | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| `config` | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| `kv` | ✅ | ✅ | ✅ | ✅ | ❌ | ❌ |
| `http-outbound` | ✅ | ❌ | ❌ | ❌ | ✅ | ✅ |
| `memory` | ✅ | ❌ | ❌ | ❌ | ❌ | ❌ |
| `events` | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |

"✅" = interface is imported in the world (may still require `permissions.required` declaration).  
"❌" = not available in this world.

---

## Appendix: WIT Source Files

All WIT definitions are in `wit/host/` relative to the PRX project root:

| File | Interface | Description |
|------|-----------|-------------|
| `wit/host/log.wit` | `prx:host/log` | Structured logging |
| `wit/host/config.wit` | `prx:host/config` | Plugin configuration |
| `wit/host/kv.wit` | `prx:host/kv` | Key-value storage |
| `wit/host/http.wit` | `prx:host/http-outbound` | Outbound HTTP |
| `wit/host/memory.wit` | `prx:host/memory` | Long-term memory |
| `wit/host/event.wit` | `prx:host/events` | Event bus |

World definitions (which interfaces are imported/exported per plugin type) are in `wit/worlds.wit`.

Plugin export interfaces (what plugins must implement) are in `wit/plugin/`:

| File | Interface | Capability |
|------|-----------|-----------|
| `wit/plugin/tool.wit` | `prx:plugin/tool-exports` | `tool` |
| `wit/plugin/hook.wit` | `prx:plugin/hook-exports` | `hook` |
| `wit/plugin/middleware.wit` | `prx:plugin/middleware-exports` | `middleware` |
| `wit/plugin/cron.wit` | `prx:plugin/cron-exports` | `cron` |
| `wit/plugin/provider.wit` | `prx:plugin/provider-exports` | `provider` |
| `wit/plugin/storage.wit` | `prx:plugin/storage-exports` | `storage` |
