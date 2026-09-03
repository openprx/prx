# Plugin Runtime Lifecycle

PRX owns exactly one `PluginRuntime` per canonical workspace in a process. Gateway and Channels obtain the same runtime instead of building independent registries, event buses, and adapters.

## Generation boundary

A plugin generation contains the compiled registry plus the tool, middleware, hook, and cron adapters derived from it. Tools are exposed through one stable multi-spec router; hook dispatch and cron scheduling resolve the current generation at execution time.

`POST /api/plugins/{name}/reload` performs these steps:

1. Serialize reloads for the workspace.
2. Build a complete candidate manager and every derived adapter without changing the active generation.
3. Verify that the requested plugin is present in the candidate.
4. Publish the candidate with one atomic generation swap.

Any parse, compile, or target-presence failure leaves the previous generation active. The API reports the published generation number on success. The event bus remains stable across generations, while old subscriber pumps close and unregister when their adapter generation is released.

## Event delivery

EventBus queues are bounded. Publishing never waits for a consumer; when a subscriber queue is full, that event is dropped with a warning. Host `subscribe` is accepted only for hook-capable instances that own an `on-event` export. A real pump continuously consumes the subscription receiver and delivers `(topic, payload)` to that guest export. Other plugin capabilities receive an explicit unsupported error.

Native lifecycle names are bridged into the documented `prx.lifecycle.*`
namespace before WASM dispatch (`agent_end` maps to
`prx.lifecycle.agent_stop`). Hook manifest events accept string arrays and the
documented `[[capabilities.events]]` table form, with exact, prefix-wildcard,
global-wildcard, and legacy short-name matching. Status reports per-adapter
invocation counters and the last event/error for end-to-end verification.

Middleware, hook, and cron adapters link the same canonical PDK host ABI as
tool plugins, including result-bearing KV mutations plus HTTP, memory, events,
WebSocket, and WASI imports. Their host state receives the live memory backend
when one is configured.

## Admission and trust

- `plugin.toml` is limited to 256 KiB and cannot be a symlink.
- A WASM component is limited to 128 MiB, cannot be a symlink, and its manifest path cannot escape the plugin directory.
- Directory discovery is deterministic and capped at 256 entries.
- A plugin missing any required sensitive permission is registered with an error status and does not receive a live adapter.

## Native hooks

`hooks.json` reload is content-addressed: parsing and validation finish before one state swap, and an invalid candidate preserves the prior generation. Hook payloads, configuration, stderr, action counts, arguments, environment entries, and timeouts are bounded. Timeout kills and reaps the child, and the restrictive payload tempfile is removed through RAII.
