//! AWS Bedrock provider using the Converse API.
//!
//! Authentication: AWS AKSK (Access Key ID + Secret Access Key)
//! from `~/.aws/credentials` file. SigV4 signing is implemented manually
//! using hmac/sha2 crates — no AWS SDK dependency.

use crate::providers::traits::{
    ChatMessage, ChatRequest as ProviderChatRequest, ChatResponse as ProviderChatResponse, Provider,
    ProviderCapabilities, StreamChunk, StreamError, StreamOptions, StreamResult, ToolCall as ProviderToolCall,
    ToolCallChunk, ToolsPayload,
};
use crate::tools::ToolSpec;
use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// Hostname prefix for the Bedrock Runtime endpoint.
const ENDPOINT_PREFIX: &str = "bedrock-runtime";
/// SigV4 signing service name (AWS uses "bedrock", not "bedrock-runtime").
const SIGNING_SERVICE: &str = "bedrock";
const DEFAULT_REGION: &str = "us-east-1";
const DEFAULT_MAX_TOKENS: u32 = 4096;

// ── AWS Credentials ─────────────────────────────────────────────

/// Resolved AWS credentials for SigV4 signing.
#[derive(Clone)]
struct AwsCredentials {
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
    region: String,
}

impl AwsCredentials {
    /// Resolve credentials using the standard AWS credential resolution chain:
    /// 1. Environment variables (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`,
    ///    `AWS_SESSION_TOKEN`, `AWS_REGION` / `AWS_DEFAULT_REGION`)
    /// 2. Shared credentials file (`~/.aws/credentials`) and config (`~/.aws/config`)
    ///
    /// This mirrors the standard AWS SDK behavior used across the entire AWS
    /// ecosystem (CI/CD, ECS tasks, Lambda, etc.).
    fn resolve() -> Option<Self> {
        Self::from_env_vars().or_else(Self::from_aws_config_files)
    }

    /// Try to resolve credentials from standard AWS environment variables.
    fn from_env_vars() -> Option<Self> {
        let access_key_id = std::env::var("AWS_ACCESS_KEY_ID")
            .ok()
            .filter(|s| !s.trim().is_empty())?;
        let secret_access_key = std::env::var("AWS_SECRET_ACCESS_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty())?;
        let session_token = std::env::var("AWS_SESSION_TOKEN").ok().filter(|s| !s.trim().is_empty());
        let region = std::env::var("AWS_REGION")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                std::env::var("AWS_DEFAULT_REGION")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            })
            .unwrap_or_else(|| DEFAULT_REGION.to_string());

        Some(Self {
            access_key_id,
            secret_access_key,
            session_token,
            region,
        })
    }

    /// Resolve credentials from the standard AWS shared credentials file
    /// (`~/.aws/credentials`) and config (`~/.aws/config`).
    ///
    /// Falls back to the `[default]` profile. Returns `None` if no valid
    /// credentials can be resolved from config files.
    fn from_aws_config_files() -> Option<Self> {
        let home = crate::config::user_home_dir()?;
        let creds_path = home.join(".aws").join("credentials");
        let config_path = home.join(".aws").join("config");

        let creds_content = std::fs::read_to_string(&creds_path).ok()?;
        let (access_key_id, secret_access_key, session_token) = Self::parse_aws_credentials_file(&creds_content)?;

        let region = std::fs::read_to_string(&config_path)
            .ok()
            .and_then(|content| Self::parse_aws_config_region(&content))
            .unwrap_or_else(|| DEFAULT_REGION.to_string());

        Some(Self {
            access_key_id,
            secret_access_key,
            session_token,
            region,
        })
    }

    /// Parse `~/.aws/credentials` INI for the `[default]` profile.
    fn parse_aws_credentials_file(content: &str) -> Option<(String, String, Option<String>)> {
        let mut in_default = false;
        let mut access_key = None;
        let mut secret_key = None;
        let mut session_token = None;

        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                in_default = trimmed == "[default]";
                continue;
            }
            if !in_default {
                continue;
            }
            if let Some((key, value)) = trimmed.split_once('=') {
                let key = key.trim();
                let value = value.trim();
                if value.is_empty() {
                    continue;
                }
                match key {
                    "aws_access_key_id" => access_key = Some(value.to_string()),
                    "aws_secret_access_key" => secret_key = Some(value.to_string()),
                    "aws_session_token" => session_token = Some(value.to_string()),
                    _ => {}
                }
            }
        }

        Some((access_key?, secret_key?, session_token))
    }

    /// Parse `~/.aws/config` INI for region in the `[default]` profile.
    fn parse_aws_config_region(content: &str) -> Option<String> {
        let mut in_default = false;

        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                in_default = trimmed == "[default]" || trimmed == "[profile default]";
                continue;
            }
            if !in_default {
                continue;
            }
            if let Some((key, value)) = trimmed.split_once('=') {
                if key.trim() == "region" {
                    let value = value.trim();
                    if !value.is_empty() {
                        return Some(value.to_string());
                    }
                }
            }
        }

        None
    }

    fn host(&self) -> String {
        format!("{ENDPOINT_PREFIX}.{}.amazonaws.com", self.region)
    }
}

// ── AWS SigV4 Signing ───────────────────────────────────────────

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(key).map_err(|e| anyhow::anyhow!("HMAC key initialization failed: {e}"))?;
    mac.update(data);
    Ok(mac.finalize().into_bytes().to_vec())
}

