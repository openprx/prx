use crate::auth::AuthService;
use crate::auth::openai_oauth::extract_account_id_from_jwt;
use crate::providers::ProviderRuntimeOptions;
use crate::providers::traits::{
    ChatMessage, ChatRequest as ProviderChatRequest, ChatResponse as ProviderChatResponse, Provider,
    ToolCall as ProviderToolCall,
};
use crate::tools::ToolSpec;
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

const CODEX_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const DEFAULT_CODEX_INSTRUCTIONS: &str = "You are OpenPRX, a concise and helpful coding assistant.";
const DEFAULT_CODEX_STREAM_IDLE_TIMEOUT_SECS: u64 = 45;
const MAX_CODEX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

pub struct OpenAiCodexProvider {
    auth: AuthService,
    auth_profile_override: Option<String>,
    client: Client,
    stream_idle_timeout: Duration,
    reasoning_effort_override: Option<String>,
}

#[derive(Debug, Serialize)]
struct ResponsesRequest {
    model: String,
    input: Vec<ResponsesInput>,
    instructions: String,
    store: bool,
    stream: bool,
    text: ResponsesTextOptions,
    reasoning: ResponsesReasoningOptions,
    include: Vec<String>,
    tool_choice: String,
    parallel_tool_calls: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Serialize)]
struct ResponsesInput {
    role: String,
    content: Vec<ResponsesInputContent>,
}

#[derive(Debug, Serialize)]
struct ResponsesInputContent {
    #[serde(rename = "type")]
    kind: String,
    text: String,
}

#[derive(Debug, Serialize)]
struct ResponsesTextOptions {
    verbosity: String,
}

#[derive(Debug, Serialize)]
struct ResponsesReasoningOptions {
    effort: String,
    summary: String,
}

#[derive(Debug, Deserialize)]
struct ResponsesResponse {
    #[serde(default)]
    output: Vec<ResponsesOutputItem>,
    #[serde(default)]
    output_text: Option<String>,
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
struct ResponsesContent {
    #[serde(rename = "type")]
    kind: Option<String>,
    text: Option<String>,
}

impl OpenAiCodexProvider {
    pub fn new(options: &ProviderRuntimeOptions) -> Self {
        let state_dir = options.openprx_dir.clone().unwrap_or_else(default_openprx_dir);
        let auth = AuthService::new_with_codex_import(
            &state_dir,
            options.secrets_encrypt,
            options.codex_auth_json_path.clone(),
            options.codex_auth_json_auto_import,
        );

        Self {
            auth,
            auth_profile_override: options.auth_profile_override.clone(),
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| Client::new()),
            stream_idle_timeout: resolve_stream_idle_timeout(options.codex_stream_idle_timeout_secs),
            reasoning_effort_override: options.codex_reasoning_effort.clone(),
        }
    }
}

fn default_openprx_dir() -> PathBuf {
    directories::UserDirs::new().map_or_else(
        || PathBuf::from(".openprx"),
        |dirs| {
            let primary = dirs.home_dir().join(".openprx");
            if primary.exists() {
                primary
            } else {
                let legacy = dirs.home_dir().join(".openprx");
                if legacy.exists() { legacy } else { primary }
            }
        },
    )
}

fn resolve_stream_idle_timeout(config_secs: Option<u64>) -> Duration {
    Duration::from_secs(config_secs.unwrap_or(DEFAULT_CODEX_STREAM_IDLE_TIMEOUT_SECS).max(1))
}

