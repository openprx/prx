# Host Function Reference

Package `prx:host@0.1.0`. WIT sources live in `wit/host/`. The host also
links `prx:host/websocket-outbound@0.1.0`, which has no file under `wit/host/`.

Plugins are WebAssembly components. They call only the host interfaces their
world imports (see [World matrix](#world-matrix)). A call without the matching
permission returns an error string to the guest (reads on `kv` return empty
instead). `log` and `config` are always granted to a trusted plugin.

Nothing in this document is a filesystem, network, or WASI clock grant. Those
are not host WIT interfaces.

---

## Table of Contents

1. [`log`](#1-log)
2. [`config`](#2-config)
3. [`kv`](#3-kv)
4. [`http-outbound`](#4-http-outbound)
5. [`websocket-outbound`](#5-websocket-outbound)
6. [`memory`](#6-memory)
7. [`events`](#7-events)
8. [World matrix](#world-matrix)
9. [WIT sources](#wit-sources)

---

## 1. `log`

**Interface:** `prx:host/log`  
**WIT:** `wit/host/log.wit`  
**Permission:** none (always granted)

```wit
interface log {
    enum level { trace, debug, info, warn, error }
    log: func(level: level, message: string);
}
```

### `log`

| Parameter | Type |
|-----------|------|
| `level` | `level` (`trace`, `debug`, `info`, `warn`, `error`) |
| `message` | `string` |

**Returns:** nothing. Messages are emitted on the host tracing subscriber and
tagged with the plugin name. An unknown level string is logged at `info`.

---

## 2. `config`

**Interface:** `prx:host/config`  
**WIT:** `wit/host/config.wit`  
**Permission:** none (always granted)

Read-only copy of the `[config]` table in `plugin.toml`. Values are strings set
at load. They do not change for a live instance.

```wit
interface config {
    get: func(key: string) -> option<string>;
    get-all: func() -> list<tuple<string, string>>;
}
```

### `get`

| Parameter | Type |
|-----------|------|
| `key` | `string` |

**Returns:** `option<string>` — `none` when the key is absent.

### `get-all`

**Returns:** `list<tuple<string, string>>` — every `[config]` pair.

---

## 3. `kv`

**Interface:** `prx:host/kv`  
**WIT:** `wit/host/kv.wit`  
**Permission:** `"kv"`

In-memory map on one capability adapter (each adapter allocates its own map).
A tool adapter and a hook adapter from the same plugin do not share keys. The map is **not** written to disk. It survives a trap rebuild of
that adapter only. It is empty after a process restart and after a new loaded
generation (install, update, reload).

```wit
interface kv {
    get: func(key: string) -> option<list<u8>>;
    set: func(key: string, value: list<u8>) -> result<_, string>;
    delete: func(key: string) -> result<bool, string>;
    list-keys: func(prefix: string) -> list<string>;
}
```

### `get`

| Parameter | Type |
|-----------|------|
| `key` | `string` |

**Returns:** `option<list<u8>>`. Missing key and a denied `"kv"` permission
both return `none` (denied also logs a warning).

### `set`

| Parameter | Type |
|-----------|------|
| `key` | `string` |
| `value` | `list<u8>` |

**Returns:** `result<_, string>`. Errors:

- `"permission 'kv' denied for plugin '<name>'"`
- `"permission 'kv' not yet granted for plugin '<name>' (optional, needs approval)"`
- `"plugin KV quota exceeded: requested N bytes, limit M bytes"` — `N` is the
  sum of every key and value byte after the write, including this key.

Quota is `resources.max_kv_storage_mb` (default 10) on that adapter's map,
counted as key bytes plus value bytes. Overwriting a key credits the previous
size. Two adapters of the same plugin each have their own quota.

### `delete`

| Parameter | Type |
|-----------|------|
| `key` | `string` |

**Returns:** `result<bool, string>` — `ok(true)` if the key existed. Permission
errors as for `set`.

### `list-keys`

| Parameter | Type |
|-----------|------|
| `prefix` | `string` |

**Returns:** `list<string>` in unspecified order. Empty prefix lists every key.
A denied `"kv"` permission returns an empty list.

There is no `increment`, `has`, or JSON helper in WIT. PDK wrappers that add
those sit on top of `get`/`set`.

---

## 4. `http-outbound`

**Interface:** `prx:host/http-outbound`  
**WIT:** `wit/host/http.wit`  
**Permission:** `"http-outbound"`

Every request is checked before any connection: permission, `http_allowlist`
(empty denies every URL), the private-network address guard (including after
DNS and on every redirect hop), and the request budget.

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

### `request`

| Parameter | Type |
|-----------|------|
| `method` | `string` — `GET`, `POST`, `PUT`, `DELETE`, `PATCH`, `HEAD` (case-insensitive) |
| `url` | `string` — `http` or `https` only |
| `headers` | `list<tuple<string, string>>` |
| `body` | `option<list<u8>>` |

**Returns:** `result<http-response, string>`. Errors include:

- permission denied / not yet granted
- allowlist or address-guard rejection
- `"plugin '<name>' exceeded max_http_requests_per_call (N)"`
- `"plugin '<name>' exceeded max_http_requests_per_minute (N)"`
- `"unsupported method: …"`
- `"request failed: …"`, `"body read error: …"`
- `"response body exceeds 16777216 bytes"` (16 MiB)
- `"too many redirects (limit 5)"`

The per-call counter resets at the start of each guest invocation. The optional
per-minute window is shared by every adapter of the plugin for the process
lifetime (a trap rebuild does not reset it). Timeout is
`resources.max_execution_time_ms`.

Allowlist pattern: `scheme://host[:port][/path-prefix][*]`. Scheme matches
exactly. Host matches exactly, or as a subdomain when the pattern starts with
`*.`. Port matches the pattern or the scheme default (80 / 443). Path is a
prefix on a segment boundary. Userinfo, query and fragment are refused. An
invalid pattern is a manifest load error.

---

## 5. `websocket-outbound`

**Interface:** `prx:host/websocket-outbound`  
**WIT file:** none (host-linked as `prx:host/websocket-outbound@0.1.0`)  
**Permission:** `"websocket-outbound"`

No world in `wit/worlds.wit` imports this interface. The host still registers
it on every capability linker. A guest that wants it must import
`prx:host/websocket-outbound` in its own WIT.

The same allowlist, address guard and request budget as HTTP apply to
`connect`. Schemes are `ws` and `wss` only. A websocket upgrade that answers
with a redirect is an error. Timeout is `resources.max_execution_time_ms`.

### `connect`

`connect: func(url: string) -> result<u64, string>`

**Returns:** a session id. Errors: permission, allowlist/guard/budget,
`"invalid websocket URL: …"`, `"websocket URL has no host"`,
`"websocket URL has no port"`, `"websocket connect failed: …"`,
`"websocket connect timed out after Nms"`.

### `send`

`send: func(session-id: u64, message: string) -> result<_, string>`

Sends a text frame. Timeout or send failure drops the session.
Errors include `"websocket session N not found"`,
`"websocket send timed out after Nms"`, `"websocket send failed: …"`.

### `receive`

`receive: func(session-id: u64) -> result<string, string>`

Returns the next text frame, or a binary frame decoded as UTF-8. Ping/pong
are skipped. Close or end of stream drops the session and returns
`"websocket session N closed"`. Invalid UTF-8 binary returns
`"websocket binary payload is not utf-8: …"`. Timeout drops the session.

### `close`

`close: func(session-id: u64) -> result<_, string>`

Closes and drops the session. Timeout: `"websocket close timed out after Nms"`.

`connect` spends one request from the HTTP/WebSocket budget. `send` /
`receive` / `close` do not.

---

## 6. `memory`

**Interface:** `prx:host/memory`  
**WIT:** `wit/host/memory.wit`  
**Permission:** `"memory"`

Writes go to the configured PRX memory backend with origin
`plugin:<plugin-name>` and a generated key `plugin:<plugin-name>:<uuid>`.

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

### `store`

| Parameter | Type |
|-----------|------|
| `text` | `string` |
| `category` | `string` — `core`, `daily`, `conversation`, or a custom name |

**Returns:** `result<string, string>` — the new entry key. Errors: permission,
`"memory backend not configured"`, `"memory store failed: …"`.

### `recall`

| Parameter | Type |
|-----------|------|
| `query` | `string` |
| `limit` | `u32` |

**Returns:** `result<list<memory-entry>, string>`. `importance` is the backend
score, or `0.0` when the backend omits one. Errors: permission,
`"memory backend not configured"`, `"memory recall failed: …"`.

---

## 7. `events`

**Interface:** `prx:host/events`  
**WIT:** `wit/host/event.wit`  
**Permission:** `"events"`

In-process publish/subscribe. Delivery is fire-and-forget. Slow subscribers
drop events (queue capacity 1024).

```wit
interface events {
    publish: func(topic: string, payload: string) -> result<_, string>;
    subscribe: func(topic-pattern: string) -> result<u64, string>;
    unsubscribe: func(subscription-id: u64) -> result<_, string>;
}
```

### `publish`

| Parameter | Type |
|-----------|------|
| `topic` | `string` — 1..=256 bytes |
| `payload` | `string` — max 65536 bytes; the host does not parse it as JSON |

**Returns:** `result<_, string>`. Errors: permission, topic length,
`"event bus: payload size N exceeds maximum 65536 bytes"`,
`"event bus: recursion depth limit (8) exceeded for topic '…'"`.

A guest `publish` while handling a delivered event uses that event's stored
depth as the publish argument (the hook pump forwards `EventMessage.depth`
into that invocation). The bus stores the next delivery as argument+1. A
publish outside any event delivery (lifecycle `on-event`, an independent
capability call) uses argument 0. Host recovery announcements
(`prx.plugin.*_quarantined`, `prx.plugin.storage_restarted`) and a rebuild
instantiation that happen because of a delivery inherit that delivery's
depth. Publish arguments 0..=8 are accepted; 9 or above is rejected with
the recursion-limit error above. A one-to-one republish chain therefore has
nine deliveries (stored depths 1..=9); the ninth delivery's republish is the
rejected hop. The host logs that rejection at warn without the payload.

When no event bus is configured, `publish` returns `ok` and drops the event.

### `subscribe`

| Parameter | Type |
|-----------|------|
| `topic-pattern` | `string` — 1..=256 bytes. `"*"` matches every topic. A pattern ending in `.*` is a prefix wildcard (`weather.*` matches `weather.update`). Any other string is an exact topic. `"*"` is not an exact literal topic. |

**Returns:** `result<u64, string>` — subscription id.

`subscribe` requires a hook-capable instance with an event sink. Other worlds
get `"event subscriptions require a hook-capable plugin event handler"`.
Without a bus: `"event bus is not configured"`. At most 4096 active
subscriptions in the process. A repeated subscribe from the same hook sink
and pattern returns the existing id.

Deliveries arrive on the hook `on-event` export; the event name is the topic.

### `unsubscribe`

| Parameter | Type |
|-----------|------|
| `subscription-id` | `u64` |

**Returns:** `result<_, string>`. Without a bus this is `ok`. Permission errors
as for `publish`. Only the plugin that subscribed may remove that id; any
other id, including another plugin's, returns
`"event bus: subscription ID N not found"`.

---

## World matrix

`"import"` means `wit/worlds.wit` imports the interface. The guest still needs
the permission (except `log` / `config`). `"host"` means the linker registers
the interface even though the world does not import it.

| Interface | tool | middleware | hook | cron | provider | storage | channel | base-plugin |
|-----------|------|------------|------|------|----------|---------|---------|-------------|
| `log` | import | import | import | import | import | import | import | import |
| `config` | import | import | import | import | import | import | import | import |
| `kv` | import | import | import | import | — | — | — | import |
| `http-outbound` | import | — | — | — | import | import | import | — |
| `memory` | import | — | — | — | — | — | — | — |
| `events` | import | import | import | import | import | import | import | — |
| `websocket-outbound` | host | host | host | host | host | host | host | — |

`base-plugin` has no exports. `channel` exports `channel-exports` and does not
import `kv`.

---

## WIT sources

Host:

| File | Interface |
|------|-----------|
| `wit/host/log.wit` | `prx:host/log` |
| `wit/host/config.wit` | `prx:host/config` |
| `wit/host/kv.wit` | `prx:host/kv` |
| `wit/host/http.wit` | `prx:host/http-outbound` |
| `wit/host/memory.wit` | `prx:host/memory` |
| `wit/host/event.wit` | `prx:host/events` |

Worlds: `wit/worlds.wit`. Plugin exports: `wit/plugin/*.wit` (documented in
[plugin-developer-guide.md](plugin-developer-guide.md)).
