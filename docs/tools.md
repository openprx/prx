# Tools

38 built-in tools organized by category.

| Category | Tools |
|----------|-------|
| **Shell & Files** | `shell`, `file_read`, `file_write`, `git_operations` |
| **Web** | `web_search`, `web_fetch`, `http_request` |
| **Memory** | `memory_store`, `memory_recall`, `memory_search`, `memory_get`, `memory_forget` |
| **Messaging** | `message_send` |
| **Sessions** | `sessions_spawn`, `sessions_send`, `sessions_list`, `sessions_history`, `session_status`, `subagents`, `delegate` |
| **Scheduling** | `cron` (unified — actions: add/schedule, once, list, get, remove/cancel, update/patch, run, runs/history, events, pause, resume, status) |
| **Images** | `image`, `image_info` |
| **MCP** | `mcp` (Model Context Protocol client — connect to any MCP server) |
| **Remote Nodes** | `nodes` (control paired devices — camera, screen, location, run commands) |
| **Infrastructure** | `gateway`, `config_reload`, `proxy_config`, `agents_list` |
| **Integrations** | `composio` (1000+ OAuth apps), `pushover` (notifications) |

`cron` schedules with `kind: "at"` are one-shot regardless of physical retention: after their final success or failure they expose a typed terminal state and are never due again. `delete_after_run` atomically removes the job with its successful terminal commit; failures remain visible for run and event audit. The cron tool's update action can re-arm a retained terminal job with a new future `at` schedule; setting `enabled: true` alone does not, and an in-flight `at` schedule cannot be replaced. Manual `run` remains available for paused or terminal jobs; only a nonterminal `at` is consumed into terminal state. The CLI supports creating and displaying `at` jobs but does not expose an `at`-schedule update flag.

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
- `timeout_ms` — per-hook timeout, default 5000ms
- `hooks.json` is hot-reloaded on change (no restart required)
- WASM plugins with the `hook` capability also receive these events

## Webhook Receiver

Built-in HTTP webhook endpoint for receiving external events:

- HMAC-SHA256 signature verification
- Memory-backed event storage
- Route external events (GitHub, CI/CD, monitoring) into agent context