fn first_nonempty(text: Option<&str>) -> Option<String> {
    text.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn resolve_instructions(system_prompt: Option<&str>) -> String {
    first_nonempty(system_prompt).unwrap_or_else(|| DEFAULT_CODEX_INSTRUCTIONS.to_string())
}

fn normalize_model_id(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

fn build_responses_input(messages: &[ChatMessage]) -> (String, Vec<ResponsesInput>) {
    let mut system_parts: Vec<&str> = Vec::new();
    let mut input: Vec<ResponsesInput> = Vec::new();

    for msg in messages {
        match msg.role.as_str() {
            "system" => system_parts.push(&msg.content),
            "user" => {
                input.push(ResponsesInput {
                    role: "user".to_string(),
                    content: vec![ResponsesInputContent {
                        kind: "input_text".to_string(),
                        text: msg.content.clone(),
                    }],
                });
            }
            "assistant" => {
                input.push(ResponsesInput {
                    role: "assistant".to_string(),
                    content: vec![ResponsesInputContent {
                        kind: "output_text".to_string(),
                        text: msg.content.clone(),
                    }],
                });
            }
            _ => {}
        }
    }

    let instructions = if system_parts.is_empty() {
        DEFAULT_CODEX_INSTRUCTIONS.to_string()
    } else {
        system_parts.join("\n\n")
    };

    (instructions, input)
}

fn clamp_reasoning_effort(model: &str, effort: &str) -> String {
    let id = normalize_model_id(model);
    if (id.starts_with("gpt-5.2") || id.starts_with("gpt-5.3")) && effort == "minimal" {
        return "low".to_string();
    }
    if id == "gpt-5.1" && effort == "xhigh" {
        return "high".to_string();
    }
    if id == "gpt-5.1-codex-mini" {
        return if effort == "high" || effort == "xhigh" {
            "high".to_string()
        } else {
            "medium".to_string()
        };
    }
    effort.to_string()
}

const VALID_REASONING_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh"];

fn resolve_reasoning_effort(model_id: &str, config_effort: Option<&str>) -> String {
    let base = match config_effort {
        None => "xhigh",
        Some(effort) if VALID_REASONING_EFFORTS.contains(&effort) => effort,
        Some(invalid) => {
            tracing::warn!(
                value = invalid,
                valid = ?VALID_REASONING_EFFORTS,
                "invalid reasoning_effort config value, falling back to default"
            );
            "xhigh"
        }
    };
    clamp_reasoning_effort(model_id, base)
}

fn nonempty_preserve(text: Option<&str>) -> Option<String> {
    text.and_then(|value| {
        if value.is_empty() {
            None
        } else {
            Some(value.to_string())
        }
    })
}

fn extract_responses_text(response: &ResponsesResponse) -> Option<String> {
    if let Some(text) = first_nonempty(response.output_text.as_deref()) {
        return Some(text);
    }

    for item in &response.output {
        if let ResponsesOutputItem::Message { content } = item {
            for part in content {
                if part.kind.as_deref() == Some("output_text") {
                    if let Some(text) = first_nonempty(part.text.as_deref()) {
                        return Some(text);
                    }
                }
            }
        }
    }

    for item in &response.output {
        if let ResponsesOutputItem::Message { content } = item {
            for part in content {
                if let Some(text) = first_nonempty(part.text.as_deref()) {
                    return Some(text);
                }
            }
        }
    }

    None
}

fn convert_tools(tools: Option<&[ToolSpec]>) -> Option<Vec<serde_json::Value>> {
    tools.map(|items| {
        items
            .iter()
            .map(|tool| {
                serde_json::json!({
                    "type": "function",
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                })
            })
            .collect()
    })
}

fn sanitize_codex_tool_name(name: &str) -> String {
    name.replace('.', "_")
}

fn sanitize_codex_tools(
    tools: Option<Vec<serde_json::Value>>,
) -> anyhow::Result<(Option<Vec<serde_json::Value>>, HashMap<String, String>)> {
    let mut tool_name_map = HashMap::new();
    let Some(items) = tools else {
        return Ok((None, tool_name_map));
    };

    let mut sanitized_items = Vec::with_capacity(items.len());
    for mut tool in items {
        let Some(obj) = tool.as_object_mut() else {
            sanitized_items.push(tool);
            continue;
        };
        let Some(name) = obj.get("name").and_then(Value::as_str) else {
            sanitized_items.push(tool);
            continue;
        };

        let sanitized = sanitize_codex_tool_name(name);
        if sanitized != name {
            tracing::warn!(
                original_tool_name = name,
                sanitized_tool_name = sanitized,
                "OpenAI Codex tool name sanitized to satisfy API pattern"
            );
            if let Some(existing) = tool_name_map.insert(sanitized.clone(), name.to_string()) {
                if existing != name {
                    anyhow::bail!(
                        "Tool name collision after sanitization: '{}' and '{}' both map to '{}'",
                        existing,
                        name,
                        sanitized
                    );
                }
            }
            obj.insert("name".to_string(), Value::String(sanitized));
        }

        sanitized_items.push(tool);
    }

    Ok((Some(sanitized_items), tool_name_map))
}

fn parse_native_tool_spec(value: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    if value
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind == "function")
    {
        Ok(value)
    } else {
        anyhow::bail!("Invalid OpenAI Codex tool specification: expected type='function'")
    }
}

fn extract_tool_calls(response: &ResponsesResponse, tool_name_map: &HashMap<String, String>) -> Vec<ProviderToolCall> {
    response
        .output
        .iter()
        .filter_map(|item| match item {
            ResponsesOutputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => {
                let name = name.as_deref().map(str::trim)?;
                if name.is_empty() {
                    return None;
                }
                let name = tool_name_map.get(name).cloned().unwrap_or_else(|| name.to_string());
                Some(ProviderToolCall {
                    id: call_id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                    name,
                    arguments: arguments.clone().unwrap_or_else(|| "{}".to_string()),
                })
            }
            _ => None,
        })
        .collect()
}

fn has_semantic_output(response: &ResponsesResponse) -> bool {
    extract_responses_text(response).is_some() || !extract_tool_calls(response, &HashMap::new()).is_empty()
}

fn extract_stream_event_text(event: &Value, saw_delta: bool) -> Option<String> {
    let event_type = event.get("type").and_then(Value::as_str);
    match event_type {
        Some("response.output_text.delta") => nonempty_preserve(event.get("delta").and_then(Value::as_str)),
        Some("response.output_text.done") if !saw_delta => nonempty_preserve(event.get("text").and_then(Value::as_str)),
        Some("response.completed" | "response.done") => event
            .get("response")
            .and_then(|value| serde_json::from_value::<ResponsesResponse>(value.clone()).ok())
            .and_then(|response| extract_responses_text(&response)),
        _ => None,
    }
}

fn extract_stream_output_item(event: &Value) -> Option<ResponsesOutputItem> {
    let event_type = event.get("type").and_then(Value::as_str)?;
    if event_type != "response.output_item.done" {
        return None;
    }
    let item = event.get("item")?.clone();
    serde_json::from_value::<ResponsesOutputItem>(item).ok()
}

fn parse_sse_text(body: &str) -> anyhow::Result<Option<ResponsesResponse>> {
    let mut saw_delta = false;
    let mut delta_accumulator = String::new();
    let mut fallback_text = None;
    let mut output = Vec::new();
    let mut completed_response = None;
    let mut buffer = body.to_string();

    let mut process_event = |event: Value| -> anyhow::Result<()> {
        if let Some(message) = extract_stream_error_message(&event) {
            return Err(anyhow::anyhow!("OpenAI Codex stream error: {message}"));
        }
        if let Some(item) = extract_stream_output_item(&event) {
            output.push(item);
        }
        if matches!(
            event.get("type").and_then(Value::as_str),
            Some("response.completed" | "response.done")
        ) {
            if let Some(response) = event.get("response") {
                if let Ok(parsed) = serde_json::from_value::<ResponsesResponse>(response.clone()) {
                    completed_response = Some(parsed);
                }
            }
        }
        if let Some(text) = extract_stream_event_text(&event, saw_delta) {
            let event_type = event.get("type").and_then(Value::as_str);
            if event_type == Some("response.output_text.delta") {
                saw_delta = true;
                delta_accumulator.push_str(&text);
            } else if fallback_text.is_none() {
                fallback_text = Some(text);
            }
        }
        Ok(())
    };

    let mut process_chunk = |chunk: &str| -> anyhow::Result<()> {
        let data_lines: Vec<String> = chunk
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(|line| line.trim().to_string())
            .collect();
        if data_lines.is_empty() {
            return Ok(());
        }

        let joined = data_lines.join("\n");
        let trimmed = joined.trim();
        if trimmed.is_empty() || trimmed == "[DONE]" {
            return Ok(());
        }

        if let Ok(event) = serde_json::from_str::<Value>(trimmed) {
            return process_event(event);
        }

        for line in data_lines {
            let line = line.trim();
            if line.is_empty() || line == "[DONE]" {
                continue;
            }
            if let Ok(event) = serde_json::from_str::<Value>(line) {
                process_event(event)?;
            } else {
                tracing::warn!(
                    "OpenAI Codex provider_response_parse_warning kind=malformed_sse_data_line line={}",
                    super::sanitize_api_error(line)
                );
            }
        }

        Ok(())
    };

    loop {
        let Some(idx) = buffer.find("\n\n") else {
            break;
        };

        let chunk = buffer[..idx].to_string();
        buffer = buffer[idx + 2..].to_string();
        process_chunk(&chunk)?;
    }

    if !buffer.trim().is_empty() {
        process_chunk(&buffer)?;
    }

    if let Some(mut response) = completed_response {
        if response.output.is_empty() && !output.is_empty() {
            response.output = output;
        }
        if response.output_text.is_none() {
            if saw_delta {
                response.output_text = nonempty_preserve(Some(&delta_accumulator));
            } else {
                response.output_text = fallback_text;
            }
        }
        if !has_semantic_output(&response) {
            return Ok(None);
        }
        return Ok(Some(response));
    }

    let output_text = if saw_delta {
        nonempty_preserve(Some(&delta_accumulator))
    } else {
        fallback_text
    };
    if output_text.is_none() && output.is_empty() {
        return Ok(None);
    }

    Ok(Some(ResponsesResponse { output, output_text }))
}

fn extract_stream_error_message(event: &Value) -> Option<String> {
    let event_type = event.get("type").and_then(Value::as_str);

    if event_type == Some("error") {
        return first_nonempty(
            event
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| event.get("code").and_then(Value::as_str))
                .or_else(|| {
                    event
                        .get("error")
                        .and_then(|error| error.get("message"))
                        .and_then(Value::as_str)
                }),
        );
    }

    if event_type == Some("response.failed") {
        return first_nonempty(
            event
                .get("response")
                .and_then(|response| response.get("error"))
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str),
        );
    }

    None
}

