# OpenClaw vs PRX OpenAI-Codex Provider Audit (2026-03-14)

## Scope
Compared:
- OpenClaw
  - `/home/ck/.nvm/versions/node/v22.13.1/lib/node_modules/openclaw/dist/gateway-cli-BjsM6fWb.js`
  - `/home/ck/.nvm/versions/node/v22.13.1/lib/node_modules/openclaw/dist/pi-embedded-Cz5VjpnY.js`
- PRX
  - `/opt/worker/code/agents/prx/src/providers/openai_codex.rs`
  - `/opt/worker/code/agents/prx/src/providers/openai.rs`
  - `/opt/worker/code/agents/prx/src/agent/loop_.rs`
  - `/opt/worker/code/agents/prx/src/providers/traits.rs`

---

## Executive Summary
PRX `openai_codex` has a **critical native tool-calling break**: it advertises native tool support but never returns structured tool calls, so Responses API `function_call` outputs are effectively dropped and cannot be executed by the agent loop.

Additional medium/high issues exist in SSE termination and event coverage, causing possible false timeouts and loss of non-text outputs.

---

## A) Responses API streaming (SSE event parsing)

### OpenClaw
- Uses typed Responses event model for OpenResponses gateway and WS path.
- Declares/handles broad event family (`response.created`, `response.in_progress`, `response.output_item.added/done`, `response.content_part.added/done`, `response.output_text.delta/done`, `response.completed`, `response.failed`) in gateway implementation.
  - Ref: `gateway-cli-BjsM6fWb.js:19596-19649`, `20088-20224`
- WS runtime path processes typed events and finalizes on `response.completed` / `response.failed` / `error`.
  - Ref: `pi-embedded-Cz5VjpnY.js:100481`, `101000-101023`

### PRX
- SSE parser (`parse_sse_text`) only extracts text from:
  - `response.output_text.delta`
  - `response.output_text.done` (fallback)
  - `response.completed` / `response.done` (text extraction only)
  - Ref: `openai_codex.rs:258-273`
- Terminal break in stream reader depends on:
  - `data: [DONE]`, or
  - single-line JSON with terminal `type` per `data:` line
  - Ref: `openai_codex.rs:382-405`, `500-504`

### Finding
PRX event coverage is text-centric and misses structural event handling (output items, especially `function_call`). This is a functional gap vs OpenClaw.

---

## B) Tool call format extraction

### OpenClaw
- Responses output parsing explicitly handles output items of `type === "function_call"` and maps to tool calls.
  - Ref: `pi-embedded-Cz5VjpnY.js:100737-100766`
- Chat-completions-style `tool_calls` also handled in multiple places.
  - Ref: `pi-embedded-Cz5VjpnY.js:52327-52338`

### PRX
- `openai_codex.rs` deserializes response output into text-only structures:
  - `ResponsesOutput { content: Vec<ResponsesContent> }`
  - `ResponsesContent { type, text }`
  - Ref: `openai_codex.rs:71-82`
- No schema/logic for `function_call`, `call_id`, `arguments`, etc.
- Agent loop can consume structured calls (`resp.tool_calls`) **if provider returns them**.
  - Ref: `loop_.rs:2290-2318`

### Finding
PRX codex provider cannot extract tool calls from Responses API output items.

---

## C) `function_call` vs `tool_calls`

### OpenClaw
- Supports both patterns:
  - Responses API: `function_call` output items.
    - Ref: `pi-embedded-Cz5VjpnY.js:100751-100766`
  - Chat Completions API: `tool_calls` arrays.
    - Ref: `pi-embedded-Cz5VjpnY.js:52327-52338`

### PRX
- `openai.rs` handles chat-completions `tool_calls` correctly (good baseline).
  - Ref: `openai.rs:252-263`, `139-152`
- `openai_codex.rs` does **not** handle Responses `function_call` items.
- Worse: provider advertises native tool calling (`native_tool_calling: true`) but does not override `chat`/`chat_with_tools`, so default trait path returns empty `tool_calls`.
  - Ref: `openai_codex.rs:593-597`
  - Ref: default `Provider::chat` behavior in `traits.rs:304-356`

### Critical Bug #1 (High)
**Native tool-calling contract violation in PRX codex provider.**
- `openai_codex` claims native tool support but uses default `Provider::chat` path that only calls `chat_with_history` and returns `tool_calls: Vec::new()`.
- This prevents tool execution even when model emits function calls.
- Refs:
  - `openai_codex.rs:593-597`
  - `traits.rs:304-356`
  - `loop_.rs:2290-2318`

