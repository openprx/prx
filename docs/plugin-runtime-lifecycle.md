# Plugin Runtime Lifecycle

PRX owns exactly one `PluginRuntime` per canonical workspace in a process. Gateway and Channels obtain the same runtime instead of building independent registries, event buses, and adapters.

## Generation boundary

A plugin generation contains the compiled registry plus tool, middleware, hook,
cron, provider, and storage adapters derived from it. Every long-lived consumer
uses a stable proxy that resolves the current generation at execution time, so
install, update, enable, disable, and reload never leave an IM channel or CLI
holding a stale provider/storage instance.

WASM providers and storage backends are selected explicitly as
`wasm:<exported-name>` in `default_provider`, model routes, or
`memory.backend`. The explicit prefix prevents a plugin from shadowing a
built-in backend. TUI chat, the interactive and one-shot agent CLIs, gateway,
standalone channels, and session workers all initialize the workspace plugin
runtime before resolving an explicitly selected WASM backend.

The four middleware stages use JSON envelopes and run in the shared agent tool
loop: `inbound` once per admitted turn, `llm_request` once per provider call,
`llm_response` once per provider response, and `outbound` once per delivered
turn — after the agent loop has accepted the final response, before delivery
and history insertion. Consequently the same transformation path covers TUI,
CLI, gateway, and every IM channel. Invalid JSON or structurally invalid
message replacements are isolated and the original envelope is preserved.

`outbound` is invoked on the terminal text only after every retry, repair and
marker guard has accepted the response, and immediately before that text is
streamed, stored in history and returned. Its invocation count is zero for
tool-call iterations (which keep their pre-outbound assistant representation in
history), zero for a response that a corrective retry then replaces, and zero
for a silent turn that delivers nothing. Its envelope is
`{ channel, text, has_tool_calls }`; `has_tool_calls` is necessarily `false` at
that single call site and is kept only so existing plugins that read the field
still parse the payload.

A turn that fails before its terminal response is accepted never reaches the
stage. A turn cancelled while the accepted reply is being streamed has already
run it: the stage runs before the first chunk leaves the loop, so the plugin
executed once for a turn the caller is told failed. A plugin whose `outbound`
path has side effects must treat an execution as "a reply was accepted", not as
"a reply was delivered in full".

On entrypoints that stream provider tokens live — chat and TUI provider
streaming — the user sees the provider's own text as it arrives, before
`outbound` runs on that turn. The reply stored in history and returned to the
caller is the post-outbound text, and so are the chunks the tool loop itself
emits; only the live provider deltas of those entrypoints are pre-outbound.

Plugin authors should note the change: outbound was previously invoked once per
loop iteration instead of once per accepted delivered turn, so a turn with N
tool rounds executed the stage N+1 times and each corrective retry added one
more. Middleware whose `outbound` path has side effects — counters, audit
records, rate budgets, billing, notifications, external writes — ran repeatedly
for one delivered reply and should be re-checked against the exactly-once
contract.

## Middleware trap recovery and quarantine

Wasmtime poisons a whole `Store` when anything unwinds out of wasm, and a
poisoned store refuses every later entry with `cannot enter component instance`.
Re-instantiating the component into that store does not clear the flag, so a
middleware adapter that traps is rebuilt on a brand-new `Store`.

The contract:

- Every guest call runs on a task of its own, detached from the caller that
  asked for it. A caller that goes away — an aborted turn, a disconnected
  gateway client, a caller-side deadline shorter than the plugin's — stops
  waiting, but the guest call runs to its own outcome under the store lock, so
  a caller can neither poison the store nor be charged for it, and a guest that
  hangs is charged by its own host timeout, not excused by the caller's.
- Two poisoning outcomes mark the adapter dead: any trap returned by the guest
  call, and the host-side timeout. Both resume or end the guest fiber with an
  error that unwinds out of wasm, and the transition is recorded while the
  store mutex is still held. If the call's own task is dropped mid-call — which
  only a runtime shutdown does — the adapter is marked dead the same way before
  the lock is released.
- The next call rebuilds the adapter — new store, manifest limits, fuel, epoch
  deadline, fresh instantiation — from the engine and precompiled component the
  generation already holds. It is an instantiation, not a recompilation.
- A dropped call task is not the plugin's fault, so the rebuild it forces is
  not charged to the restart budget; it is counted as `cancelled_count`, and a
  rebuild abandoned mid-instantiation returns its charge. Aborted turns
  therefore cannot quarantine a healthy plugin, however many there are in a
  row: they do not even reach the guest's store.
- Charged rebuild *attempts* are budgeted at three per rolling window of
  `max(60 s, 4 x max_execution_time_ms)`, per adapter, and an attempt
  that cannot instantiate is charged exactly like one that can: the adapter
  stays dead, the call passes through, and the next call retries. When the
  budget is spent the adapter transitions once to quarantined and stops
  entering the guest. The window is rolling, not tumbling — attempts age out
  one at a time, so no pair of adjacent windows can admit twice the budget.