/// Derive the SigV4 signing key via HMAC chain.
fn derive_signing_key(secret: &str, date: &str, region: &str, service: &str) -> anyhow::Result<Vec<u8>> {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes())?;
    let k_region = hmac_sha256(&k_date, region.as_bytes())?;
    let k_service = hmac_sha256(&k_region, service.as_bytes())?;
    hmac_sha256(&k_service, b"aws4_request")
}

/// Build the SigV4 `Authorization` header value.
///
/// `headers` must be sorted by lowercase header name.
fn build_authorization_header(
    credentials: &AwsCredentials,
    method: &str,
    canonical_uri: &str,
    query_string: &str,
    headers: &[(String, String)],
    payload: &[u8],
    timestamp: &chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<String> {
    let date_stamp = timestamp.format("%Y%m%d").to_string();
    let amz_date = timestamp.format("%Y%m%dT%H%M%SZ").to_string();

    let mut canonical_headers = String::new();
    for (k, v) in headers {
        canonical_headers.push_str(k);
        canonical_headers.push(':');
        canonical_headers.push_str(v);
        canonical_headers.push('\n');
    }

    let signed_headers: String = headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");

    let payload_hash = sha256_hex(payload);

    let canonical_request =
        format!("{method}\n{canonical_uri}\n{query_string}\n{canonical_headers}\n{signed_headers}\n{payload_hash}");

    let credential_scope = format!("{date_stamp}/{}/{SIGNING_SERVICE}/aws4_request", credentials.region);

    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    let signing_key = derive_signing_key(
        &credentials.secret_access_key,
        &date_stamp,
        &credentials.region,
        SIGNING_SERVICE,
    )?;

    let signature = hex::encode(hmac_sha256(&signing_key, string_to_sign.as_bytes())?);

    Ok(format!(
        "AWS4-HMAC-SHA256 Credential={}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}",
        credentials.access_key_id
    ))
}

// ── Converse API Types (Request) ────────────────────────────────

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConverseRequest {
    messages: Vec<ConverseMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Vec<SystemBlock>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inference_config: Option<InferenceConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_config: Option<ToolConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ConverseMessage {
    role: String,
    content: Vec<ContentBlock>,
}

/// Content blocks use Bedrock's union style:
/// `{"text": "..."}`, `{"toolUse": {...}}`, `{"toolResult": {...}}`, `{"cachePoint": {...}}`.
///
/// Note: `text` is a simple string value, not a nested object. `toolUse` and `toolResult`
/// are nested objects. We use `#[serde(untagged)]` with manual struct wrappers to
/// match this mixed format.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum ContentBlock {
    Text(TextBlock),
    ToolUse(ToolUseWrapper),
    ToolResult(ToolResultWrapper),
    CachePointBlock(CachePointWrapper),
}

#[derive(Debug, Serialize, Deserialize)]
struct TextBlock {
    text: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolUseWrapper {
    tool_use: ToolUseBlock,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolUseBlock {
    tool_use_id: String,
    name: String,
    input: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolResultWrapper {
    tool_result: ToolResultBlock,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolResultBlock {
    tool_use_id: String,
    content: Vec<ToolResultContent>,
    status: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CachePointWrapper {
    cache_point: CachePoint,
}

#[derive(Debug, Serialize, Deserialize)]
struct ToolResultContent {
    text: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachePoint {
    #[serde(rename = "type")]
    cache_type: String,
}

impl CachePoint {
    fn default_cache() -> Self {
        Self {
            cache_type: "default".to_string(),
        }
    }
}

/// System prompt blocks: either `{"text": "..."}` or `{"cachePoint": {...}}`.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum SystemBlock {
    Text(TextBlock),
    CachePoint(CachePointWrapper),
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InferenceConfig {
    max_tokens: u32,
    temperature: f64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolConfig {
    tools: Vec<ToolDefinition>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolDefinition {
    tool_spec: ToolSpecDef,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolSpecDef {
    name: String,
    description: String,
    input_schema: InputSchema,
}

#[derive(Debug, Serialize)]
struct InputSchema {
    json: serde_json::Value,
}

// ── Converse API Types (Response) ───────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConverseResponse {
    #[serde(default)]
    output: Option<ConverseOutput>,
    #[serde(default, rename = "stopReason")]
    _stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ConverseOutput {
    #[serde(default)]
    message: Option<ConverseOutputMessage>,
}

#[derive(Debug, Deserialize)]
struct ConverseOutputMessage {
    #[serde(rename = "role")]
    _role: String,
    content: Vec<ResponseContentBlock>,
}

/// Response content blocks from the Converse API.
///
/// Uses `#[serde(untagged)]` to match Bedrock's union format where `text` is a
/// simple string value and `toolUse` is a nested object. Unknown block types
/// (e.g. `reasoningContent`, `guardContent`) are captured as `Other` to prevent
/// deserialization failures.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ResponseContentBlock {
    ToolUse(ResponseToolUseWrapper),
    Text(TextBlock),
    /// Serde catch-all for block types this adapter does not use; only the
    /// block's kind (its keys) is read, for diagnosis.
    Other(serde_json::Value),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResponseToolUseWrapper {
    tool_use: ToolUseBlock,
}

// ── Bedrock ConverseStream event-stream decoding ────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
struct EventStreamMessage {
    event_type: Option<String>,
    message_type: Option<String>,
    payload: Vec<u8>,
}

#[derive(Debug)]
struct EventStreamDecode {
    message: EventStreamMessage,
    consumed: usize,
}

fn read_u32_be(bytes: &[u8]) -> anyhow::Result<u32> {
    let array: [u8; 4] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid eventstream u32 width"))?;
    Ok(u32::from_be_bytes(array))
}

fn eventstream_crc(bytes: &[u8]) -> u32 {
    crc32fast::hash(bytes)
}

fn parse_eventstream_string_header(bytes: &[u8], pos: &mut usize) -> anyhow::Result<String> {
    let len_bytes = bytes
        .get(*pos..*pos + 2)
        .ok_or_else(|| anyhow::anyhow!("truncated eventstream string header length"))?;
    let len = u16::from_be_bytes(
        len_bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid eventstream string header length width"))?,
    ) as usize;
    *pos += 2;
    let value_bytes = bytes
        .get(*pos..*pos + len)
        .ok_or_else(|| anyhow::anyhow!("truncated eventstream string header value"))?;
    let value = std::str::from_utf8(value_bytes)
        .map_err(|err| anyhow::anyhow!("invalid eventstream string header utf8: {err}"))?
        .to_string();
    *pos += len;
    Ok(value)
}

fn skip_eventstream_header_value(bytes: &[u8], pos: &mut usize, value_type: u8) -> anyhow::Result<()> {
    let len = match value_type {
        0 | 1 => 0,
        2 => 1,
        3 => 2,
        4 => 4,
        5 | 8 => 8,
        6 | 7 => {
            let len_bytes = bytes
                .get(*pos..*pos + 2)
                .ok_or_else(|| anyhow::anyhow!("truncated eventstream variable header length"))?;
            let len = u16::from_be_bytes(
                len_bytes
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("invalid eventstream variable header length width"))?,
            ) as usize;
            *pos += 2;
            len
        }
        9 => 16,
        other => anyhow::bail!("unsupported eventstream header value type {other}"),
    };
    if bytes.len().saturating_sub(*pos) < len {
        anyhow::bail!("truncated eventstream header value");
    }
    *pos += len;
    Ok(())
}

fn parse_eventstream_headers(bytes: &[u8]) -> anyhow::Result<(Option<String>, Option<String>)> {
    let mut pos = 0;
    let mut event_type = None;
    let mut message_type = None;

    while pos < bytes.len() {
        let name_len = usize::from(
            *bytes
                .get(pos)
                .ok_or_else(|| anyhow::anyhow!("truncated eventstream header name length"))?,
        );
        pos += 1;
        let name_bytes = bytes
            .get(pos..pos + name_len)
            .ok_or_else(|| anyhow::anyhow!("truncated eventstream header name"))?;
        let name = std::str::from_utf8(name_bytes)
            .map_err(|err| anyhow::anyhow!("invalid eventstream header name utf8: {err}"))?;
        pos += name_len;
        let value_type = *bytes
            .get(pos)
            .ok_or_else(|| anyhow::anyhow!("truncated eventstream header value type"))?;
        pos += 1;

        if value_type == 7 && (name == ":event-type" || name == ":message-type") {
            let value = parse_eventstream_string_header(bytes, &mut pos)?;
            if name == ":event-type" {
                event_type = Some(value);
            } else {
                message_type = Some(value);
            }
        } else {
            skip_eventstream_header_value(bytes, &mut pos, value_type)?;
        }
    }

    Ok((event_type, message_type))
}

fn try_parse_eventstream_message(buffer: &[u8]) -> anyhow::Result<Option<EventStreamDecode>> {
    if buffer.len() < 12 {
        return Ok(None);
    }

    let total_len = read_u32_be(
        buffer
            .get(0..4)
            .ok_or_else(|| anyhow::anyhow!("truncated eventstream total length"))?,
    )? as usize;
    let headers_len = read_u32_be(
        buffer
            .get(4..8)
            .ok_or_else(|| anyhow::anyhow!("truncated eventstream headers length"))?,
    )? as usize;
    if total_len < 16 || headers_len > total_len.saturating_sub(16) {
        anyhow::bail!("invalid eventstream prelude lengths");
    }
    if buffer.len() < total_len {
        return Ok(None);
    }

    let expected_prelude_crc = read_u32_be(
        buffer
            .get(8..12)
            .ok_or_else(|| anyhow::anyhow!("truncated eventstream prelude crc"))?,
    )?;
    let actual_prelude_crc = eventstream_crc(
        buffer
            .get(0..8)
            .ok_or_else(|| anyhow::anyhow!("truncated eventstream prelude"))?,
    );
    if expected_prelude_crc != actual_prelude_crc {
        anyhow::bail!("invalid eventstream prelude crc");
    }

    let expected_message_crc = read_u32_be(
        buffer
            .get(total_len - 4..total_len)
            .ok_or_else(|| anyhow::anyhow!("truncated eventstream message crc"))?,
    )?;
    let actual_message_crc = eventstream_crc(
        buffer
            .get(0..total_len - 4)
            .ok_or_else(|| anyhow::anyhow!("truncated eventstream message"))?,
    );
    if expected_message_crc != actual_message_crc {
        anyhow::bail!("invalid eventstream message crc");
    }

    let headers_start = 12;
    let headers_end = headers_start + headers_len;
    let payload_end = total_len - 4;
    let header_bytes = buffer
        .get(headers_start..headers_end)
        .ok_or_else(|| anyhow::anyhow!("truncated eventstream headers"))?;
    let payload = buffer
        .get(headers_end..payload_end)
        .ok_or_else(|| anyhow::anyhow!("truncated eventstream payload"))?;
    let (event_type, message_type) = parse_eventstream_headers(header_bytes)?;
    Ok(Some(EventStreamDecode {
        message: EventStreamMessage {
            event_type,
            message_type,
            payload: payload.to_vec(),
        },
        consumed: total_len,
    }))
}

#[derive(Debug, Default)]
struct BedrockToolCallBuffer {
    id: String,
    name: String,
    args: String,
    order: usize,
}

#[derive(Debug, Default)]
struct BedrockStreamState {
    tools: HashMap<usize, BedrockToolCallBuffer>,
    next_tool_order: usize,
}

impl BedrockStreamState {
    fn start_tool(&mut self, block_index: usize, id: String, name: String) -> Option<ToolCallChunk> {
        if id.is_empty() || name.is_empty() {
            return None;
        }
        let order = self.next_tool_order;
        self.next_tool_order += 1;
        self.tools.insert(
            block_index,
            BedrockToolCallBuffer {
                id: id.clone(),
                name: name.clone(),
                args: String::new(),
                order,
            },
        );
        Some(ToolCallChunk::streaming_delta(id, name, "", order))
    }

    fn push_tool_delta(&mut self, block_index: usize, delta: String) -> Option<ToolCallChunk> {
        let tool = self.tools.get_mut(&block_index)?;
        tool.args.push_str(&delta);
        Some(ToolCallChunk::streaming_delta(
            tool.id.clone(),
            tool.name.clone(),
            delta,
            tool.order,
        ))
    }

    fn finish_tool(&mut self, block_index: usize) -> Option<ToolCallChunk> {
        let tool = self.tools.remove(&block_index)?;
        Some(ToolCallChunk::new(tool.id, tool.name, tool.args, tool.order))
    }
}

fn bedrock_event_error_message(event_type: &str, payload: &[u8]) -> String {
    let value = serde_json::from_slice::<serde_json::Value>(payload).unwrap_or_default();
    let message = value
        .get("message")
        .and_then(serde_json::Value::as_str)
        .or_else(|| value.get("Message").and_then(serde_json::Value::as_str))
        .unwrap_or("Bedrock stream error");
    // The event type is upstream text too, so the message starts with PRX's
    // own prefix (D-069 L-2).
    format!(
        "Bedrock stream error ({}): {}",
        super::sanitize_api_error(event_type),
        super::sanitize_api_error(message)
    )
}

fn bedrock_reasoning_delta(delta: &serde_json::Value) -> Option<String> {
    delta
        .get("reasoningContent")
        .and_then(|reasoning| {
            reasoning.get("text").and_then(serde_json::Value::as_str).or_else(|| {
                reasoning
                    .get("reasoningText")
                    .and_then(|value| value.get("text"))
                    .and_then(serde_json::Value::as_str)
            })
        })
        .filter(|text| !text.is_empty())
        .map(ToString::to_string)
}

async fn handle_bedrock_stream_message(
    message: EventStreamMessage,
    state: &mut BedrockStreamState,
    tx: &tokio::sync::mpsc::Sender<StreamResult<StreamChunk>>,
    count_tokens: bool,
) -> bool {
    if message.message_type.as_deref() == Some("exception") {
        let event = message.event_type.as_deref().unwrap_or("exception");
        let _ = tx
            .send(Err(StreamError::Provider(bedrock_event_error_message(
                event,
                &message.payload,
            ))))
            .await;
        return true;
    }

    let Some(event_type) = message.event_type.as_deref() else {
        return false;
    };

    if event_type.ends_with("Exception") {
        let _ = tx
            .send(Err(StreamError::Provider(bedrock_event_error_message(
                event_type,
                &message.payload,
            ))))
            .await;
        return true;
    }

    let payload = match serde_json::from_slice::<serde_json::Value>(&message.payload) {
        Ok(value) => value,
        Err(err) => {
            let _ = tx.send(Err(StreamError::Json(err))).await;
            return true;
        }
    };

    match event_type {
        "contentBlockStart" => {
            let block_index = payload
                .get("contentBlockIndex")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0) as usize;
            if let Some(tool_use) = payload.get("start").and_then(|start| start.get("toolUse")) {
                let id = tool_use
                    .get("toolUseId")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let name = tool_use
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if let Some(call) = state.start_tool(block_index, id, name)
                    && tx.send(Ok(StreamChunk::tool_call_chunk(vec![call]))).await.is_err()
                {
                    return true;
                }
            }
        }
        "contentBlockDelta" => {
            let block_index = payload
                .get("contentBlockIndex")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0) as usize;
            let Some(delta) = payload.get("delta") else {
                return false;
            };

            if let Some(text) = delta
                .get("text")
                .and_then(serde_json::Value::as_str)
                .filter(|text| !text.is_empty())
            {
                let mut chunk = StreamChunk::delta(text.to_string());
                if count_tokens {
                    chunk = chunk.with_token_estimate();
                }
                if tx.send(Ok(chunk)).await.is_err() {
                    return true;
                }
            }
            if let Some(reasoning) = bedrock_reasoning_delta(delta) {
                let mut chunk = StreamChunk::reasoning_delta(reasoning);
                if count_tokens {
                    chunk = chunk.with_token_estimate();
                }
                if tx.send(Ok(chunk)).await.is_err() {
                    return true;
                }
            }
            if let Some(input) = delta
                .get("toolUse")
                .and_then(|tool_use| tool_use.get("input"))
                .and_then(serde_json::Value::as_str)
                .filter(|input| !input.is_empty())
                && let Some(call) = state.push_tool_delta(block_index, input.to_string())
                && tx.send(Ok(StreamChunk::tool_call_chunk(vec![call]))).await.is_err()
            {
                return true;
            }
        }
        "contentBlockStop" => {
            let block_index = payload
                .get("contentBlockIndex")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0) as usize;
            if let Some(call) = state.finish_tool(block_index)
                && tx.send(Ok(StreamChunk::tool_call_chunk(vec![call]))).await.is_err()
            {
                return true;
            }
        }
        "messageStop" => {
            let _ = tx.send(Ok(StreamChunk::final_chunk())).await;
            return true;
        }
        _ => {}
    }

    false
}

// ── BedrockProvider ─────────────────────────────────────────────

pub struct BedrockProvider {
    credentials: Option<AwsCredentials>,
}

impl BedrockProvider {
    pub fn new() -> Self {
        Self {
            credentials: AwsCredentials::resolve(),
        }
    }

    fn http_client(&self) -> Client {
        crate::config::build_runtime_proxy_client_with_timeouts("provider.bedrock", 120, 10)
            .map_err(|e| {
                tracing::error!("proxy build failed for provider.bedrock, using direct: {e}");
                e
            })
            .unwrap_or_else(|_| crate::prx_http_client())
    }

    /// Percent-encode the model ID for URL path: only encode `:` to `%3A`.
    /// Colons in model IDs (e.g. `v1:0`) must be encoded because `reqwest::Url`
    /// may misparse them. Dots, hyphens, and alphanumerics are safe.
    fn encode_model_path(model_id: &str) -> String {
        model_id.replace(':', "%3A")
    }

    /// Build the actual request URL. Uses raw model ID (reqwest sends colons as-is).
    fn endpoint_url(region: &str, model_id: &str) -> String {
        format!("https://{ENDPOINT_PREFIX}.{region}.amazonaws.com/model/{model_id}/converse")
    }

    fn stream_endpoint_url(region: &str, model_id: &str) -> String {
        format!("https://{ENDPOINT_PREFIX}.{region}.amazonaws.com/model/{model_id}/converse-stream")
    }

    /// Build the canonical URI for SigV4 signing. Must URI-encode the path
    /// per SigV4 spec: colons become `%3A`. AWS verifies the signature against
    /// the encoded form even though the wire request uses raw colons.
    fn canonical_uri(model_id: &str) -> String {
        let encoded = Self::encode_model_path(model_id);
        format!("/model/{encoded}/converse")
    }

    fn stream_canonical_uri(model_id: &str) -> String {
        let encoded = Self::encode_model_path(model_id);
        format!("/model/{encoded}/converse-stream")
    }

    fn require_credentials(&self) -> anyhow::Result<&AwsCredentials> {
        self.credentials.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "AWS Bedrock credentials not found. Set AWS_ACCESS_KEY_ID + \
                 AWS_SECRET_ACCESS_KEY env vars, or configure ~/.aws/credentials."
            )
        })
    }

    // ── Cache heuristics (same thresholds as AnthropicProvider) ──

    /// Cache system prompts larger than ~1024 tokens (3KB of text).
    const fn should_cache_system(text: &str) -> bool {
        text.len() > 3072
    }

    /// Cache conversations with more than 4 messages (excluding system).
    fn should_cache_conversation(messages: &[ChatMessage]) -> bool {
        messages.iter().filter(|m| m.role != "system").count() > 4
    }

    // ── Message conversion ──────────────────────────────────────

    fn convert_messages(messages: &[ChatMessage]) -> (Option<Vec<SystemBlock>>, Vec<ConverseMessage>) {
        let mut system_blocks = Vec::new();
        let mut converse_messages = Vec::new();

        for msg in messages {
            match msg.role.as_str() {
                "system" => {
                    if system_blocks.is_empty() {
                        system_blocks.push(SystemBlock::Text(TextBlock {
                            text: msg.content.clone(),
                        }));
                    }
                }
                "assistant" => {
                    if let Some(blocks) = Self::parse_assistant_tool_call_message(&msg.content) {
                        converse_messages.push(ConverseMessage {
                            role: "assistant".to_string(),
                            content: blocks,
                        });
                    } else {
                        converse_messages.push(ConverseMessage {
                            role: "assistant".to_string(),
                            content: vec![ContentBlock::Text(TextBlock {
                                text: msg.content.clone(),
                            })],
                        });
                    }
                }
                "tool" => {
                    if let Some(tool_result_msg) = Self::parse_tool_result_message(&msg.content) {
                        converse_messages.push(tool_result_msg);
                    } else {
                        converse_messages.push(ConverseMessage {
                            role: "user".to_string(),
                            content: vec![ContentBlock::Text(TextBlock {
                                text: msg.content.clone(),
                            })],
                        });
                    }
                }
                _ => {
                    converse_messages.push(ConverseMessage {
                        role: "user".to_string(),
                        content: vec![ContentBlock::Text(TextBlock {
                            text: msg.content.clone(),
                        })],
                    });
                }
            }
        }

        let system = if system_blocks.is_empty() {
            None
        } else {
            Some(system_blocks)
        };
        (system, converse_messages)
    }

    /// Parse assistant message containing structured tool calls.
    fn parse_assistant_tool_call_message(content: &str) -> Option<Vec<ContentBlock>> {
        let value = serde_json::from_str::<serde_json::Value>(content).ok()?;
        let tool_calls = value
            .get("tool_calls")
            .and_then(|v| serde_json::from_value::<Vec<ProviderToolCall>>(v.clone()).ok())?;

        let mut blocks = Vec::new();
        if let Some(text) = value
            .get("content")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            blocks.push(ContentBlock::Text(TextBlock { text: text.to_string() }));
        }
        for call in tool_calls {
            let input = serde_json::from_str::<serde_json::Value>(&call.arguments)
                .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
            blocks.push(ContentBlock::ToolUse(ToolUseWrapper {
                tool_use: ToolUseBlock {
                    tool_use_id: call.id,
                    name: call.name,
                    input,
                },
            }));
        }
        Some(blocks)
    }

    /// Parse tool result message into a user message with ToolResult block.
    fn parse_tool_result_message(content: &str) -> Option<ConverseMessage> {
        let value = serde_json::from_str::<serde_json::Value>(content).ok()?;
        let tool_use_id = value
            .get("tool_call_id")
            .and_then(serde_json::Value::as_str)?
            .to_string();
        let result = value
            .get("content")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        Some(ConverseMessage {
            role: "user".to_string(),
            content: vec![ContentBlock::ToolResult(ToolResultWrapper {
                tool_result: ToolResultBlock {
                    tool_use_id,
                    content: vec![ToolResultContent { text: result }],
                    status: "success".to_string(),
                },
            })],
        })
    }

    // ── Tool conversion ─────────────────────────────────────────

    /// Lower every published tool schema to what this endpoint was declared to
    /// accept, or refuse the request naming the tool that cannot be offered.
    fn convert_tools_to_converse(&self, tools: Option<&[ToolSpec]>) -> anyhow::Result<Option<ToolConfig>> {
        let items = tools.filter(|items| !items.is_empty());
        let Some(items) = items else {
            return Ok(None);
        };
        let dialect = self.tool_schema_dialect();
        let mut tool_defs = Vec::with_capacity(items.len());
        for tool in items {
            let parameters =
                dialect
                    .wire_schema(&tool.name, &tool.parameters)
                    .map_err(|error| super::ProviderCapabilityError {
                        provider: "bedrock".to_string(),
                        capability: "tool_schema_dialect".to_string(),
                        message: error.to_string(),
                    })?;
            tool_defs.push(ToolDefinition {
                tool_spec: ToolSpecDef {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    input_schema: InputSchema {
                        json: parameters.into_owned(),
                    },
                },
            });
        }
        Ok(Some(ToolConfig { tools: tool_defs }))
    }

    // ── Response parsing ────────────────────────────────────────

    fn parse_converse_response(response: ConverseResponse) -> ProviderChatResponse {
        let mut text_parts = Vec::new();
        let mut tool_calls = Vec::new();

        if let Some(output) = response.output {
            if let Some(message) = output.message {
                for block in message.content {
                    match block {
                        ResponseContentBlock::Text(tb) => {
                            let trimmed = tb.text.trim().to_string();
                            if !trimmed.is_empty() {
                                text_parts.push(trimmed);
                            }
                        }
                        ResponseContentBlock::ToolUse(wrapper) => {
                            if !wrapper.tool_use.name.is_empty() {
                                tool_calls.push(ProviderToolCall {
                                    id: wrapper.tool_use.tool_use_id,
                                    name: wrapper.tool_use.name,
                                    arguments: wrapper.tool_use.input.to_string(),
                                });
                            }
                        }
                        ResponseContentBlock::Other(block) => {
                            // Unknown block types are discarded; name their kind, never
                            // their content (a reasoning block carries model text).
                            tracing::debug!(
                                block_kinds = ?block.as_object().map(|object| object.keys().collect::<Vec<_>>()),
                                "bedrock: discarded a response content block of an unknown kind"
                            );
                        }
                    }
                }
            }
        }

        ProviderChatResponse {
            text: if text_parts.is_empty() {
                None
            } else {
                Some(text_parts.join("\n"))
            },
            tool_calls,
            reasoning_content: None,
        }
    }

    // ── HTTP request ────────────────────────────────────────────

    async fn send_converse_request(
        &self,
        credentials: &AwsCredentials,
        model: &str,
        request_body: &ConverseRequest,
    ) -> anyhow::Result<ConverseResponse> {
        let payload = serde_json::to_vec(
            &super::parameter_wire::request("bedrock", super::parameter_wire::Format::Bedrock, model, request_body)
                .with_default_output_limit(),
        )?;
        let url = Self::endpoint_url(&credentials.region, model);
        let canonical_uri = Self::canonical_uri(model);
        let now = chrono::Utc::now();
        let host = credentials.host();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();

        let mut headers_to_sign = vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("host".to_string(), host),
            ("x-amz-date".to_string(), amz_date.clone()),
        ];
        if let Some(ref token) = credentials.session_token {
            headers_to_sign.push(("x-amz-security-token".to_string(), token.clone()));
        }
        headers_to_sign.sort_by(|a, b| a.0.cmp(&b.0));

        let authorization = build_authorization_header(
            credentials,
            "POST",
            &canonical_uri,
            "",
            &headers_to_sign,
            &payload,
            &now,
        )?;

        let mut request = self
            .http_client()
            .post(&url)
            .header("content-type", "application/json")
            .header("x-amz-date", &amz_date)
            .header("authorization", &authorization);

        if let Some(ref token) = credentials.session_token {
            request = request.header("x-amz-security-token", token);
        }

        let response: reqwest::Response = request.body(payload).send().await?;

        if !response.status().is_success() {
            return Err(super::api_error("Bedrock", response).await);
        }

        let converse_response: ConverseResponse = response.json().await?;
        Ok(converse_response)
    }

    async fn send_converse_stream_request(
        client: Client,
        credentials: AwsCredentials,
        model: String,
        request_body: ConverseRequest,
    ) -> anyhow::Result<reqwest::Response> {
        let payload = serde_json::to_vec(
            &super::parameter_wire::request("bedrock", super::parameter_wire::Format::Bedrock, &model, &request_body)
                .with_default_output_limit(),
        )?;
        let url = Self::stream_endpoint_url(&credentials.region, &model);
        let canonical_uri = Self::stream_canonical_uri(&model);
        let now = chrono::Utc::now();
        let host = credentials.host();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();

        let mut headers_to_sign = vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("host".to_string(), host),
            ("x-amz-date".to_string(), amz_date.clone()),
        ];
        if let Some(ref token) = credentials.session_token {
            headers_to_sign.push(("x-amz-security-token".to_string(), token.clone()));
        }
        headers_to_sign.sort_by(|a, b| a.0.cmp(&b.0));

        let authorization = build_authorization_header(
            &credentials,
            "POST",
            &canonical_uri,
            "",
            &headers_to_sign,
            &payload,
            &now,
        )?;

        let mut request = client
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", "application/vnd.amazon.eventstream")
            .header("x-amz-date", &amz_date)
            .header("authorization", &authorization);

        if let Some(ref token) = credentials.session_token {
            request = request.header("x-amz-security-token", token);
        }

        // FIX-P0-33: return the raw response (success or not) so the streaming
        // caller can read the `Retry-After` header off a 429/503 before the body
        // is consumed. The non-streaming callers still get an error via the
        // status check the caller performs.
        let response = request.body(payload).send().await?;
        Ok(response)
    }

    fn build_converse_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolSpec]>,
        temperature: f64,
    ) -> anyhow::Result<ConverseRequest> {
        let (system_blocks, mut converse_messages) = Self::convert_messages(messages);

        let system = system_blocks.map(|mut blocks| {
            let has_large_system = blocks
                .iter()
                .any(|b| matches!(b, SystemBlock::Text(tb) if Self::should_cache_system(&tb.text)));
            if has_large_system {
                blocks.push(SystemBlock::CachePoint(CachePointWrapper {
                    cache_point: CachePoint::default_cache(),
                }));
            }
            blocks
        });

        if Self::should_cache_conversation(messages)
            && let Some(last_msg) = converse_messages.last_mut()
        {
            last_msg.content.push(ContentBlock::CachePointBlock(CachePointWrapper {
                cache_point: CachePoint::default_cache(),
            }));
        }

        Ok(ConverseRequest {
            system,
            messages: converse_messages,
            inference_config: Some(InferenceConfig {
                max_tokens: DEFAULT_MAX_TOKENS,
                temperature,
            }),
            tool_config: self.convert_tools_to_converse(tools)?,
        })
    }
}