---

## D) Error handling (malformed responses, timeouts, partial streams)

### OpenClaw
- WS connection manager has explicit parse-shape guards and emits errors for malformed payloads.
  - Ref: `pi-embedded-Cz5VjpnY.js:100467-100480`
- Reconnect/backoff behavior in WS manager.
  - Ref: `pi-embedded-Cz5VjpnY.js:100439-100458`

### PRX
- Has per-chunk idle timeout and max payload guards.
  - Ref: `openai_codex.rs:479-497`
- But `parse_sse_text` silently skips non-JSON `data:` fragments (no parse error surfaced), which can hide malformed/partial events and degrade diagnostics.
  - Ref: `openai_codex.rs:313-325`

### Finding
PRX error handling is weaker for malformed SSE event payloads and can turn protocol issues into opaque "empty_or_unsupported_payload" outcomes.

---

## E) Stream termination

### OpenClaw
- Finalization keyed to semantic events (`response.completed`, `response.failed`, `error`) in WS path.
  - Ref: `pi-embedded-Cz5VjpnY.js:101000-101018`

### PRX
- Breaks stream only when:
  - `[DONE]`, or
  - a single-line `data: {"type": ...terminal...}` line is detectable.
  - Ref: `openai_codex.rs:382-405`, `500-504`

### Bug #2 (Medium)
**Terminal detection misses multi-line `data:` JSON events.**
- `contains_terminal_response_event` parses each `data:` line independently.
- If terminal event JSON is split across multiple `data:` lines, PRX won’t detect completion and may hit stream idle timeout.
- Refs:
  - `openai_codex.rs:388-405`
  - `openai_codex.rs:500-504`

### Bug #3 (Medium)
**Mislabelled SSE Content-Type can force false timeout before decode fallback.**
- `decode_responses_body` only enables early terminal detection when `Content-Type` contains `text/event-stream` (`is_sse` gate).
- Later decode has tolerant SSE fallback, but stream loop may already timeout if connection stays open.
- Refs:
  - `openai_codex.rs:473`
  - `openai_codex.rs:500-505`
  - fallback exists at `openai_codex.rs:442-447`

---

## F) Token/auth (codex OAuth vs PRX)

### OpenClaw
- Auth layer supports OAuth/token profiles and resolves provider auth dynamically.
  - Ref: `pi-embedded-Cz5VjpnY.js:28801-28831`, `28851-28887`
- OpenAI Responses WS path uses Bearer token; no ChatGPT account-id header requirement.
  - Ref: `pi-embedded-Cz5VjpnY.js:100411-100412`

### PRX
- Uses ChatGPT backend endpoint (`chatgpt.com/backend-api/codex/responses`) with required OAuth-derived account id header.
  - Ref: `openai_codex.rs:13`, `541-548`, `573-575`

### Missing Feature (PRX)
- No alternative path to OpenAI public Responses endpoint (`api.openai.com/v1/responses` / WS mode) with standard API-key auth.
- This reduces portability vs OpenClaw’s broader auth/runtime path.

---

## G) Content-Type handling

### OpenClaw
- WS path is event-object based (no manual SSE framing parse in that path).
- Gateway OpenResponses emits explicit SSE `event:` + `data:` frames.
  - Ref: `gateway-cli-BjsM6fWb.js:19703-19706`

### PRX
- `decode_responses_payload` is tolerant: content-type + body-shape inference (`event:`/`data:` vs JSON).
  - Ref: `openai_codex.rs:413-454`
