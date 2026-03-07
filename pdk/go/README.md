# PRX Go PDK

Go / TinyGo Plugin Development Kit for PRX WASM plugins.

Module: `github.com/openprx/prx-pdk-go`

---

## Requirements

| Tool | Version | Notes |
|------|---------|-------|
| Go | ≥ 1.22 | For host-side testing with stubs |
| TinyGo | ≥ 0.34 | Required for WASM compilation |

### Install TinyGo

```bash
# macOS
brew install tinygo

# Linux (download release)
wget https://github.com/tinygo-org/tinygo/releases/download/v0.34.0/tinygo0.34.0.linux-amd64.tar.gz
tar xzf tinygo0.34.0.linux-amd64.tar.gz
export PATH=$PATH:$(pwd)/tinygo/bin
```

Verify:
```bash
tinygo version
# tinygo version 0.34.0 ...
```

---

## Build a Plugin

```bash
cd my-plugin/
tinygo build \
  -target wasm32-wasip2 \
  -scheduler none \
  -no-debug \
  -opt 2 \
  -o plugin.wasm \
  .
```

| Flag | Purpose |
|------|---------|
| `-target wasm32-wasip2` | WASI Preview 2 + Component Model |
| `-scheduler none` | No goroutines (required for WASM) |
| `-no-debug` | Strip DWARF to reduce binary size |
| `-opt 2` | Optimise for size and speed |

---

## Quick Start

1. **Initialise module**

```bash
mkdir my-hash-tool && cd my-hash-tool
go mod init github.com/myorg/my-hash-tool
go get github.com/openprx/prx-pdk-go@latest
```

2. **Write the plugin** (`main.go`)

```go
package main

import (
    "crypto/sha256"
    "encoding/hex"
    "unsafe"

    "github.com/openprx/prx-pdk-go/host/log"
    "github.com/openprx/prx-pdk-go/host/config"
)

//go:wasmexport get-spec
func getSpec() (ptr *uint8, length uint32) {
    log.Debug("get-spec called")
    spec := `{"name":"hash","description":"SHA-256 hash","parameters_schema":"{}"}`
    b := []byte(spec)
    return &b[0], uint32(len(b))
}

//go:wasmexport execute
func execute(inputPtr *uint8, inputLen uint32) (outPtr *uint8, outLen uint32) {
    input := string(unsafe.Slice(inputPtr, inputLen))
    _ = config.GetOr("prefix", "")

    h := sha256.New()
    h.Write([]byte(input))
    hash := hex.EncodeToString(h.Sum(nil))
    log.Info("computed: " + hash[:8])

    result := `{"success":true,"output":"` + hash + `","error":null}`
    b := []byte(result)
    return &b[0], uint32(len(b))
}

func main() {}
```

3. **Add plugin.toml**

```toml
[plugin]
name    = "my-hash-tool"
version = "0.1.0"
wasm    = "plugin.wasm"

[[capabilities]]
type = "tool"
name = "hash"

[permissions]
required = ["log"]
```

4. **Build**

```bash
tinygo build -target wasm32-wasip2 -scheduler none -o plugin.wasm .
```

---

## Package Reference

### `host/log` — Structured Logging

```go
import "github.com/openprx/prx-pdk-go/host/log"

log.Trace("verbose detail")
log.Debug("debug info")
log.Info("normal message")
log.Warn("something unexpected")
log.Error("something failed")
```

WIT: `prx:host/log@0.1.0`

---

### `host/config` — Plugin Configuration

```go
import "github.com/openprx/prx-pdk-go/host/config"

// Get a required value (returns "", false if missing).
val, ok := config.Get("api_key")

// Get all configured values.
pairs := config.GetAll()        // [][2]string

// Get with a default fallback.
timeout := config.GetOr("timeout_ms", "5000")
```

Config values are set in `plugin.toml [config]` and are read-only at runtime.

WIT: `prx:host/config@0.1.0`

---

### `host/kv` — Key-Value Storage

```go
import "github.com/openprx/prx-pdk-go/host/kv"

// Raw bytes.
_ = kv.Set("key", []byte("value"))
val, ok := kv.Get("key")

// Strings.
_ = kv.SetString("counter", "42")
s, ok := kv.GetString("counter")

// Pre-serialised JSON (no reflect).
jsonBytes := []byte(`{"count":1}`)
_ = kv.SetJSON("state", jsonBytes)
data, ok := kv.GetJSON("state")

// List and delete.
keys := kv.ListKeys("prefix:")
existed, err := kv.Delete("key")
```

Each plugin has an isolated namespace; plugins cannot access each other's keys.

WIT: `prx:host/kv@0.1.0`

---

### `host/events` — Event Bus

```go
import "github.com/openprx/prx-pdk-go/host/events"

// Publish (payload must be valid JSON, max 64 KB).
err := events.Publish("weather.update", `{"city":"Berlin","temp":22}`)

// Publish pre-serialised JSON.
err = events.PublishJSON("alerts", jsonPayload)

// Subscribe / unsubscribe.
id, err := events.Subscribe("weather.*")
err = events.Unsubscribe(id)
```

