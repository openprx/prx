//! JSON-RPC 2.0 envelopes and ramflux bridge payloads (protocol version 1).
//!
//! Payloads the agent-host sends use `deny_unknown_fields`. prx never learns
//! nodes, gateways, or ramflux keys; these structs are the whole contract.

use super::super::rich::{
    AttachmentRef, Chat, ControlKind, Sender, SurfaceAction, SurfaceBlock, SurfaceOp, ToolEventKind, TurnError,
    TurnState,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::fmt;

pub const PROTOCOL_VERSION: &str = "1";
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

pub const METHOD_HELLO: &str = "bridge.hello";
pub const METHOD_PING: &str = "bridge.ping";
pub const METHOD_INBOUND_MESSAGE: &str = "inbound.message";
pub const METHOD_INBOUND_SURFACE: &str = "inbound.surface_action";
pub const METHOD_INBOUND_TASK: &str = "inbound.task_event";
pub const METHOD_INBOUND_CONTROL: &str = "inbound.control";
pub const METHOD_INBOUND_DELETED: &str = "inbound.message_deleted";
pub const METHOD_APPROVAL_DECISION: &str = "approval.decision";
pub const METHOD_TURN_CANCEL: &str = "turn.cancel";

pub const METHOD_REPLY_TEXT: &str = "reply.text";
pub const METHOD_REPLY_SURFACE: &str = "reply.surface";
pub const METHOD_REPLY_TASK_UPDATE: &str = "reply.task_update";
pub const METHOD_REPLY_TASK_PROPOSE: &str = "reply.task_propose";
pub const METHOD_REPLY_ATTACHMENT: &str = "reply.attachment";
pub const METHOD_REPLY_TYPING: &str = "reply.typing";
pub const METHOD_TOOL_EVENT: &str = "tool.event";
pub const METHOD_APPROVAL_REQUEST: &str = "approval.request";
pub const METHOD_ATTACHMENT_FETCH: &str = "attachment.fetch";
pub const METHOD_TURN_STATUS: &str = "turn.status";

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("bridge frame is not valid json")]
    Json(#[source] serde_json::Error),
    #[error("bridge frame is not a json object")]
    NotObject,
    #[error("bridge frame has an unknown field")]
    UnknownField,
    #[error("jsonrpc version must be \"2.0\"")]
    Version,
    #[error("bridge frame is not a request, notification, or response")]
    Shape,
    #[error("bridge frame id is null")]
    NullId,
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RpcId {
    Number(u64),
    String(String),
}

impl fmt::Display for RpcId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(id) => write!(f, "{id}"),
            Self::String(id) => write!(f, "{id}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RpcErrorBody {
    pub code: i64,
    pub message: String,
}

impl RpcErrorBody {
    #[must_use]
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Envelope {
    Request {
        id: RpcId,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    Response {
        id: RpcId,
        body: Result<Value, RpcErrorBody>,
    },
}

/// Parse one JSON-RPC frame. The agent-host and prx both send these.
///
/// # Errors
///
/// Returns [`ProtocolError`] when the bytes are not a JSON-RPC 2.0 object.
pub fn parse_envelope(bytes: &[u8]) -> Result<Envelope, ProtocolError> {
    let value: Value = serde_json::from_slice(bytes).map_err(ProtocolError::Json)?;
    let Value::Object(map) = value else {
        return Err(ProtocolError::NotObject);
    };
    for key in map.keys() {
        if !matches!(
            key.as_str(),
            "jsonrpc" | "id" | "method" | "params" | "result" | "error"
        ) {
            return Err(ProtocolError::UnknownField);
        }
    }
    let version = map.get("jsonrpc").and_then(Value::as_str).unwrap_or("");
    if version != "2.0" {
        return Err(ProtocolError::Version);
    }
    let id = match map.get("id") {
        Some(Value::Null) => return Err(ProtocolError::NullId),
        Some(value) => Some(serde_json::from_value(value.clone()).map_err(ProtocolError::Json)?),
        None => None,
    };
    let method = map.get("method").and_then(Value::as_str).map(str::to_string);
    let params = map.get("params").cloned().unwrap_or_else(|| Value::Object(Map::new()));
    if let Some(method) = method {
        if let Some(id) = id {
            return Ok(Envelope::Request { id, method, params });
        }
        return Ok(Envelope::Notification { method, params });
    }
    let Some(id) = id else {
        return Err(ProtocolError::Shape);
    };
    if let Some(error) = map.get("error") {
        let body = serde_json::from_value(error.clone()).map_err(ProtocolError::Json)?;
        return Ok(Envelope::Response { id, body: Err(body) });
    }
    if let Some(result) = map.get("result") {
        return Ok(Envelope::Response {
            id,
            body: Ok(result.clone()),
        });
    }
    Err(ProtocolError::Shape)
}

/// Encode a request. `id` is the correlation token.
///
/// # Errors
///
/// Returns [`ProtocolError`] when `params` cannot be serialized.
pub fn encode_bridge_call(id: &RpcId, method: &str, params: &Value) -> Result<Vec<u8>, ProtocolError> {
    encode(&RequestWire {
        jsonrpc: "2.0",
        id: id.clone(),
        method,
        params,
    })
}

/// Encode a success response.
///
/// # Errors
///
/// Returns [`ProtocolError`] when `result` cannot be serialized.
pub fn encode_result(id: &RpcId, result: &Value) -> Result<Vec<u8>, ProtocolError> {
    encode(&ResultWire {
        jsonrpc: "2.0",
        id: id.clone(),
        result,
    })
}

/// Encode an error response.
///
/// # Errors
///
/// Returns [`ProtocolError`] when the error body cannot be serialized.
pub fn encode_error(id: &RpcId, error: &RpcErrorBody) -> Result<Vec<u8>, ProtocolError> {
    encode(&ErrorWire {
        jsonrpc: "2.0",
        id: id.clone(),
        error,
    })
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, ProtocolError> {
    serde_json::to_vec(value).map_err(ProtocolError::Json)
}

#[derive(Serialize)]
struct RequestWire<'a> {
    jsonrpc: &'a str,
    id: RpcId,
    method: &'a str,
    params: &'a Value,
}

#[derive(Serialize)]
struct ResultWire<'a> {
    jsonrpc: &'a str,
    id: RpcId,
    result: &'a Value,
}

#[derive(Serialize)]
struct ErrorWire<'a> {
    jsonrpc: &'a str,
    id: RpcId,
    error: &'a RpcErrorBody,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HelloAuthorization {
    Full,
    Supervised,
}

impl HelloAuthorization {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Supervised => "supervised",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeHello {
    pub protocol_version: String,
    pub agent_account_id: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub authorization: HelloAuthorization,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BridgeHelloAck {
    pub ok: bool,
    pub prx_version: String,
    pub effective_autonomy: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InboundMessageParams {
    pub conversation_id: String,
    pub message_id: String,
    pub sender: Sender,
    pub chat: Chat,
    pub text: String,
    #[serde(default)]
    pub attachments: Vec<AttachmentRef>,
    #[serde(default, rename = "reply_to")]
    pub reply_target: Option<String>,
    pub timestamp_ms: u64,
    #[serde(default)]
    pub turn_id: Option<String>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurfaceActionParams {
    pub conversation_id: String,
    pub sender: Sender,
    pub surface_id: String,
    pub version: u64,
    pub surface_hash: String,
    pub action_id: String,
    #[serde(default)]
    pub values: Value,
    pub args_hash: String,
    pub message_id: String,
    #[serde(default)]
    pub turn_id: Option<String>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskEventParams {
    pub conversation_id: String,
    pub sender: Sender,
    pub task_id: String,
    pub version: u64,
    pub kind: String,
    #[serde(default)]
    pub body: Value,
    #[serde(default)]
    pub turn_id: Option<String>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlParams {
    pub conversation_id: String,
    pub kind: ControlKind,
    #[serde(default)]
    pub actor: Option<Sender>,
    pub timestamp_ms: u64,
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageDeletedParams {
    pub conversation_id: String,
    pub message_id: String,
    pub deleted_by: Sender,
    pub timestamp_ms: u64,
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalDecisionKind {
    Approved,
    Denied,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalDecisionParams {
    pub conversation_id: String,
    pub approval_id: String,
    pub decision: ApprovalDecisionKind,
    #[serde(default)]
    pub grant_id: Option<String>,
    pub decided_by: Sender,
    pub decided_at_ms: u64,
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnCancelParams {
    pub conversation_id: String,
    pub turn_id: String,
    pub reason: String,
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReplyTextParams<'a> {
    pub conversation_id: &'a str,
    pub turn_id: &'a str,
    pub text: &'a str,
    #[serde(rename = "final")]
    pub is_final: bool,
    #[serde(rename = "reply_to", skip_serializing_if = "Option::is_none")]
    pub reply_target: Option<&'a str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReplySurfaceParams<'a> {
    pub conversation_id: &'a str,
    pub turn_id: &'a str,
    pub op: SurfaceOp,
    pub surface_id: &'a str,
    pub version: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_hash: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocks: Option<&'a [SurfaceBlock]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actions: Option<&'a [SurfaceAction]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ops: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_delta: Option<&'a str>,
    #[serde(rename = "final", skip_serializing_if = "Option::is_none")]
    pub is_final: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_text: Option<&'a str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskStepWire<'a> {
    pub id: &'a str,
    pub title: &'a str,
    pub state: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub needs_me: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReplyTaskUpdateParams<'a> {
    pub conversation_id: &'a str,
    pub turn_id: &'a str,
    pub task_id: &'a str,
    pub version: u64,
    pub state: &'a str,
    pub steps: &'a [TaskStepWire<'a>],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_ref: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_summary: Option<&'a str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReplyTaskProposeParams<'a> {
    pub conversation_id: &'a str,
    pub turn_id: &'a str,
    pub task_id: &'a str,
    pub title: &'a str,
    pub steps: &'a [TaskStepWire<'a>],
}

#[derive(Debug, Clone, Serialize)]
pub struct ReplyAttachmentParams<'a> {
    pub conversation_id: &'a str,
    pub turn_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_b64: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_id: Option<&'a str>,
    pub mime: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<&'a str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReplyTypingParams<'a> {
    pub conversation_id: &'a str,
    pub turn_id: &'a str,
    pub on: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolEventParams<'a> {
    pub conversation_id: &'a str,
    pub turn_id: &'a str,
    pub kind: ToolEventKind,
    pub tool_call_id: &'a str,
    pub tool_name: &'a str,
    pub iteration: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<&'a str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApprovalRequestParams<'a> {
    pub conversation_id: &'a str,
    pub turn_id: &'a str,
    pub approval_id: &'a str,
    pub tool_name: &'a str,
    pub arguments_redacted: &'a Value,
    pub args_hash: &'a str,
    pub risk: &'a str,
    pub expires_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rationale: Option<&'a str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AttachmentFetchParams<'a> {
    pub conversation_id: &'a str,
    pub turn_id: &'a str,
    pub object_id: &'a str,
    pub max_bytes: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentFetchResult {
    pub bytes_b64: String,
    pub mime: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TurnStatusParams<'a> {
    pub conversation_id: &'a str,
    pub turn_id: &'a str,
    pub state: TurnState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<&'a TurnError>,
    /// Present when `state` is `awaiting_approval`. This is the prx decision id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<&'a str>,
}

/// Canonical JSON: object keys sorted, no insignificant whitespace.
///
/// # Errors
///
/// Returns [`ProtocolError`] when a string cannot be encoded.
pub fn canonical_json(value: &Value) -> Result<Vec<u8>, ProtocolError> {
    let mut out = Vec::new();
    write_canonical(&mut out, value)?;
    Ok(out)
}

fn write_canonical(out: &mut Vec<u8>, value: &Value) -> Result<(), ProtocolError> {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Number(number) => out.extend_from_slice(number.to_string().as_bytes()),
        Value::String(text) => {
            let encoded = serde_json::to_vec(text).map_err(ProtocolError::Json)?;
            out.extend_from_slice(&encoded);
        }
        Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_canonical(out, item)?;
            }
            out.push(b']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push(b'{');
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                let encoded = serde_json::to_vec(key).map_err(ProtocolError::Json)?;
                out.extend_from_slice(&encoded);
                out.push(b':');
                let child = map
                    .get(*key)
                    .ok_or_else(|| ProtocolError::Invalid("missing object key".to_string()))?;
                write_canonical(out, child)?;
            }
            out.push(b'}');
        }
    }
    Ok(())
}

/// Lower-hex SHA-256 of `bytes`.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex::encode(digest)
}

/// SHA-256 of the canonical JSON of `arguments`.
///
/// # Errors
///
/// Returns [`ProtocolError`] when canonical encoding fails.
pub fn args_hash(arguments: &Value) -> Result<String, ProtocolError> {
    Ok(sha256_hex(&canonical_json(arguments)?))
}

#[must_use]
pub fn is_reply_method(method: &str) -> bool {
    method.starts_with("reply.")
}

/// JSON-RPC code for a frame that could not be parsed.
#[must_use]
pub const fn parse_error_code(error: &ProtocolError) -> i64 {
    match error {
        ProtocolError::Json(_) => -32700,
        ProtocolError::NullId
        | ProtocolError::NotObject
        | ProtocolError::UnknownField
        | ProtocolError::Version
        | ProtocolError::Shape
        | ProtocolError::Invalid(_) => -32600,
    }
}

/// Error frame with `id: null` for a frame that has no usable id.
#[must_use]
pub fn encode_null_error(code: i64, message: &str) -> Vec<u8> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": null,
        "error": {"code": code, "message": message}
    });
    serde_json::to_vec(&body).unwrap_or_else(|_| Vec::new())
}

pub fn idempotency_key(params: &Value) -> Option<String> {
    params
        .get("idempotency_key")
        .and_then(Value::as_str)
        .map(str::to_string)
}