fn contains_done_event(text: &str) -> bool {
    text.lines()
        .map(str::trim)
        .any(|line| line == "data: [DONE]" || line == "data:[DONE]")
}

fn contains_terminal_response_event(text: &str) -> bool {
    text.split("\n\n").any(|block| {
        let data_lines = block
            .lines()
            .filter_map(|line| line.trim_start().strip_prefix("data:"))
            .map(str::trim)
            .collect::<Vec<_>>();
        let data = data_lines.join("\n");
        if data.is_empty() || data == "[DONE]" {
            return false;
        }
        if let Ok(event) = serde_json::from_str::<Value>(&data) {
            return matches!(
                event.get("type").and_then(Value::as_str),
                Some("response.completed" | "response.done" | "response.failed" | "error")
            );
        }
        data_lines.into_iter().any(|line| {
            if line.is_empty() || line == "[DONE]" {
                return false;
            }
            let Ok(event) = serde_json::from_str::<Value>(line) else {
                return false;
            };
            matches!(
                event.get("type").and_then(Value::as_str),
                Some("response.completed" | "response.done" | "response.failed" | "error")
            )
        })
    })
}

fn normalize_content_type(value: Option<&str>) -> String {
    first_nonempty(value)
        .map(|raw| raw.to_ascii_lowercase())
        .unwrap_or_else(|| "unknown".to_string())
}

