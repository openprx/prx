# Multi-Identity `sessions_spawn` Example

This example shows how to configure multiple agent identities for `sessions_spawn`.

## Config (`config.toml`)

```toml
[agents.alpha]
provider = "openrouter"
model = "anthropic/claude-sonnet-4-6"
identity_dir = "identities/alpha"
memory_scope = "isolated"
spawn_enabled = true
allowed_tools = ["file_read", "file_write", "memory_store", "memory_recall", "shell"]

[agents.bravo]
provider = "openrouter"
model = "openai/gpt-4.1"
identity_dir = "identities/bravo"
memory_scope = "shared"
spawn_enabled = true
allowed_tools = ["web_search", "http_request", "memory_store", "memory_recall"]
```

## Identity Directory Layout

```text
workspace/
  identities/
    alpha/
      SOUL.md
      AGENTS.md
      IDENTITY.md
      USER.md
      TOOLS.md
      MEMORY.md
    bravo/
      SOUL.md
      AGENTS.md
      IDENTITY.md
      USER.md
      TOOLS.md
      MEMORY.md
```

## Tool Call Example

```json
{
  "name": "sessions_spawn",
  "arguments": {
    "task": "Review this PR and report high-risk issues.",
    "agent": "alpha"
  }
}
```

