# PRX WASM Plugin Developer Guide

Plugins are WebAssembly components (Wasmtime 50.0.1, Component Model, WIT
package `prx:plugin@0.1.0`). The runtime is compiled in by default
(`wasm-plugins`) and does nothing until `[plugins] enabled = true` (default
`false`). A plugin then runs only when the operator's trust file names that
exact component hash, manifest hash and permission list.

Host function signatures, errors and limits: [host-function-reference.md](host-function-reference.md).
Runtime generation, trap rebuild and middleware stages: [plugin-runtime-lifecycle.md](plugin-runtime-lifecycle.md).

---

## Table of Contents

1. [Enable, trust and install](#1-enable-trust-and-install)
2. [Plugin structure](#2-plugin-structure)
3. [Capability types](#3-capability-types)
4. [Host functions](#4-host-functions)
5. [Permissions and allowlist](#5-permissions-and-allowlist)
6. [Resource limits](#6-resource-limits)
7. [Languages](#7-languages)
8. [Operator CLI](#8-operator-cli)
9. [Reload](#9-reload)
10. [Troubleshooting](#10-troubleshooting)

---

## 1. Enable, trust and install

1. Set `[plugins] enabled = true` and restart PRX.
2. Stage a directory **inside the workspace** that contains `plugin.toml` and
   the file named by `plugin.wasm` (default `plugin.wasm`). Do not copy into
   `<workspace>/plugins/` by hand: that directory is scanned, but a plugin
   without a matching trust record is listed with an error and never compiled.
3. Install with `wasm_plugins_manage`:

   ```json
   {"action": "install", "source": "plugin-sources/echo-tool", "grant_permissions": ["kv"]}
   ```

   `install` and `update` are high-risk. The approval names the plugin, the
   SHA-256 of the component, the SHA-256 of `plugin.toml`, and
   `grant_permissions`. That list must include every `required` permission and
   may list only permissions the manifest declares. Optional
   `expected_sha256` / `expected_manifest_sha256` bind the call to reviewed
   bytes.

`wasm_plugins_manage` actions: `status`, `get`, `refresh`, `install`,
`update`, `enable`, `disable`, `remove`, plus adapter probes
(`middleware_test`, `cron_run`, `provider_chat`, `storage_health`,
`storage_store`, `storage_recall`, `storage_forget`, `storage_count`).
`wasm_plugin_reload` reloads one named plugin. `wasm_plugin_call` invokes a
loaded tool plugin. `wasm_plugins_status` reports the loaded generation.

The trust file `plugin-trust.json` lives in the PRX state directory next to
`config.toml`, outside the workspace, mode `0600`. A symlink, a group/other
readable file, or a malformed file means nothing is trusted. Plugins are
refused entirely when the state directory lies inside the workspace. There is
no marker file, manifest field or `trusted_plugins` setting that confers
trust. The compiled-component cache is `plugin-cache/` beside the trust file.

At most 256 plugins. A component larger than 128 MiB or a manifest larger than
256 KiB is refused. A symlink manifest is refused.

---

## 2. Plugin structure

```
my-plugin/
├── plugin.wasm
└── plugin.toml
```

### `plugin.toml`

```toml
[plugin]
name = "echo-tool"
version = "0.1.0"
description = "Echo input text"
wasm = "plugin.wasm"
# api_version defaults to "1"
# author and license are optional strings

[[capabilities]]
type = "tool"          # tool | hook | middleware | cron | provider | storage | channel
name = "echo"
description = "Echo the input text"
# events = ["prx.lifecycle.*"]   # hook; "*" matches all; trailing * is prefix
# priority = 100                 # middleware, lower runs first
# schedule = "0 * * * *"         # cron, 5-field, required for cron

[permissions]
required = ["kv"]
optional = []
# http_allowlist = ["https://api.example.com/*"]
# filesystem_allowlist is accepted and unused

[resources]
max_fuel = 1_000_000_000
max_memory_mb = 64
max_execution_time_ms = 30000
max_http_requests_per_call = 10
# max_http_requests_per_minute = 60
max_kv_storage_mb = 10
# max_payload_bytes = 65536
# pool_size is accepted and unused

[config]
# string values only; read via prx:host/config
```

| Field | Default / rule |
|-------|----------------|
| `plugin.name` | required; 1–128 ASCII letters, digits, `-` or `_` |
| `plugin.version` | required, non-empty |
| `plugin.wasm` | `plugin.wasm` |
| `plugin.api_version` | `"1"` |
| `capabilities` | at least one; `type` as above; `name` non-empty |
| `capabilities[].schedule` | required for `cron` |
| `capabilities[].events` | strings; a hook may not list an empty pattern |
| `capabilities[].priority` | `100` |
| channel | a plugin declares exactly one `channel` capability |
| `resources.max_payload_bytes` | eighth of `max_memory_mb`, floor 64 KiB; `0` is refused; above linear memory is clamped |

---

## 3. Capability types

Each type targets a world in `wit/worlds.wit` and exports the matching
interface in `wit/plugin/`.

### `tool` — `prx:plugin/tool-exports`

World imports: `log`, `config`, `kv`, `http-outbound`, `memory`, `events`.

```wit
interface tool-exports {
    record tool-spec { name: string, description: string, parameters-schema: string }
    record plugin-result { success: bool, output: string, error: option<string> }
    get-spec: func() -> tool-spec;
    execute: func(args: string) -> plugin-result;
}
```

`get-spec` runs once at load. `execute` receives JSON arguments.

### `hook` — `prx:plugin/hook-exports`

World imports: `log`, `config`, `kv`, `events`.

```wit
interface hook-exports {
    on-event: func(event: string, payload-json: string) -> result<_, string>;
}
```

Hooks do not transform the agent pipeline. They may still call host functions
(KV, `events.publish`). Manifest `events` patterns select lifecycle
deliveries: `"*"` matches every event; a trailing `*` is a prefix; a pattern
also matches with or without a `prx.lifecycle.` prefix. Dynamic
`events.subscribe` deliveries use the topic as `event` and use the subscribe
wildcards (`"*"` / `.*` suffix), which are not the same rules. Only this world
can `subscribe`.

### `middleware` — `prx:plugin/middleware-exports`

World imports: `log`, `config`, `kv`, `events`.

```wit
interface middleware-exports {
    process: func(stage: string, data-json: string) -> result<string, string>;
}
```

`stage` is `inbound`, `outbound`, `llm_request` or `llm_response`. Return
modified JSON. Invalid JSON keeps the original envelope. `outbound` runs once
per accepted delivered turn. See [plugin-runtime-lifecycle.md](plugin-runtime-lifecycle.md).

### `cron` — `prx:plugin/cron-exports`

World imports: `log`, `config`, `kv`, `events`.

```wit
interface cron-exports {
    run: func() -> result<string, string>;
}
```

The host logs the `ok` string. `schedule` is a 5-field cron expression.

### `provider` — `prx:plugin/provider-exports`

World imports: `log`, `config`, `http-outbound`, `events`. No `kv`.

```wit
interface provider-exports {
    record chat-message { role: string, content: string }
    record tool-call { id: string, name: string, arguments: string }
    record chat-response { text: option<string>, tool-calls: list<tool-call> }
    name: func() -> string;
    chat: func(messages: list<chat-message>, model: string, temperature: f64) -> result<chat-response, string>;
}
```

Select as `wasm:<exported-name>` in `default_provider` or a model route.

### `storage` — `prx:plugin/storage-exports`

World imports: `log`, `config`, `http-outbound`, `events`. No `kv`.

```wit
interface storage-exports {
    record memory-entry {
        id: string, key: string, content: string, category: string,
        timestamp: string, score: option<f64>,
    }
    name: func() -> string;
    store-memory: func(key: string, content: string, category: string, session-id: option<string>) -> result<_, string>;
    recall-memory: func(query: string, limit: u32, session-id: option<string>) -> result<list<memory-entry>, string>;
    forget-memory: func(key: string) -> result<bool, string>;
    count-memories: func() -> result<u32, string>;
    health-check: func() -> bool;
}
```

Select as `wasm:<exported-name>` in `memory.backend`.

### `channel` — `prx:plugin/channel-exports`

World imports: `log`, `config`, `http-outbound`, `events`. No `kv`.

```wit
interface channel-exports {
    send: func(message: string) -> result<string, string>;
    poll: func(request: string) -> result<string, string>;
}
```

The host polls every 250 ms with `{}`. `poll` returns a JSON array of at most
32 objects. Each object has `id`, `sender`, `reply_target`, `content`
(required) and optional `is_group`, `thread_ts`. Unknown fields are rejected.
`id` / `sender` / `reply_target` are 1..=256 bytes; `content` is at most
65536 bytes. Return `[]` when idle. Do not replay acknowledged inbound
messages after a guest restart.

`[config] allowed_users` is a comma-separated sender list (`*` allows all).
The host filters senders and sets channel name and timestamp. Declaring a
channel does not grant transport; `http-outbound` still needs permission and
allowlist.

`send` receives JSON with `content`, `recipient`, and optional `subject`,
`thread_ts`, `quote_timestamp`, `quote_author`. An empty recipient is refused
before the guest runs.

A plugin declares exactly one channel capability.

### `base-plugin`

World imports `log`, `config`, `kv` only. No exports. Used as a minimal import
set, not as an installable capability type.

---

## 4. Host functions

| Interface | WIT | Permission |
|-----------|-----|------------|
| `log` | `wit/host/log.wit` | always granted |
| `config` | `wit/host/config.wit` | always granted |
| `kv` | `wit/host/kv.wit` | `kv` |
| `http-outbound` | `wit/host/http.wit` | `http-outbound` |
| `memory` | `wit/host/memory.wit` | `memory` |
| `events` | `wit/host/event.wit` | `events` |
| `websocket-outbound` | none (host-linked) | `websocket-outbound` |

`kv` is an in-memory map **per capability adapter**. A tool and a hook from the
same plugin do not share keys. It survives a trap rebuild of that adapter
only. It does not persist across a process restart or a new loaded generation.

`events.publish` payload max 64 KiB (a string; the host does not parse JSON);
topic 1..=256 bytes; 4096 subscriptions; per-subscriber queue 1024. A guest
`publish` while handling a delivered event uses that event's stored depth as
the publish argument, so the next delivery is stored as argument+1. A
publish outside any event delivery (lifecycle `on-event`, an independent
capability call) uses argument 0. Host recovery announcements and a rebuild
instantiation that happen because of a delivery inherit that delivery's
depth. Publish arguments 0..=8 are accepted; 9 or above is rejected with
`"event bus: recursion depth limit (8) exceeded for topic '…'"`. A
one-to-one republish chain therefore has nine deliveries (stored depths
1..=9); the ninth delivery's republish is the rejected hop. `subscribe` is
hook-only. Patterns: `"*"` (every topic), `.*` suffix (prefix), otherwise
exact; pattern 1..=256 bytes.

`http-outbound.request` methods: GET, POST, PUT, DELETE, PATCH, HEAD. Response
body cap 16 MiB. At most 5 redirects. Private, loopback, link-local, CGNAT and
metadata addresses are refused.

Signatures, errors and the world matrix: [host-function-reference.md](host-function-reference.md).

### Rust sketch (tool)

```rust
use prx_pdk::prelude::*;

pub struct EchoTool;

impl EchoTool {
    pub fn get_spec_impl() -> ToolSpec {
        ToolSpec {
            name: "echo".into(),
            description: "Echo the input text.".into(),
            parameters_schema: r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}"#.into(),
        }
    }

    pub fn execute_impl(args_json: &str) -> PluginResult {
        let args: serde_json::Value = match serde_json::from_str(args_json) {
            Ok(v) => v,
            Err(e) => return PluginResult::err(format!("Bad args: {e}")),
        };
        let Some(text) = args["text"].as_str() else {
            return PluginResult::err("Missing 'text' parameter");
        };
        log::info(&format!("echo called with: {text}"));
        PluginResult::ok(text)
    }
}
```

`pdk/rust/prx-pdk` generates host wrappers from WIT. PDK helpers such as
`kv::increment` are not host functions.

---

## 5. Permissions and allowlist

A manifest can only ask. The trust record grants. Calling a host function
without the grant returns an error to the guest (`kv.get` / `kv.list-keys`
return empty).

| Permission | Always granted? |
|------------|-----------------|
| `log`, `config` | yes (for a trusted plugin) |
| `kv`, `events`, `http-outbound`, `websocket-outbound`, `memory` | no |

`required` permissions must all appear in the trust record or the plugin is
refused. `optional` permissions are granted only when listed there; otherwise
the plugin loads without them.

`http_allowlist` applies to `http-outbound` and `websocket-outbound`. Empty
denies every URL. Pattern `scheme://host[:port][/path-prefix][*]`:

- scheme exact (`https` ≠ `http`, `wss` ≠ `ws`)
- host exact, or `*.suffix` on a label boundary (`*.example.com` matches
  `a.example.com`, not `example.com`)
- port exact, or the scheme default (80 / 443) when omitted
- path prefix on a segment boundary
- no userinfo, query or fragment

An invalid pattern is a manifest load error.

---

## 6. Resource limits

| Key | Default | Meaning |
|-----|---------|---------|
| `max_fuel` | `1_000_000_000` | Wasmtime fuel per invocation |
| `max_memory_mb` | `64` | Guest linear memory |
| `max_execution_time_ms` | `30000` | Epoch deadline / I/O timeout |
| `max_http_requests_per_call` | `10` | HTTP `request` and WebSocket `connect` per invocation |
| `max_http_requests_per_minute` | unset | Optional rolling 60-second cap, shared by plugin name across adapters and rebuilds |
| `max_kv_storage_mb` | `10` | In-memory KV per capability adapter, keys plus values |
| `max_payload_bytes` | 1/8 of memory, floor 64 KiB | Largest host-to-guest payload |

A trap poisons the Wasmtime `Store`. The next call rebuilds a new store,
reusing the same in-memory KV namespace. Repeated traps quarantine the
adapter. See [plugin-runtime-lifecycle.md](plugin-runtime-lifecycle.md).

---

## 7. Languages

WIT is the ABI. Language PDKs live in-tree:

| Language | Path |
|----------|------|
| Rust | `pdk/rust/` |
| Python | `pdk/python/` |
| JavaScript/TypeScript | `pdk/javascript/` |
| Go | `pdk/go/` |

Rust example: `pdk/rust/examples/echo-channel` (channel fixture, no network
permissions). Build:

```bash
cargo build --locked --release --target wasm32-wasip2 --manifest-path pdk/rust/examples/echo-channel/Cargo.toml
```

The optional scaffold CLI is `pdk/cli` (`prx-plugin new|build|validate|test|pack`).
That binary is not `prx plugin`.

---

## 8. Operator CLI

`prx plugin` inspects and authorizes artifacts. It does not compile guests
except through a running gateway reload/enable.

| Command | Effect |
|---------|--------|
| `prx plugin list` | Artifact identities and trust; no guest execution |
| `prx plugin inspect SOURCE` | Same for one bundle |
| `prx plugin trust SOURCE --sha256 HASH --manifest-sha256 HASH --permission P` | Preview; `--yes` writes the record |
| `prx plugin revoke NAME` | Preview; `--yes` removes the record |
| `prx plugin reload NAME --gateway-url URL` | Preview; `--token-from-stdin --yes` posts `POST /api/plugins/NAME/reload` |
| `prx plugin enable NAME --gateway-url URL` | Preview; `--token-from-stdin --yes` posts `POST /api/plugins/NAME/enable`; does not write trust or change `[plugins] enabled` |

Reload and enable require HTTPS, or HTTP to a numeric loopback origin, with
no path, query, fragment or userinfo. The token is piped on stdin (max 4096
bytes). Read-only autonomy refuses `--yes` on trust, reload and enable. A name
absent from the loaded generation is not first-discovered by enable or reload.
Apply a local trust change with reload or by restarting the owning runtime.

Gateway console install carries `source`, `sha256`, `manifest_sha256` and
`permissions` and records trust after the component compiles.

---

## 9. Reload

There is no directory watcher, inotify, or FSEvents path. A new `.wasm` on
disk does nothing until `wasm_plugin_reload`, `prx plugin reload`,
`wasm_plugins_manage` `update`/`refresh`, or a process restart loads it.

Reload publishes a complete new generation. In-flight guest calls keep their
store. Each adapter's KV map in the new generation starts empty.

---

## 10. Troubleshooting

**Fuel / timeout.** Raise `max_fuel` or `max_execution_time_ms`. A hang is
ended by the plugin epoch deadline, not by the caller's deadline.

**`permission 'http-outbound' denied`.** Add it to `permissions.required` and
to `grant_permissions`. Add allowlist entries. Private addresses are refused
even when listed.

**KV empty after restart, reload, or in another capability of the same
plugin.** Expected: the store is in-memory and per adapter.

**`subscribe` fails on a tool plugin.** Only hook-capable instances have an
event sink.

**`unsupported plugin capability type`.** Allowed types are `tool`, `hook`,
`middleware`, `cron`, `provider`, `storage`, `channel`.

**Channel batch refused.** At most 32 messages; field bounds as in
[channel](#channel--prxpluginchannel-exports).

**Missing export.** The compiled component must export the interface for every
declared capability (`get-spec`/`execute`, `on-event`, `process`, `run`,
`name`/`chat`, storage exports, or `send`/`poll`).

Contact: `info@sylvode.org`.
