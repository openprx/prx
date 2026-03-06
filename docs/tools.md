# Tools

45+ built-in tools organized by category.

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
| **Hardware** | `hardware_board_info`, `hardware_memory_map`, `hardware_memory_read` (ESP32/Arduino) |

## Hooks System

Event-driven hooks for extending agent behavior without modifying core code:

- **Events**: `agent_start`, `agent_end`, `llm_request`, `llm_response`, `tool_call_start`, `tool_call_end`, `message_received`, `message_sent`
- **Config**: `hooks.json` in workspace — map events to shell commands
- **Timeout**: Configurable per-hook (default 5s)

## Webhook Receiver

Built-in HTTP webhook endpoint for receiving external events:

- HMAC-SHA256 signature verification
- Memory-backed event storage
- Route external events (GitHub, CI/CD, monitoring) into agent context