fn decode_responses_payload(body: &str, content_type: Option<&str>) -> anyhow::Result<ResponsesResponse> {
    let normalized_content_type = normalize_content_type(content_type);
    let trimmed = body.trim_start();
    let looks_like_sse = normalized_content_type.contains("text/event-stream")
        || trimmed.starts_with("event:")
        || trimmed.starts_with("data:");
    let looks_like_json =
        normalized_content_type.contains("application/json") || trimmed.starts_with('{') || trimmed.starts_with('[');

    if looks_like_sse {
        if let Some(response) = parse_sse_text(body)? {
            if has_semantic_output(&response) {
                return Ok(response);
            }
        }
    }

    if looks_like_json {
        let parsed: ResponsesResponse = serde_json::from_str(body).map_err(|err| {
            anyhow::anyhow!(
                "OpenAI Codex provider_response_parse_error kind=malformed_json content_type={} detail={}",
                normalized_content_type,
                super::sanitize_api_error(&err.to_string())
            )
        })?;
        if has_semantic_output(&parsed) {
            return Ok(parsed);
        }
    }

    // Keep tolerant fallback order for mislabelled Content-Type.
    if !looks_like_sse {
        if let Some(response) = parse_sse_text(body)? {
            if has_semantic_output(&response) {
                return Ok(response);
            }
        }
    }
    if !looks_like_json {
        if let Ok(parsed) = serde_json::from_str::<ResponsesResponse>(body) {
            if has_semantic_output(&parsed) {
                return Ok(parsed);
            }
        }
    }

    Err(anyhow::anyhow!(
        "OpenAI Codex provider_response_parse_error kind=empty_or_unsupported_payload content_type={} body_len={}",
        normalized_content_type,
        body.len()
    ))
}

