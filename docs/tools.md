# Tools

43 built-in tools organized by category.

| Category | Tools |
|----------|-------|
| **Shell & Files** | `shell`, `file_read`, `file_write`, `git_operations` |
| **Web** | `web_search`, `web_fetch`, `http_request` |
| **Browser** | `browser` (automation), `browser_open`, `screenshot`, `canvas` |
| **Memory** | `memory_store`, `memory_recall`, `memory_search`, `memory_get`, `memory_forget` |
| **Messaging** | `message_send`, `tts` (text-to-speech) |
| **Sessions** | `sessions_spawn`, `sessions_send`, `sessions_list`, `sessions_history`, `session_status`, `subagents`, `delegate` |
| **Scheduling** | `cron_add`, `cron_list`, `cron_update`, `cron_remove`, `cron_run`, `cron_runs`, `schedule` |
| **Images** | `image`, `image_info` |
| **MCP** | `mcp` (Model Context Protocol client — connect to any MCP server) |
| **Remote Nodes** | `nodes` (control paired devices — camera, screen, location, run commands) |
| **Infrastructure** | `gateway`, `config_reload`, `proxy_config`, `agents_list` |
| **Integrations** | `composio` (1000+ OAuth apps), `pushover` (notifications) |

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
