use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

pub const JSONRPC_VERSION: &str = "2.0";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl JsonRpcRequest {
    pub fn new(id: String, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            method: method.into(),
            params,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    pub fn success(id: String, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn failure(id: String, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecShellParams {
    pub cmd: String,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
    #[serde(default, rename = "async")]
    pub async_exec: Option<bool>,
    #[serde(default)]
    pub callback_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadFileParams {
    pub path: String,
    #[serde(default)]
    pub offset: Option<u64>,
    #[serde(default)]
    pub limit: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteFileParams {
    pub path: String,
    pub content: String,
    #[serde(default)]
    pub create_dirs: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelParams {
    pub task_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecShellResult {
    pub task_id: String,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    pub timed_out: bool,
    pub cancelled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsyncTaskAccepted {
    pub task_id: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskStatusParams {
    pub task_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskStatusResult {
    pub task_id: String,
    pub status: String,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timed_out: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskListItem {
    pub task_id: String,
    pub status: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskListResult {
    pub tasks: Vec<TaskListItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadFileResult {
    pub path: String,
    pub content: String,
    pub bytes_read: usize,
    pub offset: u64,
    pub eof: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteFileResult {
    pub path: String,
    pub bytes_written: usize,
    pub created_dirs: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PingResult {
    pub message: String,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsResult {
    pub timestamp: DateTime<Utc>,
    pub cpu_cores: usize,
    pub load_avg_1m: Option<f64>,
    pub mem_total_kb: Option<u64>,
    pub mem_available_kb: Option<u64>,
    pub uptime_seconds: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── JsonRpcRequest ──────────────────────────────────────────

    #[test]
    fn request_new_sets_version() {
        let req = JsonRpcRequest::new("1".into(), "ping", serde_json::json!({}));
        assert_eq!(req.jsonrpc, "2.0");
        assert_eq!(req.method, "ping");
        assert_eq!(req.id, "1");
    }

    #[test]
    fn request_serializes_to_json() {
        let req = JsonRpcRequest::new("42".into(), "exec", serde_json::json!({"cmd": "ls"}));
        let json = serde_json::to_value(&req).expect("test: serialize");
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["method"], "exec");
        assert_eq!(json["params"]["cmd"], "ls");
    }

    #[test]
    fn request_roundtrip() {
        let req = JsonRpcRequest::new("5".into(), "read", serde_json::json!({"path": "/etc/hosts"}));
        let json_str = serde_json::to_string(&req).expect("test: ser");
        let restored: JsonRpcRequest = serde_json::from_str(&json_str).expect("test: deser");
        assert_eq!(restored.id, "5");
        assert_eq!(restored.method, "read");
    }

    // ── JsonRpcResponse ─────────────────────────────────────────

    #[test]
    fn response_success_contains_result() {
        let response = JsonRpcResponse::success("id1".into(), serde_json::json!({"ok": true}));
        assert!(response.error.is_none());
        assert_eq!(response.result.expect("test: result")["ok"], true);
    }

    #[test]
    fn response_failure_contains_error() {
        let response = JsonRpcResponse::failure("id2".into(), -32600, "invalid request");
        assert!(response.result.is_none());
        let err = response.error.expect("test: error");
        assert_eq!(err.code, -32600);
        assert_eq!(err.message, "invalid request");
    }

    #[test]
    fn response_success_omits_error_in_json() {
        let response = JsonRpcResponse::success("1".into(), serde_json::json!(null));
        let json = serde_json::to_value(&response).expect("test: ser");
        assert!(json.get("error").is_none());
    }

    #[test]
    fn response_failure_omits_result_in_json() {
        let response = JsonRpcResponse::failure("1".into(), -1, "err");
        let json = serde_json::to_value(&response).expect("test: ser");
        assert!(json.get("result").is_none());
    }

    // ── Params/Result roundtrips ────────────────────────────────

    #[test]
    fn exec_shell_params_roundtrip() {
        let params = ExecShellParams {
            cmd: "ls -la".into(),
            timeout_ms: Some(5000),
            cwd: Some("/tmp".into()),
            env: None,
            async_exec: None,
            callback_url: None,
        };
        let json = serde_json::to_string(&params).expect("test: ser");
        let restored: ExecShellParams = serde_json::from_str(&json).expect("test: deser");
        assert_eq!(restored.cmd, "ls -la");
        assert_eq!(restored.timeout_ms, Some(5000));
    }

    #[test]
    fn exec_shell_result_roundtrip() {
        let result = ExecShellResult {
            task_id: "t1".into(),
            exit_code: Some(0),
            stdout: "hello".into(),
            stderr: String::new(),
            duration_ms: 42,
            timed_out: false,
            cancelled: false,
        };
        let json = serde_json::to_string(&result).expect("test: ser");
        let restored: ExecShellResult = serde_json::from_str(&json).expect("test: deser");
        assert_eq!(restored.task_id, "t1");
        assert!(!restored.timed_out);
    }

    #[test]
    fn read_file_result_roundtrip() {
        let result = ReadFileResult {
            path: "/etc/hosts".into(),
            content: "127.0.0.1 localhost".into(),
            bytes_read: 19,
            offset: 0,
            eof: true,
        };
        let json = serde_json::to_string(&result).expect("test: ser");
        let restored: ReadFileResult = serde_json::from_str(&json).expect("test: deser");
        assert!(restored.eof);
        assert_eq!(restored.bytes_read, 19);
    }

    #[test]
    fn write_file_result_roundtrip() {
        let result = WriteFileResult {
            path: "/tmp/test.txt".into(),
            bytes_written: 5,
            created_dirs: true,
        };
        let json = serde_json::to_string(&result).expect("test: ser");
        let restored: WriteFileResult = serde_json::from_str(&json).expect("test: deser");
        assert!(restored.created_dirs);
    }

    #[test]
    fn ping_result_roundtrip() {
        let result = PingResult {
            message: "pong".into(),
            timestamp: Utc::now(),
        };
        let json = serde_json::to_string(&result).expect("test: ser");
        let restored: PingResult = serde_json::from_str(&json).expect("test: deser");
        assert_eq!(restored.message, "pong");
    }
}