async fn decode_responses_body(
    response: reqwest::Response,
    idle_timeout: Duration,
) -> anyhow::Result<ResponsesResponse> {
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_string());
    let normalized_content_type = normalize_content_type(content_type.as_deref());
    let is_sse_by_header = normalized_content_type.contains("text/event-stream");

    let mut stream = response.bytes_stream();
    let mut body_bytes = Vec::new();

    loop {
        let next = tokio::time::timeout(idle_timeout, stream.next()).await.map_err(|_| {
            anyhow::anyhow!(
                "OpenAI Codex provider_response_timeout kind=stream_idle_timeout timeout_ms={} content_type={}",
                idle_timeout.as_millis(),
                normalized_content_type
            )
        })?;

        match next {
            Some(Ok(chunk)) => {
                body_bytes.extend_from_slice(&chunk);
                if body_bytes.len() > MAX_CODEX_RESPONSE_BYTES {
                    anyhow::bail!(
                        "OpenAI Codex provider_response_parse_error kind=payload_too_large content_type={} body_len={}",
                        normalized_content_type,
                        body_bytes.len()
                    );
                }

                let current = String::from_utf8_lossy(&body_bytes);
                let looks_sse_by_body =
                    current.contains("\ndata:") || current.starts_with("data:") || current.contains("\nevent:");
                if is_sse_by_header || looks_sse_by_body {
                    if contains_done_event(&current) || contains_terminal_response_event(&current) {
                        break;
                    }
                }
            }
            Some(Err(err)) => {
                anyhow::bail!(
                    "OpenAI Codex provider_response_parse_error kind=body_read_failed content_type={} detail={}",
                    normalized_content_type,
                    super::sanitize_api_error(&err.to_string())
                );
            }
            None => break,
        }
    }

    let body = String::from_utf8_lossy(&body_bytes).into_owned();
    decode_responses_payload(&body, content_type.as_deref())
}

