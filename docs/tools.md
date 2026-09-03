# Tools

Built-in and dynamically discovered tools are organized by category. The exact
provider-visible count is selected per turn from intent, configuration, runtime
health, MCP discovery, and the active WASM generation.

| Category | Tools |
|----------|-------|
| **Shell & Files** | `shell`, `file_read`, `file_write`, `file_edit`, `git_operations` |
| **Web** | `web_search_tool`, `web_fetch`, `http_request` |
| **Memory & Documents** | `memory_store`, `memory_recall`, `memory_search`, `memory_get`, `memory_forget`, `memory_reindex`, `document_search`, `document_get_chunk`, `document_ingest`, `document_sync` |
| **Skills** | `skills_list`, `skill_read`, `skills_manage`, `skill_execute`, plus up to 32 declared skill-tool aliases |
| **Messaging** | `message_send` |
| **Sessions** | `sessions_spawn`, `sessions_send`, `sessions_list`, `sessions_history`, `session_status`, `subagents`, `delegate` |
| **Scheduling** | `cron` (calendar/time scheduling), `xin` (autonomous task and durable Goal/Step workflows) |
| **Images** | `image`, `image_info` |
| **MCP** | `mcp_call`, `mcp_status`, plus up to 32 discovered aliases |
| **WASM Plugins** | `wasm_plugin_call`, `wasm_plugins_status`, `wasm_plugin_reload`, plus up to 32 aliases when compiled with `wasm-plugins` |
| **Remote Nodes** | `nodes` (control paired devices — camera, screen, location, run commands) |
| **Infrastructure** | `gateway`, `config_reload`, `proxy_config`, `agents_list` |
| **Integrations** | `composio` (1000+ OAuth apps), `pushover` (notifications) |

`skills_list` reports active and disabled skills, their origin/loading mode,
and whether each supported `SKILL.toml` tool is executable through the skill
adapter. `skill_read` resolves a skill by catalog name, then reads its
instruction file or a relative resource under that skill's own canonical root.
It never turns an arbitrary absolute path into a file-read bypass.

`skills_manage` is the approval-gated agent control plane for `create`,
`install`, `update`, `enable`, `disable`, `validate`, `sync`, and `remove`.
The CLI exposes `list`, `sync`, `install`, `update`, `enable`, `disable`, and
`remove`. Disabled workspace skills remain installed but are removed from
prompt selection and executable alias discovery.

`skill_execute` is the root execution fallback for enabled skill tools. It also
exports up to 32 deterministic `skill__<skill>__<tool>` aliases and is recorded
as the `skill` adapter by the canonical tool catalog. Supported manifest kinds
are `shell`, `script`, and `http`; shell/script scalar arguments are supplied
as `PRX_SKILL_ARG_*` environment variables, scripts are confined to their
canonical skill root, and HTTP tools reuse the public-network and domain
allowlist enforcement of `http_request`. Skill tool calls use the same policy,
approval, cancellation, idempotency, and audit execution service as native,
MCP, and WASM tools.

`document_ingest` accepts UTF-8 inline content or a workspace-relative file.
`document_sync` requires a workspace-relative file and derives a stable document
id from its canonical source path, so a later sync updates the same durable
document and chunks. Both operations are approval-gated, limited to 4 MiB, and
preserve source/hash metadata. `memory_reindex` rebuilds both Memory and Document
FTS indexes and backfills stale embeddings supported by the active backend.
Markdown sources are parsed as CommonMark blocks with nested heading paths;
lists, fenced code, tables, HTML blocks, and frontmatter remain atomic. Cold-start
Markdown hydration uses stable content anchors instead of line-number keys.

`mcp_status` performs discovery without calling a remote tool and reports each
server's transport, tool list, last refresh, and retained configuration or
discovery error. `mcp_call` also accepts `action: list|status|refresh`; its
backward-compatible default action is `call`. Discovery runs before the
provider-facing catalog is snapshotted. MCP and WASM aliases are deterministically
ordered and bounded; their root call tools remain available for capabilities
outside the alias window.

Stdio MCP servers are stateful: PRX keeps one client process alive per server
and serializes that server's calls, while calls to different servers may run in
parallel. An explicit refresh or workspace MCP configuration change closes the
cached session before rediscovery. A timeout or transport failure also resets
the session, but PRX does not replay the failed call because it may have already
performed an external side effect.

