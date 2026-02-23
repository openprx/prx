use super::traits::{Tool, ToolResult};
use crate::config::{RemoteNodeConfig, SharedConfig};
use crate::nodes::client::RemoteNodeClient;
use crate::nodes::transport::H2Transport;
use crate::security::SecurityPolicy;
use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

pub struct NodesTool {
    config: SharedConfig,
    security: Arc<SecurityPolicy>,
}

impl NodesTool {
    pub fn new(config: SharedConfig, security: Arc<SecurityPolicy>) -> Self {
        Self { config, security }
    }

    fn require_write_access(&self) -> Option<ToolResult> {
        if !self.security.can_act() {
            return Some(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Action blocked: autonomy is read-only".into()),
            });
        }

        if !self.security.record_action() {
            return Some(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Action blocked: rate limit exceeded".into()),
            });
        }

        None
    }

    fn load_nodes(&self) -> Vec<RemoteNodeConfig> {
        self.config
            .load_full()
            .nodes
            .nodes
            .iter()
            .filter(|node| node.enabled)
            .cloned()
            .collect()
    }

    fn resolve_node<'a>(nodes: &'a [RemoteNodeConfig], id: &str) -> Option<&'a RemoteNodeConfig> {
        nodes.iter().find(|node| node.id == id)
    }

    fn make_client(&self, node: &RemoteNodeConfig) -> anyhow::Result<RemoteNodeClient> {
        let cfg = self.config.load_full();
        let timeout_ms = node
            .timeout_ms
            .unwrap_or(cfg.nodes.request_timeout_ms)
            .max(100);
        let retry_max = node.retry_max.unwrap_or(cfg.nodes.retry_max);

        let transport = Arc::new(H2Transport::new(
            Duration::from_millis(timeout_ms),
            retry_max,
        )?);

        Ok(RemoteNodeClient::new(node.clone(), transport))
    }

    fn require_string_arg<'a>(args: &'a Value, key: &str) -> anyhow::Result<&'a str> {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("Missing or invalid '{key}' parameter"))
    }

    fn optional_u64_arg(args: &Value, key: &str) -> anyhow::Result<Option<u64>> {
        args.get(key)
            .map(|value| {
                value
                    .as_u64()
                    .ok_or_else(|| anyhow!("'{key}' must be an unsigned integer"))
            })
            .transpose()
    }

    fn optional_bool_arg(args: &Value, key: &str) -> anyhow::Result<Option<bool>> {
        args.get(key)
            .map(|value| {
                value
                    .as_bool()
                    .ok_or_else(|| anyhow!("'{key}' must be a boolean"))
            })
            .transpose()
    }
}

#[async_trait]
impl Tool for NodesTool {
    fn name(&self) -> &str {
        "nodes"
    }