// ── Provider trait implementation ───────────────────────────────

#[async_trait]
impl Provider for BedrockProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            vision: true,
        }
    }

    fn supports_native_tools(&self) -> bool {
        true
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn convert_tools(&self, tools: &[ToolSpec]) -> anyhow::Result<ToolsPayload> {
        let dialect = self.tool_schema_dialect();
        let mut tool_values = Vec::with_capacity(tools.len());
        for tool in tools {
            let parameters =
                dialect
                    .wire_schema(&tool.name, &tool.parameters)
                    .map_err(|error| super::ProviderCapabilityError {
                        provider: "bedrock".to_string(),
                        capability: "tool_schema_dialect".to_string(),
                        message: error.to_string(),
                    })?;
            tool_values.push(serde_json::json!({
                "toolSpec": {
                    "name": tool.name,
                    "description": tool.description,
                    "inputSchema": { "json": parameters }
                }
            }));
        }
        Ok(ToolsPayload::Anthropic { tools: tool_values })
    }

    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let credentials = self.require_credentials()?;

        let system = system_prompt.map(|text| {
            let mut blocks = vec![SystemBlock::Text(TextBlock { text: text.to_string() })];
            if Self::should_cache_system(text) {
                blocks.push(SystemBlock::CachePoint(CachePointWrapper {
                    cache_point: CachePoint::default_cache(),
                }));
            }
            blocks
        });

        let request = ConverseRequest {
            system,
            messages: vec![ConverseMessage {
                role: "user".to_string(),
                content: vec![ContentBlock::Text(TextBlock {
                    text: message.to_string(),
                })],
            }],
            inference_config: Some(InferenceConfig {
                max_tokens: DEFAULT_MAX_TOKENS,
                temperature,
            }),
            tool_config: None,
        };

        let response = self.send_converse_request(credentials, model, &request).await?;

        Self::parse_converse_response(response)
            .text
            .ok_or_else(|| anyhow::anyhow!("No response from Bedrock"))
    }

    async fn chat(
        &self,
        request: ProviderChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ProviderChatResponse> {
        let credentials = self.require_credentials()?;

        let (system_blocks, mut converse_messages) = Self::convert_messages(request.messages);

        // Apply cachePoint to system if large.
        let system = system_blocks.map(|mut blocks| {
            let has_large_system = blocks
                .iter()
                .any(|b| matches!(b, SystemBlock::Text(tb) if Self::should_cache_system(&tb.text)));
            if has_large_system {
                blocks.push(SystemBlock::CachePoint(CachePointWrapper {
                    cache_point: CachePoint::default_cache(),
                }));
            }
            blocks
        });

        // Apply cachePoint to last message if conversation is long.
        if Self::should_cache_conversation(request.messages) {
            if let Some(last_msg) = converse_messages.last_mut() {
                last_msg.content.push(ContentBlock::CachePointBlock(CachePointWrapper {
                    cache_point: CachePoint::default_cache(),
                }));
            }
        }

        let tool_config = self.convert_tools_to_converse(request.tools)?;

        let converse_request = ConverseRequest {
            system,
            messages: converse_messages,
            inference_config: Some(InferenceConfig {
                max_tokens: DEFAULT_MAX_TOKENS,
                temperature,
            }),
            tool_config,
        };

        let response = self
            .send_converse_request(credentials, model, &converse_request)
            .await?;

        Ok(Self::parse_converse_response(response))
    }

    fn stream_chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
        options: StreamOptions,
    ) -> stream::BoxStream<'static, StreamResult<StreamChunk>> {
        let credentials = match self.require_credentials() {
            Ok(credentials) => credentials.clone(),
            Err(err) => return stream::once(async move { Err(StreamError::Provider(err.to_string())) }).boxed(),
        };
        // Lowered before the connection is opened, so a tool this endpoint
        // cannot be offered ends the turn with the reason, not an upstream 400.
        let request = match self.build_converse_request(messages, options.tools.as_deref(), temperature) {
            Ok(request) => request,
            Err(err) => return stream::once(async move { Err(StreamError::Provider(err.to_string())) }).boxed(),
        };
        let client = self.http_client();
        let model = model.to_string();
        let count_tokens = options.count_tokens;

        let (tx, rx) = tokio::sync::mpsc::channel::<StreamResult<StreamChunk>>(100);
        super::parameter_capture::spawn(async move {
            let response = match Self::send_converse_stream_request(client, credentials, model, request).await {
                Ok(response) => response,
                Err(err) => {
                    let _ = tx.send(Err(StreamError::Provider(err.to_string()))).await;
                    return;
                }
            };
            // FIX-P0-33: surface 429/503 as a structured `RateLimited` (carrying
            // the real `Retry-After` header) so the reliability layer can honor
            // the upstream backoff instead of parsing the error text.
            if !response.status().is_success() {
                let _ = tx.send(Err(super::stream_api_error("Bedrock", response).await)).await;
                return;
            }

            let mut bytes_stream = response.bytes_stream();
            let mut buffer = Vec::<u8>::new();
            let mut state = BedrockStreamState::default();

            while let Some(item) = bytes_stream.next().await {
                let bytes = match item {
                    Ok(bytes) => bytes,
                    Err(err) => {
                        let _ = tx.send(Err(StreamError::from(err))).await;
                        return;
                    }
                };
                buffer.extend_from_slice(&bytes);

                loop {
                    let decoded = match try_parse_eventstream_message(&buffer) {
                        Ok(Some(decoded)) => decoded,
                        Ok(None) => break,
                        Err(err) => {
                            let _ = tx.send(Err(StreamError::InvalidSse(err.to_string()))).await;
                            return;
                        }
                    };
                    buffer.drain(..decoded.consumed);
                    if handle_bedrock_stream_message(decoded.message, &mut state, &tx, count_tokens).await {
                        return;
                    }
                }
            }

            for call in state
                .tools
                .into_values()
                .map(|tool| ToolCallChunk::new(tool.id, tool.name, tool.args, tool.order))
            {
                if tx.send(Ok(StreamChunk::tool_call_chunk(vec![call]))).await.is_err() {
                    return;
                }
            }
            let _ = tx.send(Ok(StreamChunk::final_chunk())).await;
        });

        stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|chunk| (chunk, rx)) }).boxed()
    }

    fn stream_chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
        options: StreamOptions,
    ) -> stream::BoxStream<'static, StreamResult<StreamChunk>> {
        let mut messages = Vec::new();
        if let Some(system) = system_prompt {
            messages.push(ChatMessage::system(system));
        }
        messages.push(ChatMessage::user(message));
        self.stream_chat_with_history(&messages, model, temperature, options)
    }

    async fn warmup(&self) -> anyhow::Result<()> {
        if let Some(ref creds) = self.credentials {
            let url = format!("https://{ENDPOINT_PREFIX}.{}.amazonaws.com/", creds.region);
            let _ = self.http_client().get(&url).send().await;
        }
        Ok(())
    }
}

// ── Tests ───────────────────────────────────────────────────────

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests;