For prompt-guided models, the textual tool catalog now uses the same per-turn
intent selection as native function calling. Registered capabilities whose
availability is only `declared` remain visible to control-plane catalogs but are
not advertised as executable ToolSpecs.

`http_request` only accepts public HTTP(S) targets. It blocks local, private,
link-local, multicast, reserved, and legacy numeric-IP forms; applies
`browser.allowed_domains` when configured; never follows redirects; and stops
reading the response body at `http_request.max_response_size` rather than
buffering an unlimited body before truncation.

`cron` schedules with `kind: "at"` are one-shot regardless of physical retention: after their final success or failure they expose a typed terminal state and are never due again. `delete_after_run` atomically removes the job with its successful terminal commit; failures remain visible for run and event audit. The cron tool's update action can re-arm a retained terminal job with a new future `at` schedule; setting `enabled: true` alone does not, and an in-flight `at` schedule cannot be replaced. Manual `run` remains available for paused or terminal jobs; only a nonterminal `at` is consumed into terminal state. The CLI supports creating and displaying `at` jobs but does not expose an `at`-schedule update flag.

`xin` task actions are `list`, `get`, `add`, `update`, `remove`, `pause`,
`resume`, `cancel`, `run`, `runs`, `events`, and `status`. Durable workflow
actions are `goal_list`, `goal_get`, `goal_add`, `goal_pause`, `goal_resume`,
`goal_cancel`, `goal_remove`, `step_list`, `step_get`, `step_add`, and
`step_retry`. `pause` prevents future scheduling but lets a currently leased
execution finish; `cancel` revokes the lease and cooperatively stops active
work. Completed user and agent tasks remain available for `get`, `runs`, and
`events` until explicitly removed. When a trusted channel owner scope is
present, reads and mutations are restricted to that owner; local operator calls
without a channel scope retain workspace-wide visibility.