- The window scales with the timeout because a hung call holds the store for
  the whole `max_execution_time_ms` before it is marked dead, so charged
  rebuilds of a hanging guest are at least one timeout apart; a fixed minute
  would never see three of them at the 30 s default. With the scaled window a
  guest that hangs on every call, or hangs three times and then answers once,
  is quarantined at the same call a fast trap loop is — **but only while it is
  called back to back**: the fourth charged rebuild is refused only if the idle
  time between calls stays under about a third of the timeout (10 s at the
  30 s default; this holds for timeouts of 15 s or more, and shorter timeouts
  get a 60 s window, which tolerates more idle). Faults spaced further apart than the window — an hourly job
  whose guest traps through a long upstream outage — never share one and are
  retried for as long as they last (faults more than about 4/3 of the timeout
  apart, at least 20 s), so the adapter is running again when the outage ends.
- Independently of the window, **four timeouts in a row** quarantine the
  adapter at the fourth, however far apart they are. Both forms of timeout
  count: the host timer, which stops a guest waiting in a host call, and the
  epoch deadline, which interrupts a guest computing in wasm at the same
  `max_execution_time_ms`. Guest calls are detached
  from their callers, so a timeout is always the guest's own hang, never a
  caller giving up. Only timeouts count: any other trap does not add to the streak or
  end it, and any call the guest returns from — a result or a guest-returned
  error — ends it. This catches a guest that hangs on every call at any pace,
  such as an agent that retries a hanging tool after each LLM round trip; a
  guest that hangs three times and then answers once at such a pace is not
  quarantined, and each hung call costs up to `max_execution_time_ms`. The
  trade-off is deliberate: an upstream that makes a guest hang on four calls in
  a row — a black-holed endpoint through an outage, even an hourly job's —
  quarantines the adapter until the next generation (`refresh` or `reload`).
- Quarantine ends with the next generation: any `reload`, `refresh`, install,
  enable or disable builds new adapters, which start live.
- The state, the budget and every transition are read and written inside the one
  mutex that guards the store, so a call queued behind a trapping call observes
  the transition when it acquires the lock and is never admitted into a poisoned
  store.
- A payload larger than the adapter's cap (`max_payload_bytes`, otherwise an
  eighth of `max_memory_mb` with a 64 KiB floor) is skipped before the lock is
  taken and passes through unchanged, so a payload-driven trap cannot start a
  rebuild loop. Manifest validation rejects `max_payload_bytes = 0`, which
  would skip every call for the adapter's life, and clamps a cap above the
  guest's own linear memory down to it with a warning, because such a cap
  re-arms the very trap loop the guard exists to close.
- Every stage still fails open, for trusted and untrusted plugins alike: a
  failed, rebuilding, quarantined or skipped adapter leaves the envelope
  unchanged. Transitions are logged once each with `plugin`, `capability`
  (`middleware`), `operation` (the stage), `reason` and `restart_count`; pass-throughs are logged at most three times between
  transitions, with the suppressed total reported on the next transition. The
  quarantine transition also publishes one `prx.plugin.middleware_quarantined`
  event on the plugin event bus.

`middleware_adapters` in `wasm_plugins_status` therefore carries `state`
(`live`, `restarting` or `quarantined`), `restart_count`, `failure_count`,
`cancelled_count`, `skipped_oversize`, `skipped_quarantined`, `consecutive_timeouts` (timeouts
since the guest last returned), `last_error`, `last_error_at` and, while quarantined,
`quarantined_for_ms` and `quarantine_rule` (`restart_window` or `timeout_streak`: which rule
fired), alongside `plugin` and `priority`.
`last_error` can carry text the guest itself authored, so it is truncated to
512 characters when it is recorded, followed by a `" ... (truncated)"` marker (quotes delimit the string and are not appended). The
`wasm_plugins_status` capability reports `Ready` instead of `Healthy` while any
middleware adapter is quarantined.

A rebuilt instance starts with empty guest linear memory. Host-side KV survives
a rebuild because it lives in host state; counters a guest kept in its own
memory do not.

## Recovery for the other WASM capability kinds

Every adapter logs its transitions with the same fields: `plugin`,
`capability` (`middleware`, `tool`, `hook`, `cron`, `provider`, `storage`),
`operation` (the middleware stage, or the export such as `execute`,
`on-event`, `run`, `chat`, `store-memory`), `reason` and `restart_count`.
Middleware, hook and cron callers wait for the store without a bound, as
before: each caller ahead of them in the queue can hold it for up to one
rebuild and one guest call, about twice `max_execution_time_ms`. Because calls
are detached, a caller that gives up returns at once, but its call keeps the
store until it completes or times out, so callers queued behind it still wait
for it.

Tool, hook, cron, provider and storage adapters share the middleware state
machine above — fresh-store rebuild, three charged rebuild attempts per rolling
window of `max(60 s, 4 x max_execution_time_ms)`, uncharged rebuilds after a caller cancellation, quarantine until the
next generation, host KV carried across rebuilds, fuel re-armed before every
call, the payload cap checked before the store lock — and the same status
vocabulary. What differs per kind is what a failure means to the caller:

- **Tool** (`execute`): every failure is a failed `ToolResult`, never a
  success the guest did not produce. A trap or timeout returns a tool error; a
  quarantined tool stays listed and answers every call with a tool error naming
  the quarantine; arguments over the payload cap are refused with a tool error
  before the guest is entered. The host timeout is enforced inside the store
  lock, so a guest that runs past `max_execution_time_ms` is charged like a
  trap; waiting for the store behind other calls is bounded by the same value
  separately and is not charged. The worst case for one call is therefore
  about three times `max_execution_time_ms`: up to one timeout waiting for the
  store, up to one for a rebuild (the rebuilt instance starts under the same
  epoch deadline), and up to one for the guest call itself. The same bound
  holds for provider and storage calls. `tool_adapters` in `wasm_plugins_status`
  carries `plugin` and `tool` plus the health fields, and quarantine publishes
  `prx.plugin.tool_quarantined`.
- **Hook** (`on-event`, for lifecycle events and for the guest's own event-bus
  subscriptions alike): hooks are observe-only — nothing consults their result
  and no WASM hook can veto an action — so every failure fails open. A failing,
  rebuilding or quarantined hook does not observe the event; a payload over the
  cap is skipped and counted. Subscriptions a guest registered survive a
  rebuild, because every rebuilt store delivers into the same sink, and
  subscribing is idempotent per plugin, pattern and sink: a guest that
  subscribes again after a rebuild (it lost the memory of having subscribed)
  gets the existing subscription back rather than a duplicate delivery.
  `hook_adapters` keeps `plugin`, `event_patterns`, `invocation_count` and
  `last_event` and gains the health fields; `last_error` now keeps the most
  recent failure (with `last_error_at`) instead of being cleared by the next
  successful delivery. Quarantine publishes `prx.plugin.hook_quarantined`.
- **Cron** (`run`): a run that cannot complete — a trap, a timeout, a
  quarantine refusal — returns an error and never reports output it did not
  produce. The occurrence is not retried; the job next runs at its next
  scheduled time, as before. A quarantined job is skipped on every occurrence
  and counted in `skipped_quarantined`; the scheduler no longer warns once per
  failed occurrence. A rebuilt job starts with empty guest memory, so a counter
  kept there (as in the `counter-cron` fixture) starts over. `cron_adapters`
  keeps `plugin` and `schedule` and gains the health fields; quarantine
  publishes `prx.plugin.cron_quarantined`.
- **Provider** (`chat`): every failure — a trap, a timeout, a quarantine
  refusal, a request over the payload cap — is returned as an error for the
  caller's retry and fallback layer to act on. The adapter never fabricates a
  response and never truncates a request to make it fit; the request's role
  and content strings plus the model name are what the cap is measured
  against. The timeout and lock-wait rules are the tool's. `provider_adapters`
  is unchanged (the exported names); the new `provider_adapter_status` carries
  `plugin` and `provider` plus the health fields per adapter, and quarantine
  publishes `prx.plugin.provider_quarantined`. A `wasm:` provider used as the
  primary is not wrapped in the reliability layer, so its errors reach the
  agent directly; as a fallback entry it is retried like any other provider,
  and each retry of a trapping request spends a restart.
- **Storage** (`store`, `forget`, `recall`/`get`/`list`, `count`,
  `health_check`): every failure is an error. A write is never acknowledged
  unless the guest completed it, a failed write is not retried, and a read never
  turns a failure into an empty answer; `health_check` reports `false`. A
  quarantined backend refuses every operation. As for every kind, each storage
  operation runs on a task of its own: a caller that goes away — an aborted turn, another
  plugin's host call timed out while waiting on this memory backend, a
  caller-side deadline — does not cancel it, so it can neither poison the store
  mid-call (and wipe a guest-memory backend through the rebuild that follows)
  nor lose a write it had already handed over. The operation completes, or
  reaches its own host timeout, under the store lock as if its caller had
  waited. A rebuilt guest starts with
  empty linear memory, so a backend that keeps its entries there — the
  `map-storage` fixture does — comes back **empty**; the trap had already made
  those entries unreachable. The reset is reported in band: the first
  `recall`/`get`/`list`/`count` made by the call that rebuilt the backend
  returns an error saying the store was rebuilt and its guest-memory entries
  are gone, instead of answering "no memories" from the empty guest; the next
  read answers from the rebuilt store. A write made by that call succeeds
  normally. Every storage rebuild is also logged at `warn` with the storage
  name, the plugin and the operation, and published on
  `prx.plugin.storage_restarted` (`plugin`, `capability`, `reason`,
  `restart_count`) for operators and any subscriber; nothing in PRX itself
  subscribes to that event. Durable backends keep their data outside
  the guest (host KV survives a rebuild; an external service is untouched).
  `storage_adapters` is unchanged (the exported names); the new
  `storage_adapter_status` carries `plugin` and `storage` plus the health
  fields, and quarantine publishes `prx.plugin.storage_quarantined`.

`wasm_plugins_manage` also exposes direct component probes for operational
verification: `middleware_test`, `cron_run`, `provider_chat`, and storage
health/store/recall/forget/count actions. These probes use the same current
generation as production traffic; they are not separate mock adapters.

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