- However early stream loop behavior still depends on header-derived `is_sse` (see Bug #3).

---

## Specific Bugs Found in PRX

1. **Critical: Native tool call path broken for openai-codex provider**
- Files:
  - `src/providers/openai_codex.rs:593-597`
  - `src/providers/traits.rs:304-356`
  - `src/agent/loop_.rs:2290-2318`
- Why: provider advertises native tool support but never returns structured tool calls.

2. **High: Responses `function_call` output items not parsed**
- Files:
  - `src/providers/openai_codex.rs:63-82`
  - `src/providers/openai_codex.rs:232-273`
- Why: response model is text-only; function call outputs are dropped.

3. **Medium: Terminal event detection fragile for multi-line `data:` events**
- Files:
  - `src/providers/openai_codex.rs:388-405`
  - `src/providers/openai_codex.rs:500-504`

4. **Medium: Early stream handling can timeout when SSE is mislabelled as JSON**
- Files:
  - `src/providers/openai_codex.rs:473`
  - `src/providers/openai_codex.rs:500-505`
  - `src/providers/openai_codex.rs:442-447`

5. **Medium: Malformed SSE event fragments are silently ignored in parser**
- Files:
  - `src/providers/openai_codex.rs:313-325`
- Why: parse failures are swallowed instead of surfaced with event context.

---

## Missing Features in PRX

1. Native extraction of Responses API `function_call` output items into `ChatResponse.tool_calls`.
2. `chat`/`chat_with_tools` implementation in `openai_codex` aligned with declared native tooling capability.
3. Full event handling parity for Responses stream events (`response.output_item.added/done`, etc.) for robust non-text outputs.
4. Optional OpenAI public Responses endpoint mode (API-key path), comparable to OpenClaw’s broader auth/runtime strategy.

---

## Recommended Fixes (Code Snippets)

### Fix 1: Implement native `chat` in `openai_codex` and return structured tool calls

```rust
// src/providers/openai_codex.rs
#[derive(Debug, Deserialize)]
struct ResponsesFunctionCall {
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ResponsesOutputItem {
    #[serde(rename = "message")]
    Message {
        #[serde(default)]
        content: Vec<ResponsesContent>,
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        #[serde(default)]
        call_id: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        arguments: Option<String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct ResponsesResponse {
    #[serde(default)]
    output: Vec<ResponsesOutputItem>,
    #[serde(default)]
    output_text: Option<String>,
}
```

```rust
// src/providers/openai_codex.rs
fn extract_tool_calls(response: &ResponsesResponse) -> Vec<crate::providers::traits::ToolCall> {
    response
        .output
        .iter()
        .filter_map(|item| match item {
            ResponsesOutputItem::FunctionCall { call_id, name, arguments } => {
                let name = name.as_deref()?.trim();
                if name.is_empty() { return None; }
                Some(crate::providers::traits::ToolCall {
                    id: call_id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                    name: name.to_string(),
                    arguments: arguments.clone().unwrap_or_else(|| "{}".to_string()),
                })
            }
            _ => None,
        })
        .collect()
}
```

### Fix 2: Override `chat`/`chat_with_tools` in `openai_codex`

```rust
#[async_trait]
impl Provider for OpenAiCodexProvider {
    // ...existing methods...

    async fn chat(
        &self,
        request: crate::providers::traits::ChatRequest<'_>,
        model: &str,
        _temperature: f64,
    ) -> anyhow::Result<crate::providers::traits::ChatResponse> {
        let (instructions, input) = build_responses_input(request.messages);
        let parsed = self.send_and_decode_full_response(input, instructions, model).await?;
        Ok(crate::providers::traits::ChatResponse {
            text: extract_responses_text(&parsed),
            tool_calls: extract_tool_calls(&parsed),
        })
    }
}
```

### Fix 3: Make terminal detection multi-line aware

```rust
fn contains_terminal_response_event(text: &str) -> bool {
    // Reuse parse_sse_text-style chunk assembly: join all data: lines per SSE event,
    // then parse JSON once per event block.
    for block in text.split("\n\n") {
        let data = block
            .lines()
            .filter_map(|l| l.trim_start().strip_prefix("data:"))
            .map(str::trim)
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() || data == "[DONE]" { continue; }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) {
            if matches!(v.get("type").and_then(|t| t.as_str()),
                Some("response.completed" | "response.done" | "response.failed" | "error")) {
                return true;
            }
        }
    }
    false
}
```

### Fix 4: Treat body heuristics as SSE in stream loop when headers lie

```rust
// inside decode_responses_body, after first chunk append:
let current = String::from_utf8_lossy(&body_bytes);
let looks_sse_by_body = current.contains("\ndata:") || current.starts_with("data:") || current.contains("\nevent:");
if is_sse || looks_sse_by_body {
    if contains_done_event(&current) || contains_terminal_response_event(&current) {
        break;
    }
}
```

### Fix 5: Surface malformed SSE parse segments for diagnostics

```rust
if let Ok(event) = serde_json::from_str::<Value>(line) {
    process_event(event)?;
} else {
    anyhow::bail!("OpenAI Codex provider_response_parse_error kind=malformed_sse_event_line line={}",
        super::sanitize_api_error(line));
}
```

---

## Risk Ranking
- High: Bug #1, Bug #2
- Medium: Bug #3, Bug #4, Bug #5

---

## Bottom Line
PRX’s `openai_codex` path is currently text-only in practice, despite declaring native tool capability. Compared to OpenClaw’s dual-format handling (`function_call` + `tool_calls`) and typed event flow, PRX is missing key logic needed for reliable Responses API tool execution.