    fn description(&self) -> &str {
        "Remote node management over HTTP/2 JSON-RPC. Actions: list, status, exec, read, write, cancel."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "status", "exec", "read", "write", "cancel"],
                    "description": "Action to perform."
                },
                "node": {
                    "type": "string",
                    "description": "Node ID for status/exec/read/write/cancel"
                },
                "command": {
                    "type": "string",
                    "description": "Shell command for exec"
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Timeout override in milliseconds"
                },
                "cwd": {
                    "type": "string",
                    "description": "Working directory for exec"
                },
                "path": {
                    "type": "string",
                    "description": "File path for read/write"
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Read offset"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Read byte limit"
                },
                "content": {
                    "type": "string",
                    "description": "Write content"
                },
                "create_dirs": {
                    "type": "boolean",
                    "description": "Create parent directories when writing"
                },
                "task_id": {
                    "type": "string",
                    "description": "Task ID to cancel"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = Self::require_string_arg(&args, "action")?;
        let nodes = self.load_nodes();

        match action {
            "list" => {
                let items: Vec<Value> = nodes
                    .iter()
                    .map(|node| {
                        json!({
                            "id": node.id,
                            "endpoint": node.endpoint,
                            "enabled": node.enabled,
                        })
                    })
                    .collect();

                Ok(ToolResult {
                    success: true,
                    output: serde_json::to_string_pretty(&json!({
                        "count": items.len(),
                        "nodes": items,
                    }))?,
                    error: None,
                })
            }
            "status" => {
                let node_id = Self::require_string_arg(&args, "node")?;
                let node = Self::resolve_node(&nodes, node_id)
                    .ok_or_else(|| anyhow!("node '{node_id}' not found or disabled"))?;
                let client = self.make_client(node)?;

                let latency = client
                    .ping()
                    .await
                    .with_context(|| format!("ping failed for node '{node_id}'"))?;
                let metrics = client
                    .metrics()
                    .await
                    .with_context(|| format!("metrics failed for node '{node_id}'"))?;

                Ok(ToolResult {
                    success: true,
                    output: serde_json::to_string_pretty(&json!({
                        "node": node_id,
                        "latency_ms": latency.as_millis(),
                        "metrics": metrics,
                    }))?,
                    error: None,
                })
            }
            "exec" => {
                if let Some(blocked) = self.require_write_access() {
                    return Ok(blocked);
                }

                let node_id = Self::require_string_arg(&args, "node")?;
                let command = Self::require_string_arg(&args, "command")?;
                let timeout_ms = Self::optional_u64_arg(&args, "timeout_ms")?;
                let cwd = args.get("cwd").and_then(Value::as_str);

                let node = Self::resolve_node(&nodes, node_id)
                    .ok_or_else(|| anyhow!("node '{node_id}' not found or disabled"))?;
                let client = self.make_client(node)?;
                let result = client.exec_shell(command, timeout_ms, cwd).await?;

                Ok(ToolResult {
                    success: !result.timed_out && !result.cancelled,
                    output: serde_json::to_string_pretty(&result)?,
                    error: None,
                })
            }
            "read" => {
                let node_id = Self::require_string_arg(&args, "node")?;
                let path = Self::require_string_arg(&args, "path")?;
                let offset = Self::optional_u64_arg(&args, "offset")?;
                let limit = Self::optional_u64_arg(&args, "limit")?;

                let node = Self::resolve_node(&nodes, node_id)
                    .ok_or_else(|| anyhow!("node '{node_id}' not found or disabled"))?;
                let client = self.make_client(node)?;
                let result = client.read_file(path, offset, limit).await?;

                Ok(ToolResult {
                    success: true,
                    output: serde_json::to_string_pretty(&result)?,
                    error: None,
                })
            }
            "write" => {
                if let Some(blocked) = self.require_write_access() {
                    return Ok(blocked);
                }

                let node_id = Self::require_string_arg(&args, "node")?;
                let path = Self::require_string_arg(&args, "path")?;
                let content = Self::require_string_arg(&args, "content")?;
                let create_dirs = Self::optional_bool_arg(&args, "create_dirs")?.unwrap_or(false);

                let node = Self::resolve_node(&nodes, node_id)
                    .ok_or_else(|| anyhow!("node '{node_id}' not found or disabled"))?;
                let client = self.make_client(node)?;
                let result = client.write_file(path, content, create_dirs).await?;

                Ok(ToolResult {
                    success: true,
                    output: serde_json::to_string_pretty(&result)?,
                    error: None,
                })
            }
            "cancel" => {
                if let Some(blocked) = self.require_write_access() {
                    return Ok(blocked);
                }

                let node_id = Self::require_string_arg(&args, "node")?;
                let task_id = Self::require_string_arg(&args, "task_id")?;

                let node = Self::resolve_node(&nodes, node_id)
                    .ok_or_else(|| anyhow!("node '{node_id}' not found or disabled"))?;
                let client = self.make_client(node)?;
                client.cancel(task_id).await?;

                Ok(ToolResult {
                    success: true,
                    output: serde_json::to_string_pretty(&json!({
                        "node": node_id,
                        "task_id": task_id,
                        "cancelled": true,
                    }))?,
                    error: None,
                })
            }
            other => {
                bail!("Unknown action '{other}'. Use: list, status, exec, read, write, cancel")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{new_shared, Config};

    fn make_tool() -> NodesTool {
        let config = new_shared(Config::default());
        let security = Arc::new(SecurityPolicy::default());
        NodesTool::new(config, security)
    }

    #[tokio::test]
    async fn exec_rejects_non_string_command_param() {
        let tool = make_tool();
        let error = tool
            .execute(json!({
                "action": "exec",
                "node": "n1",
                "command": 123
            }))
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("Missing or invalid 'command' parameter"));
    }

    #[tokio::test]
    async fn read_rejects_invalid_offset_param_type() {
        let tool = make_tool();
        let error = tool
            .execute(json!({
                "action": "read",
                "node": "n1",
                "path": "file.txt",
                "offset": "bad"
            }))
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("'offset' must be an unsigned integer"));
    }
}
