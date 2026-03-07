# @prx/pdk — JavaScript/TypeScript PDK

PRX WASM Plugin Development Kit for JavaScript and TypeScript.

Build PRX plugins with TypeScript ≥ 5.0, targeting the WASM Component Model
via [`jco`](https://github.com/bytecodealliance/jco) and
[`componentize-js`](https://github.com/bytecodealliance/ComponentizeJS).

---

## Prerequisites

| Tool | Version | Purpose |
|------|---------|---------|
| Node.js | ≥ 20 | Runtime (build-time only) |
| TypeScript | ≥ 5.0 | Type-checked source |
| `@bytecodealliance/jco` | ≥ 1.6 | Transpile + componentize JS → WASM |
| `@bytecodealliance/componentize-js` | ≥ 0.12 | JS → WASM component pipeline |

Install the build tools once:

```sh
npm install --save-dev @bytecodealliance/jco @bytecodealliance/componentize-js typescript
```

---

## Quick Start — Tool Plugin

### 1. Create the plugin directory

```sh
mkdir my-tool && cd my-tool
cp -r /path/to/prx/pdk/javascript/templates/tool/. .
# Rename .tmpl files and fill in {{PLACEHOLDERS}}
mv src/plugin.ts.tmpl src/plugin.ts
mv package.json.tmpl package.json
mv tsconfig.json.tmpl tsconfig.json
mv plugin.toml.tmpl plugin.toml
```

### 2. Install dependencies

```sh
npm install
```

### 3. Implement your tool

Edit `src/plugin.ts`.  The only two exports required by the `tool` WIT world are:

```typescript
import { log, resultOk, resultErr } from "@prx/pdk";
import type { ToolSpec, PluginResult } from "@prx/pdk";

export function getSpec(): ToolSpec {
  return {
    name: "my_tool",
    description: "Does something useful",
    parametersSchema: JSON.stringify({
      type: "object",
      properties: {
        input: { type: "string" }
      },
      required: ["input"]
    }),
  };
}

export function execute(argsJson: string): PluginResult {
  const args = JSON.parse(argsJson) as { input: string };
  log.info(`Processing: ${args.input}`);
  return resultOk(`Result: ${args.input.toUpperCase()}`);
}
```

### 4. Build the WASM component

```sh
npm run build:wasm
# → plugin.wasm
```

### 5. Deploy

Copy `plugin.wasm` + `plugin.toml` to your PRX plugins directory:

```sh
cp plugin.wasm plugin.toml /path/to/prx/plugins/my-tool/
```

---

## Build Pipeline

```
src/plugin.ts
    │
    ▼  npx tsc
dist/plugin.js
    │
    ▼  npx jco componentize dist/plugin.js --wit <wit-path> --world <world>
plugin.wasm          ← WASM component ready for PRX
```

### World choices

| Plugin type | `--world` flag |
|-------------|---------------|
| Tool        | `tool`        |
| Middleware  | `middleware`  |
| Hook        | `hook`        |
| Cron job    | `cron`        |

---

## TypeScript API Reference

### Types (`@prx/pdk`)

#### `ToolSpec`
```typescript
interface ToolSpec {
  name: string;              // snake_case tool name
  description: string;       // shown to the LLM
  parametersSchema: string;  // JSON Schema string
}
```

#### `PluginResult`
```typescript
interface PluginResult {
  success: boolean;
  output: string;
  error?: string;
}
```

#### `MiddlewareAction`
```typescript
type MiddlewareAction =
  | { action: "continue"; data: string }   // pass (modified) data downstream
  | { action: "block";    reason: string }; // halt pipeline
```

#### `HttpResponse`
```typescript
interface HttpResponse {
  status: number;
  headers: [string, string][];
  body: Uint8Array;
}
```

#### `MemoryEntry`
```typescript
interface MemoryEntry {
  id: string;
  text: string;
  category: string;
  importance: number;  // 0.0–1.0
}
```

#### `CronContext`
```typescript
interface CronContext {
  firedAt: string;   // ISO 8601 timestamp
  cronExpr?: string; // cron expression from manifest
}
```

---

### Helper functions

```typescript
import { resultOk, resultErr, middlewareContinue, middlewareBlock } from "@prx/pdk";

resultOk("output text")        // → PluginResult { success: true, output: "..." }
resultErr("error message")     // → PluginResult { success: false, error: "..." }
middlewareContinue(dataJson)   // → MiddlewareAction { action: "continue", ... }
middlewareBlock("reason")      // → MiddlewareAction { action: "block", ... }
```

---

### Host modules

All modules are available as named exports from `@prx/pdk`.
Outside a WASM component (e.g. in unit tests), they fall back to harmless stubs.

#### `log`

```typescript
import { log } from "@prx/pdk";

log.trace("message");  // TRACE level
log.debug("message");  // DEBUG level
log.info("message");   // INFO level  ← most common
log.warn("message");   // WARN level
log.error("message");  // ERROR level
```

Requires `"log"` permission in `plugin.toml` (always granted by default).

---

#### `config`

Read-only access to values from `plugin.toml [config]`.

```typescript
import { config } from "@prx/pdk";

const value = config.get("key");             // string | undefined
const all   = config.getAll();               // [string, string][]
const safe  = config.getOr("key", "default"); // string (never undefined)
```

Requires `"config"` permission.

---

#### `kv`

Isolated per-plugin persistent key-value store.

```typescript
import { kv } from "@prx/pdk";

kv.set("key", new Uint8Array([1, 2, 3]));
kv.setString("greeting", "hello");
kv.setJson("user", { id: 42 });

const bytes  = kv.get("key");           // Uint8Array | undefined
const str    = kv.getString("greeting"); // string | undefined
const obj    = kv.getJson<User>("user"); // User | undefined

kv.delete("key");                        // boolean
kv.listKeys("prefix:");                  // string[]
kv.increment("counter", 1);             // number (new value)
```

Requires `"kv"` permission.

---

#### `events`

Fire-and-forget publish/subscribe event bus.

```typescript
import { events } from "@prx/pdk";

// Publish
events.publish("weather.update", JSON.stringify({ city: "NYC", temp: 25 }));
events.publishJson("weather.update", { city: "NYC", temp: 25 });

// Subscribe (returns subscription ID as bigint)
const subId = events.subscribe("weather.*");

// Unsubscribe
events.unsubscribe(subId);
```

Requires `"events"` permission. Payload max 64 KB.

---

#### `http`

Outbound HTTP requests.

```typescript
import { http } from "@prx/pdk";

// GET
const resp = http.get("https://api.example.com/data");
const text = http.bodyText(resp);
const json = http.bodyJson<MyType>(resp);

// POST JSON
const resp2 = http.postJson("https://api.example.com/submit", { key: "value" });

// Generic request
const resp3 = http.request("PUT", "https://...", [["X-Custom", "value"]], body);
```

Requires `"http-outbound"` permission. URLs must match `http_allowlist` in `plugin.toml`.

---

#### `clock`

Current time utilities.

```typescript
import { clock } from "@prx/pdk";

const ms  = clock.nowMs();   // number — Unix milliseconds
const iso = clock.nowIso();  // string — ISO 8601 (e.g. "2025-01-01T00:00:00.000Z")
```

No permission required.

---

#### `memory`

Long-term memory store.

```typescript
import { memory } from "@prx/pdk";

const id      = memory.store("Paris is the capital of France", "fact");
const entries = memory.recall("capital of France", 5); // MemoryEntry[]
```

Requires `"memory"` permission.

---

## Examples

| Example | Type | Description |
|---------|------|-------------|
| [`examples/markdown-tool/`](./examples/markdown-tool/) | Tool | Converts Markdown to HTML (pure TS, no deps) |
| [`examples/rate-limiter-middleware/`](./examples/rate-limiter-middleware/) | Middleware | Per-user sliding-window rate limiting |

---

## Plugin Manifest Reference (`plugin.toml`)

```toml
[plugin]
name = "my-tool"          # kebab-case identifier
version = "0.1.0"
description = "..."
author = "Your Name"
wasm = "plugin.wasm"      # path to the compiled WASM file

[[capabilities]]
type = "tool"             # tool | middleware | hook | cron
name = "my_tool"          # snake_case capability name

[permissions]
required = ["log", "kv"]  # capabilities the plugin must have
optional = ["http-outbound", "memory", "events"]

[resources]
max_fuel = 100_000_000    # compute budget (wasmtime fuel)
max_memory_mb = 16        # memory limit
max_execution_time_ms = 5000

[config]
# Static configuration injected at deploy time
# api_key = "..."
```

### Permission reference

| Permission | Host interface | Notes |
|------------|---------------|-------|
| `log` | `prx:host/log` | Always granted |
| `config` | `prx:host/config` | Read-only plugin config |
| `kv` | `prx:host/kv` | Isolated per-plugin KV |
| `events` | `prx:host/events` | Event bus pub/sub |
| `http-outbound` | `prx:host/http-outbound` | Outbound HTTP (allowlist required) |
| `memory` | `prx:host/memory` | PRX memory store |

---

## Constraints

- TypeScript ≥ 5.0, Node.js ≥ 20 (build-time only)
- Do **not** use Node.js-specific APIs (`fs`, `net`, `path`, etc.) — they are unavailable in the WASM sandbox
- Do **not** import native Node.js modules — the jco componentize sandbox does not expose them
- `Date`, `Math`, `JSON`, `TextEncoder`/`TextDecoder` and most Web APIs are available
- Third-party npm packages must be pure JS/TS with no native bindings

---

## Troubleshooting

**`jco componentize` fails with "unknown import"**

Ensure the `--wit` path points to the `wit/` directory at the root of the PRX repository
and that all WIT files are present.  The `--world` flag must match one of the worlds
defined in `wit/worlds.wit`.

**Type errors in `@prx/pdk` imports**

Run `npm run build` inside `packages/prx-pdk/` to generate the `dist/` typings first,
then reinstall in your plugin directory:

```sh
cd packages/prx-pdk && npm run build
cd ../../examples/my-plugin && npm install
```

**WASM component too large**

`componentize-js` bundles the SpiderMonkey JS engine (~5–10 MB).  This is expected.
The PRX host pre-compiles and caches WASM modules, so cold-start overhead is a
one-time cost.