impl OpenAiCodexProvider {
    async fn send_and_decode_full_response(
        &self,
        input: Vec<ResponsesInput>,
        instructions: String,
        model: &str,
        tools: Option<Vec<serde_json::Value>>,
    ) -> anyhow::Result<ResponsesResponse> {
        let access_token = self
            .auth
            .get_valid_openai_access_token(self.auth_profile_override.as_deref())
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("OpenAI Codex auth profile not found. Run `prx auth login --provider openai-codex`.")
            })?;
        let profile = self
            .auth
            .get_profile("openai-codex", self.auth_profile_override.as_deref())?;
        let account_id = profile
            .and_then(|profile| profile.account_id)
            .or_else(|| extract_account_id_from_jwt(&access_token))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "OpenAI Codex account id not found in auth profile/token. Run `prx auth login --provider openai-codex` again."
                )
            })?;
        let normalized_model = normalize_model_id(model);

        let request = ResponsesRequest {
            model: normalized_model.to_string(),
            input,
            instructions,
            store: false,
            stream: true,
            text: ResponsesTextOptions {
                verbosity: "medium".to_string(),
            },
            reasoning: ResponsesReasoningOptions {
                effort: resolve_reasoning_effort(normalized_model, self.reasoning_effort_override.as_deref()),
                summary: "auto".to_string(),
            },
            include: vec!["reasoning.encrypted_content".to_string()],
            tool_choice: "auto".to_string(),
            parallel_tool_calls: true,
            tools,
        };

        let response = self
            .client
            .post(CODEX_RESPONSES_URL)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("chatgpt-account-id", account_id)
            .header("OpenAI-Beta", "responses=experimental")
            .header("originator", "pi")
            .header("accept", "text/event-stream")
            .header("accept-encoding", "identity")
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(super::api_error("OpenAI Codex", response).await);
        }

        decode_responses_body(response, self.stream_idle_timeout).await
    }

    async fn send_responses_request(
        &self,
        input: Vec<ResponsesInput>,
        instructions: String,
        model: &str,
    ) -> anyhow::Result<String> {
        let response = self
            .send_and_decode_full_response(input, instructions, model, None)
            .await?;
        extract_responses_text(&response).ok_or_else(|| anyhow::anyhow!("No response from OpenAI Codex"))
    }
}

