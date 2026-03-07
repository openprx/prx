# prx-pdk — Python Plugin Development Kit

Build PRX WASM plugins with Python 3.10+.

## Requirements

| Tool | Version | Purpose |
|------|---------|---------|
| Python | ≥ 3.10 | Plugin authoring |
| componentize-py | ≥ 0.16 | Compile Python → WASM component |
| pip / uv | any | Install `prx-pdk` |

## Install

```bash
pip install componentize-py>=0.16
pip install -e /path/to/prx/pdk/python   # local install
```

## Quick Start

### 1. Tool Plugin

Create `plugin.py`:

```python
from prx_pdk import prx_tool, ToolResult, host
import json

@prx_tool(
    name="json_formatter",
    description="Format JSON with configurable indentation",
    params={
        "type": "object",
        "properties": {
            "json_str": {"type": "string"},
            "indent":   {"type": "integer", "default": 2},
        },
        "required": ["json_str"],
    },
)
def execute(args: dict) -> ToolResult:
    host.log.info("json_formatter called")
    data = json.loads(args["json_str"])
    return ToolResult.ok(json.dumps(data, indent=args.get("indent", 2)))
```

Create `plugin.toml`:

```toml
[plugin]
name        = "json-formatter"
version     = "0.1.0"
description = "Format JSON with indentation"
capability  = "tool"

[permissions]
required = []
```

Build:

```bash
componentize-py \
    --wit-path /path/to/prx/wit \
    --world tool \
    componentize plugin.py \
    -o plugin.wasm
```

### 2. Hook Plugin

```python
from prx_pdk import prx_hook, host

@prx_hook(events=["tool_call", "agent_start"])
def on_event(event: str, payload: dict) -> None:
    host.log.info(f"Event received: {event}")
    host.kv.increment(f"event_count:{event}")
```

Build:

```bash
componentize-py \
    --wit-path /path/to/prx/wit \
    --world hook \
    componentize plugin.py \
    -o plugin.wasm
```

### 3. Middleware Plugin

```python
import json
from prx_pdk import prx_middleware, MiddlewareAction, host

@prx_middleware(priority=10)
def process(stage: str, data: dict) -> MiddlewareAction:
    if stage == "inbound":
        host.log.info("Enriching inbound message")
        data["enriched"] = True
    return MiddlewareAction.continue_(json.dumps(data))
```

### 4. Cron Plugin

```python
from prx_pdk import prx_cron, host

@prx_cron
def run() -> str:
    host.log.info("Cron job executing")
    count = host.kv.increment("run_count")
    return f"Run #{count} completed"
```

## Host API Reference

All host functions are available via `from prx_pdk import host`.

### `host.log`

```python
host.log.info("message")
host.log.warn("warning")
host.log.error("error")
host.log.debug("debug")
host.log.trace("trace")
```

### `host.config`

Read-only configuration from `plugin.toml [config]`.

```python
value = host.config.get("key")          # Optional[str]
value = host.config.get_or("key", "default")
pairs = host.config.get_all()           # list[tuple[str, str]]
```

### `host.kv`

Persistent per-plugin key-value store. Values are `bytes`.

```python
host.kv.set("key", b"bytes")
host.kv.set_str("key", "text")
host.kv.set_json("key", {"x": 1})

data: bytes | None = host.kv.get("key")
text: str | None   = host.kv.get_str("key")
obj                = host.kv.get_json("key")

host.kv.delete("key")
keys = host.kv.list_keys("prefix:")
count = host.kv.increment("counter", delta=1)
```

Requires `"kv"` permission.

### `host.events`

Fire-and-forget inter-plugin event bus. Payload must be valid JSON, max 64 KB.

```python
host.events.publish("my.topic", '{"key":"value"}')
host.events.publish_json("my.topic", {"key": "value"})

sub_id = host.events.subscribe("weather.*")
host.events.unsubscribe(sub_id)
```

Requires `"events"` permission.

### `host.http`

Outbound HTTP requests. URLs must be in `plugin.toml [http_allowlist]`.

```python
resp = host.http.get("https://api.example.com/data")
print(resp.status)     # int
print(resp.text())     # str
print(resp.json())     # any

resp = host.http.post_json("https://api.example.com/post", {"key": "val"})
resp = host.http.request("PUT", url, headers=[("X-Token", "abc")], body=b"...")
```

Requires `"http-outbound"` permission.

### `host.clock`

```python
ms  = host.clock.now_ms()   # int — Unix milliseconds
tz  = host.clock.timezone() # str — always "UTC" currently
```

### `host.memory`

Long-term semantic memory (requires `"memory"` permission).

```python
entry_id = host.memory.store("Important fact", category="fact")

entries = host.memory.recall("search query", limit=5)
for e in entries:
    print(e.id, e.text, e.category, e.importance)
```

## Local Development

The PDK detects whether it is running inside a WASM component by checking
`sys.platform == "wasi"`.  Outside WASM, all host functions fall back to
safe stubs so you can unit-test plugin logic without a WASM runtime:

```python
# tests/test_plugin.py
import pytest
from plugin import execute   # imports from your plugin.py directly

def test_formats_json():
    result = execute({"json_str": '{"a":1}', "indent": 4})
    assert result["success"]
    assert '"a": 1' in result["output"]
```

Run tests:

```bash
cd examples/hello-tool
pytest ../../tests/
```

### KV stub

The KV store uses an in-memory `dict` in local mode — changes are **not**
persisted across test runs.

### HTTP stub

`host.http.request(...)` raises `RuntimeError` in local mode.  Mock it in
tests:

```python
from unittest.mock import patch
from prx_pdk.types import HttpResponse

with patch("prx_pdk.host._BINDINGS_AVAILABLE", False):
    with patch("prx_pdk.host._Http.request") as mock_req:
        mock_req.return_value = HttpResponse(status=200, body=b'{"ok":true}')
        result = execute({"url": "https://example.com"})
```

## Build Reference

### Supported `--world` values

| World | Capability | Decorator |
|-------|-----------|-----------|
| `tool` | LLM-callable tool | `@prx_tool` |
| `hook` | Lifecycle observer | `@prx_hook` |
| `middleware` | Pipeline transformer | `@prx_middleware` |
| `cron` | Scheduled task | `@prx_cron` |

### componentize-py version compatibility

| componentize-py | wasmtime |
|----------------|---------|
| ≥ 0.16 | 31.x |

### Binary size

Python WASM components are typically **8–15 MB** because they embed the
Python interpreter.  This is expected — PRX supports pre-compilation caching
to amortise the cold-start cost.

## Templates

Use the template in `templates/tool/` as a starting point:

```bash
cp pdk/python/templates/tool/plugin.py.tmpl my-plugin/plugin.py
cp pdk/python/templates/tool/plugin.toml.tmpl my-plugin/plugin.toml
# Edit the {{placeholders}} and implement your logic.
```

## Examples

| Example | Capability | Description |
|---------|-----------|-------------|
| `examples/hello-tool/` | Tool | JSON formatter |
| `examples/logger-hook/` | Hook | Event logger with KV counters |