`message_send` normally replies on the channel the turn arrived on. An optional
`channel` argument names a different configured channel instead — resolved
against the same channel registry `sessions_spawn` announces into, so an
unknown name is an error that lists what is addressable rather than a silent
reply on the current channel. Cross-channel delivery must be permitted by
`send_allow` (see [Scope rules](configuration.md#scope-rules); the default for a
destination channel other than the turn's own is **deny**) and carries **text
only**: `[IMAGE:]` / `[VOICE:]` / `[DOCUMENT:]` markers and `as_voice` are
refused, because an attachment is a local path owned by the originating channel.
`action="react"` cannot be redirected — reactions are always delivered by the
Signal handle — so it rejects a `channel` naming anything else.

In `prx chat` the same tool name and the same schema are registered, but the
send is performed by the daemon: a chat session opens no IM connection of its
own, because a second listener would race the daemon for inbound messages. The
chat variant therefore requires `channel` (there is no conversation to inherit
one from), offers `action="send"` only, and refuses arguments it cannot carry
(`quote_timestamp` / `quote_author`) instead of dropping them. It reaches the
daemon over `POST /api/channels/{name}/send` using `[chat.daemon]`
(see [Configuration](configuration.md#chatdaemon)); when no daemon is running
the tool reports that and the chat turn continues. Every gate above — unknown
channel, `send_allow`, text-only — is applied **by the daemon**, against the
daemon's configuration; the chat side decides nothing. Because such a send has
no inbound conversation behind it, it is authorized as a send *from* the
operator plane channel `api`, which makes it cross-channel by construction and
therefore denied until an operator opts in with `send_allow`.

`sessions_spawn` also carries the two actions that let a correspondent on a
messaging channel hand work to a live `prx chat` session, rather than to a
sub-agent. `action="chat_sessions"` lists the sessions currently registered with
the daemon, and `action="chat_assign"` hands one of them a `task` with a
`disposition` of `queue` (default — wait for the session to be free), `steer`
(hand it to the turn running right now as extra input) or `interrupt` (stop that
turn first). Both are actions on this tool rather than tools of their own, so the
model's tool surface does not grow.

Assignment is **default-deny** and the identity it is judged on is the one the
runtime injected for the message being processed, never anything the model wrote:
an operator opts a correspondent in with `autonomy.scopes.assign_owners` or an
`assign_allow` scope rule (see
[Scope rules](configuration.md#scope-rules)). A caller with no grant is refused,
and the refusal names both parties by audit fingerprint only; the listing is
filtered to what that caller could actually assign to, so it cannot be used to
discover which sessions exist. The call is deliberately made in-process rather
than over `POST /api/chat-sessions/{id}/assign`: an HTTP request carries no
trustworthy caller identity, so the same call over the loopback API would be
authorized as the operator plane whoever typed the message.

`chat_assign` returns as soon as the mailbox accepts the task — a chat session has
no deadline, so waiting would park the correspondent's turn for as long as the
other end takes. Instead a **relay** is started: it waits for that assignment's
result and sends it back to the conversation the assignment came from, on the
channel that conversation arrived on. The destination is fixed from the assigning
turn and is never model-supplied, and the send passes the same `send_allow` /
`send_deny` gate every other outbound message does — a denied recipient is
withheld and logged with the destination's fingerprint. The relay has a
work-registry row of its own (its address is `relay_work_id` in the answer), so
`prx tasks list` shows it while it waits and `prx tasks kill` ends it; nothing
about it expires on a clock.

Agentic `delegate` configurations require an explicit non-empty
`allowed_tools`. A named list is intersected with the eligible parent registry;
unknown or ineligible names fail before the provider turn starts. The exclusive
wildcard form `allowed_tools = ["*"]` inherits all eligible parent tools except
`delegate` itself. An empty list never means “inherit all.”

Scheduler execution uses renewable database claim leases (`scheduler.claim_lease_secs`, default 90 seconds) and attempt fencing. A crashed worker's claim becomes recoverable at the lease expiry boundary, while an older attempt cannot record a run or overwrite the newer attempt's state. This protects cron state and run history from stale commits; it does **not** make external side effects exactly-once. Operators upgrading a shared database must coordinate shutdown of all older scheduler processes before starting lease-aware schedulers, because older binaries do not renew or honor the claim tuple.

Claim timestamps currently use each scheduler caller's UTC clock. Multi-node deployments must maintain NTP synchronization and bounded clock skew well below the configured lease interval. A schedule change is rejected while any claim is still active; once that claim has expired, an explicit update may clear the stale tuple. Manual runs of nonterminal jobs use the same renewable lease, delivery, and fenced commit path as background runs.

## Hooks System

Event-driven hooks let you extend agent behavior without modifying core code. Hooks fire shell commands (or WASM plugin callbacks) on lifecycle events.

### Events

| Event | When it fires |
|-------|--------------|
| `agent_start` | Agent loop begins a new turn |
| `agent_end` | Agent loop completes a turn |
| `llm_request` | Before sending a request to the LLM |
| `llm_response` | After receiving an LLM response |
| `tool_call_start` | Before a tool is executed |
| `tool_call` | After a tool completes |
| `turn_complete` | Full turn (LLM + tools) finished |
| `error` | Any error in the agent loop |

### Configuration

Create `hooks.json` in the workspace directory:

```json
{
  "hooks": {
    "tool_call": [
      {
        "command": "/usr/local/bin/log-tool",
        "args": ["--event", "tool_call"],
        "timeout_ms": 5000
      }
    ],
    "error": [
      {
        "command": "notify-send",
        "args": ["OpenPRX error"]
      }
    ]
  }
}
```

- `command` + `args` — executed directly, not via shell (no injection risk)
- `timeout_ms` — per-hook timeout, default 5000ms; valid range 1–300000ms
- `hooks.json` is capped at 256 KiB and hot-reloaded by content generation; an invalid candidate leaves the active generation unchanged
- payloads are capped at 4 MiB and passed through a restrictive temporary file plus optional stdin
- timeout covers stdin and process execution; timed-out children are killed and reaped, and temporary payload files are removed on every exit path
- WASM plugins with the `hook` capability also receive these events

## Webhook Receiver

Built-in HTTP webhook endpoint for receiving external events:

- HMAC-SHA256 signature verification
- Memory-backed event storage
- Route external events (GitHub, CI/CD, monitoring) into agent context