Requires `"events"` permission.

WIT: `prx:host/events@0.1.0`

---

### `host/http` — Outbound HTTP

```go
import "github.com/openprx/prx-pdk-go/host/http"

headers := [][2]string{{"Accept", "application/json"}}

resp, err := http.Get("https://api.example.com/data", headers)
resp, err = http.PostJSON("https://api.example.com/post", headers, []byte(`{"key":"val"}`))
resp, err = http.Request("PUT", url, headers, body)

fmt.Println(resp.Status, resp.BodyText())
```

Requires `"http-outbound"` permission. URLs must match `http_allowlist` in `plugin.toml`.

WIT: `prx:host/http-outbound@0.1.0`

---

### `host/clock` — Current Time

```go
import "github.com/openprx/prx-pdk-go/host/clock"

ms  := clock.NowMs()   // Unix milliseconds (uint64)
sec := clock.NowSec()  // Unix seconds (uint64)
tz  := clock.Timezone() // "UTC"
```

Implemented via TinyGo's WASI Preview 2 `time.Now()` — no custom host import required.

---

### `host/memory` — Long-Term Memory

```go
import "github.com/openprx/prx-pdk-go/host/memory"

// Store a memory entry. Returns the entry ID.
id, err := memory.Store("User prefers dark mode", "preference")

// Recall entries matching a query.
entries, err := memory.Recall("user preferences", 5)
for _, e := range entries {
    // e.ID, e.Text, e.Category, e.Importance
}
```

Requires `"memory"` permission.

WIT: `prx:host/memory@0.1.0`

---

### Types (`github.com/openprx/prx-pdk-go`)

| Type | Description |
|------|-------------|
| `ToolSpec` | Tool descriptor: Name, Description, ParametersSchema |
| `PluginResult` | Execution result: Success, Output, Error |
| `HttpResponse` | HTTP response: Status, Headers, Body |
| `MiddlewareResult` | Middleware decision: Action (Continue/Block), Data/Reason |
| `MemoryEntry` | Memory record: ID, Text, Category, Importance |
| `CronContext` | Cron invocation context: Schedule, AtMs |
| `KeyValue` | String key-value pair (config.GetAll) |

Convenience constructors:
```go
pdk.OK("success output")           // PluginResult
pdk.Fail("something went wrong")   // PluginResult
pdk.Continue(`{"data":"..."}`)     // MiddlewareResult
pdk.Block("rate limit exceeded")   // MiddlewareResult
```

---

## TinyGo Compatibility Notes

| Feature | Status |
|---------|--------|
| Goroutines | ❌ Not supported — use `-scheduler none` |
| `reflect` | ❌ Avoid — no `encoding/json` |
| `crypto/sha256` | ✅ Supported |
| `encoding/hex` | ✅ Supported |
| `time.Now()` | ✅ Supported (WASI clock) |
| `unsafe.Slice` | ✅ Supported (Go ≥ 1.17) |
| `fmt.Fprintf` | ✅ Supported (stub builds only) |
| Standard lib | Partial — see [TinyGo docs](https://tinygo.org/docs/reference/lang-support/stdlib/) |

### JSON Without Reflect

Since `encoding/json` requires reflect, encode/decode JSON manually:

```go
// Encode — build JSON strings by hand.
json := `{"name":"` + name + `","value":` + itoa(n) + `}`

// Decode — scan for field values.
func extractField(json, field string) string {
    needle := `"` + field + `":"`
    // ... linear scan ...
}
```

See `examples/hash-tool/main.go` for a complete reference implementation.

---

## Build Tag Isolation

The PDK uses Go build tags to separate WASM and stub implementations:

| Tag | File suffix | Environment |
|-----|-------------|-------------|
| `tinygo` | `_wasm.go` | TinyGo → WASM, uses `//go:wasmimport` |
| `!tinygo` | `_stub.go` | Standard Go → host testing |

This lets you run `go test ./...` on the host without a WASM runtime:

```bash
# Host-side unit tests (uses fmt.Println stubs).
go test ./...

# WASM build.
tinygo build -target wasm32-wasip2 -o plugin.wasm .
```

---

## Examples

| Example | Type | Description |
|---------|------|-------------|
| [`examples/hash-tool`](examples/hash-tool/) | Tool | SHA-256 hash computation |
| [`examples/event-forwarder-hook`](examples/event-forwarder-hook/) | Hook | Event forwarding with metadata |

---

## Template

Use `templates/tool/` as a starting point for new tool plugins:

```bash
cp -r pdk/go/templates/tool/ my-plugin/
cd my-plugin/
# Edit main.go.tmpl → main.go (fill in {{.PluginName}} etc.)
# Edit plugin.toml.tmpl → plugin.toml
tinygo build -target wasm32-wasip2 -o plugin.wasm .
```