#[async_trait]
impl Provider for OpenAiCodexProvider {
    fn capabilities(&self) -> crate::providers::traits::ProviderCapabilities {
        crate::providers::traits::ProviderCapabilities {
            native_tool_calling: true,
            vision: true,
        }
    }

    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        let input = vec![ResponsesInput {
            role: "user".to_string(),
            content: vec![ResponsesInputContent {
                kind: "input_text".to_string(),
                text: message.to_string(),
            }],
        }];
        self.send_responses_request(input, resolve_instructions(system_prompt), model)
            .await
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        let (instructions, input) = build_responses_input(messages);
        self.send_responses_request(input, instructions, model).await
    }

    async fn chat(
        &self,
        request: ProviderChatRequest<'_>,
        model: &str,
        _temperature: f64,
    ) -> anyhow::Result<ProviderChatResponse> {
        let (instructions, input) = build_responses_input(request.messages);
        let (tools, tool_name_map) = sanitize_codex_tools(convert_tools(request.tools))?;
        let parsed = self
            .send_and_decode_full_response(input, instructions, model, tools)
            .await?;
        Ok(ProviderChatResponse {
            text: extract_responses_text(&parsed),
            tool_calls: extract_tool_calls(&parsed, &tool_name_map),
            reasoning_content: None,
        })
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
        model: &str,
        _temperature: f64,
    ) -> anyhow::Result<ProviderChatResponse> {
        let (instructions, input) = build_responses_input(messages);
        let native_tools: Option<Vec<serde_json::Value>> = if tools.is_empty() {
            None
        } else {
            Some(
                tools
                    .iter()
                    .cloned()
                    .map(parse_native_tool_spec)
                    .collect::<Result<Vec<_>, _>>()?,
            )
        };
        let (native_tools, tool_name_map) = sanitize_codex_tools(native_tools)?;
        let parsed = self
            .send_and_decode_full_response(input, instructions, model, native_tools)
            .await?;
        Ok(ProviderChatResponse {
            text: extract_responses_text(&parsed),
            tool_calls: extract_tool_calls(&parsed, &tool_name_map),
            reasoning_content: None,
        })
    }
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_output_text_first() {
        let response = ResponsesResponse {
            output: vec![],
            output_text: Some("hello".into()),
        };
        assert_eq!(extract_responses_text(&response).as_deref(), Some("hello"));
    }

    #[test]
    fn extracts_nested_output_text() {
        let response = ResponsesResponse {
            output: vec![ResponsesOutputItem::Message {
                content: vec![ResponsesContent {
                    kind: Some("output_text".into()),
                    text: Some("nested".into()),
                }],
            }],
            output_text: None,
        };
        assert_eq!(extract_responses_text(&response).as_deref(), Some("nested"));
    }

    #[test]
    fn default_state_dir_is_non_empty() {
        let path = default_openprx_dir();
        assert!(!path.as_os_str().is_empty());
    }

    #[test]
    fn resolve_instructions_uses_default_when_missing() {
        assert_eq!(resolve_instructions(None), DEFAULT_CODEX_INSTRUCTIONS.to_string());
    }

    #[test]
    fn resolve_instructions_uses_default_when_blank() {
        assert_eq!(
            resolve_instructions(Some("   ")),
            DEFAULT_CODEX_INSTRUCTIONS.to_string()
        );
    }

    #[test]
    fn resolve_instructions_uses_system_prompt_when_present() {
        assert_eq!(resolve_instructions(Some("Be strict")), "Be strict".to_string());
    }

    #[test]
    fn clamp_reasoning_effort_adjusts_known_models() {
        assert_eq!(clamp_reasoning_effort("gpt-5.3-codex", "minimal"), "low".to_string());
        assert_eq!(clamp_reasoning_effort("gpt-5.1", "xhigh"), "high".to_string());
        assert_eq!(
            clamp_reasoning_effort("gpt-5.1-codex-mini", "low"),
            "medium".to_string()
        );
        assert_eq!(
            clamp_reasoning_effort("gpt-5.1-codex-mini", "xhigh"),
            "high".to_string()
        );
        assert_eq!(clamp_reasoning_effort("gpt-5.3-codex", "xhigh"), "xhigh".to_string());
    }

    #[test]
    fn parse_sse_text_reads_output_text_delta() {
        let payload = r#"data: {"type":"response.created","response":{"id":"resp_123"}}

data: {"type":"response.output_text.delta","delta":"Hello"}
data: {"type":"response.output_text.delta","delta":" world"}
data: {"type":"response.completed","response":{"output_text":"Hello world"}}
data: [DONE]
"#;

        let response = parse_sse_text(payload).unwrap().unwrap();
        assert_eq!(response.output_text.as_deref(), Some("Hello world"));
    }

    #[test]
    fn parse_sse_text_falls_back_to_completed_response() {
        let payload = r#"data: {"type":"response.completed","response":{"output_text":"Done"}}
data: [DONE]
"#;

        let response = parse_sse_text(payload).unwrap().unwrap();
        assert_eq!(response.output_text.as_deref(), Some("Done"));
    }

    #[test]
    fn build_responses_input_maps_content_types_by_role() {
        let messages = vec![
            ChatMessage {
                role: "system".into(),
                content: "You are helpful.".into(),
            },
            ChatMessage {
                role: "user".into(),
                content: "Hi".into(),
            },
            ChatMessage {
                role: "assistant".into(),
                content: "Hello!".into(),
            },
            ChatMessage {
                role: "user".into(),
                content: "Thanks".into(),
            },
        ];
        let (instructions, input) = build_responses_input(&messages);
        assert_eq!(instructions, "You are helpful.");
        assert_eq!(input.len(), 3);

        let json: Vec<Value> = input.iter().map(|item| serde_json::to_value(item).unwrap()).collect();
        assert_eq!(json[0]["role"], "user");
        assert_eq!(json[0]["content"][0]["type"], "input_text");
        assert_eq!(json[1]["role"], "assistant");
        assert_eq!(json[1]["content"][0]["type"], "output_text");
        assert_eq!(json[2]["role"], "user");
        assert_eq!(json[2]["content"][0]["type"], "input_text");
    }

    #[test]
    fn build_responses_input_uses_default_instructions_without_system() {
        let messages = vec![ChatMessage {
            role: "user".into(),
            content: "Hello".into(),
        }];
        let (instructions, input) = build_responses_input(&messages);
        assert_eq!(instructions, DEFAULT_CODEX_INSTRUCTIONS);
        assert_eq!(input.len(), 1);
    }

    #[test]
    fn build_responses_input_ignores_unknown_roles() {
        let messages = vec![
            ChatMessage {
                role: "tool".into(),
                content: "result".into(),
            },
            ChatMessage {
                role: "user".into(),
                content: "Go".into(),
            },
        ];
        let (instructions, input) = build_responses_input(&messages);
        assert_eq!(instructions, DEFAULT_CODEX_INSTRUCTIONS);
        assert_eq!(input.len(), 1);
        let json = serde_json::to_value(&input[0]).unwrap();
        assert_eq!(json["role"], "user");
    }

    #[test]
    fn decode_payload_malformed_json_returns_structured_error() {
        let err = decode_responses_payload("{not-json", Some("application/json"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("provider_response_parse_error"));
        assert!(err.contains("kind=malformed_json"));
    }

    #[test]
    fn decode_payload_unexpected_content_type_still_parses_json() {
        let body = r#"{"output_text":"hello"}"#;
        let response = decode_responses_payload(body, Some("text/plain")).unwrap();
        assert_eq!(response.output_text.as_deref(), Some("hello"));
    }

    #[test]
    fn decode_payload_fallback_error_does_not_leak_protocol_labels() {
        let body = r#"data: {"type":"response.output_text.delta","delta":""}
data: {"type":"response.completed","response":{"output":[]}}
data: [DONE]
"#;
        let err = decode_responses_payload(body, Some("text/event-stream"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("provider_response_parse_error"));
        assert!(!err.contains("response.output_text.delta"));
    }

    #[test]
    fn detects_terminal_response_event_without_done_marker() {
        let body = r#"data: {"type":"response.output_text.delta","delta":"partial"}
data: {"type":"response.completed","response":{"output_text":"final"}}
"#;
        assert!(contains_terminal_response_event(body));
        assert!(!contains_done_event(body));
    }

    #[test]
    fn parse_sse_text_extracts_function_call_output_item() {
        let payload = r#"event: response.output_item.done
data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_123","name":"list_files","arguments":"{\"path\":\".\"}"}}

data: [DONE]
"#;

        let response = parse_sse_text(payload).unwrap().unwrap();
        let tool_calls = extract_tool_calls(&response, &HashMap::new());
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "call_123");
        assert_eq!(tool_calls[0].name, "list_files");
        assert_eq!(tool_calls[0].arguments, "{\"path\":\".\"}");
    }

    #[test]
    fn sanitize_codex_tools_rewrites_dot_name_and_tracks_reverse_mapping() {
        let tools = Some(vec![serde_json::json!({
            "type": "function",
            "name": "email.execute",
            "description": "email action",
            "parameters": {"type":"object"}
        })]);

        let (sanitized, mapping) = sanitize_codex_tools(tools).expect("sanitize should succeed");
        let sanitized = sanitized.expect("sanitized tools must exist");
        assert_eq!(sanitized[0]["name"], "email_execute");
        assert_eq!(mapping.get("email_execute").map(String::as_str), Some("email.execute"));
    }

    #[test]
    fn extract_tool_calls_restores_original_name_from_mapping() {
        let response = ResponsesResponse {
            output: vec![ResponsesOutputItem::FunctionCall {
                call_id: Some("call_123".to_string()),
                name: Some("email_execute".to_string()),
                arguments: Some("{\"to\":\"x@example.com\"}".to_string()),
            }],
            output_text: None,
        };
        let mut mapping = HashMap::new();
        mapping.insert("email_execute".to_string(), "email.execute".to_string());

        let tool_calls = extract_tool_calls(&response, &mapping);
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].name, "email.execute");
    }

    #[test]
    fn detects_terminal_response_event_for_multiline_data_block() {
        let body = "event: response.completed\n\
data: {\"type\":\"response.completed\",\"response\":\n\
data: {\"output_text\":\"final\"}}\n\n";
        assert!(contains_terminal_response_event(body));
    }
}
