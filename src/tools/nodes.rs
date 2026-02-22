use super::traits::{Tool, ToolResult};
use crate::config::SharedConfig;
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::fs;
use std::sync::Arc;

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

    fn load_nodes_from_config(&self) -> anyhow::Result<Vec<Value>> {
        let cfg = self.config.load_full();
        let raw = fs::read_to_string(&cfg.config_path).map_err(|error| {
            anyhow::anyhow!(
                "Failed to read config file {}: {error}",
                cfg.config_path.display()
            )
        })?;

        let parsed: toml::Value = toml::from_str(&raw).map_err(|error| {
            anyhow::anyhow!(
                "Failed to parse config file {}: {error}",
                cfg.config_path.display()
            )
        })?;

        Ok(Self::parse_nodes_value(parsed.get("nodes")))
    }

    fn parse_nodes_value(nodes_value: Option<&toml::Value>) -> Vec<Value> {
        let mut nodes = Vec::new();

        let Some(nodes_value) = nodes_value else {
            return nodes;
        };

        match nodes_value {
            toml::Value::Table(table) => {
                // Preferred shape:
                // [nodes]
                // edge_1 = { status = "healthy", endpoint = "http://..." }
                for (name, value) in table {
                    if let toml::Value::Table(node_table) = value {
                        nodes.push(Self::normalize_node(Some(name.as_str()), node_table));
                    }
                }

                // Also accept:
                // [nodes]
                // list = [{ id = "edge_1", ... }, ...]
                if nodes.is_empty() {
                    if let Some(list) = table.get("list").and_then(toml::Value::as_array) {
                        for value in list {
                            if let toml::Value::Table(node_table) = value {
                                nodes.push(Self::normalize_node(None, node_table));
                            }
                        }
                    }
                }
            }
            toml::Value::Array(list) => {
                // Also accept:
                // [[nodes]]
                // id = "edge_1"
                for value in list {
                    if let toml::Value::Table(node_table) = value {
                        nodes.push(Self::normalize_node(None, node_table));
                    }
                }
            }
            _ => {}
        }

        nodes
    }

    fn normalize_node(name_hint: Option<&str>, node_table: &toml::value::Table) -> Value {
        let id = node_table
            .get("id")
            .and_then(toml::Value::as_str)
            .or(name_hint)
            .unwrap_or("unknown");

        let status = node_table
            .get("status")
            .and_then(toml::Value::as_str)
            .unwrap_or("unknown");

        let health = node_table
            .get("health")
            .and_then(toml::Value::as_str)
            .unwrap_or(status);

        let endpoint = node_table
            .get("endpoint")
            .and_then(toml::Value::as_str)
            .or_else(|| node_table.get("address").and_then(toml::Value::as_str))
            .or_else(|| node_table.get("host").and_then(toml::Value::as_str));

        let enabled = node_table
            .get("enabled")
            .and_then(toml::Value::as_bool)
            .unwrap_or(true);

        let raw = serde_json::to_value(node_table).unwrap_or_else(|_| json!({}));

        json!({
            "id": id,
            "status": status,
            "health": health,
            "endpoint": endpoint,
            "enabled": enabled,
            "raw": raw,
        })
    }

    fn find_node<'a>(nodes: &'a [Value], node_id: &str) -> Option<&'a Value> {
        nodes.iter().find(|node| {
            node.get("id")
                .and_then(Value::as_str)
                .map(|id| id == node_id)
                .unwrap_or(false)
        })
    }

    fn require_string_arg<'a>(args: &'a Value, key: &str) -> anyhow::Result<&'a str> {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing or invalid '{key}' parameter"))
    }
}

#[async_trait]
impl Tool for NodesTool {
    fn name(&self) -> &str {
        "nodes"
    }

    fn description(&self) -> &str {
        "Manage configured collaboration nodes. Actions: list, status, notify, invoke. \
         Current implementation is config-backed stub using [nodes] in config.toml."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "status", "notify", "invoke"],
                    "description": "Action to perform."
                },
                "node": {
                    "type": "string",
                    "description": "Node ID for status/notify/invoke."
                },
                "message": {
                    "type": "string",
                    "description": "Notification message for notify action."
                },
                "command": {
                    "type": "string",
                    "description": "Command string for invoke action."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = Self::require_string_arg(&args, "action")?;
        let nodes = self.load_nodes_from_config()?;

        match action {
            "list" => Ok(ToolResult {
                success: true,
                output: serde_json::to_string_pretty(&json!({
                    "mode": "stub",
                    "count": nodes.len(),
                    "nodes": nodes,
                }))?,
                error: None,
            }),
            "status" => {
                if let Some(node_id) = args.get("node").and_then(Value::as_str).map(str::trim) {
                    if node_id.is_empty() {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("'node' must be a non-empty string when provided".into()),
                        });
                    }

                    if let Some(node) = Self::find_node(&nodes, node_id) {
                        Ok(ToolResult {
                            success: true,
                            output: serde_json::to_string_pretty(&json!({
                                "mode": "stub",
                                "node": node,
                                "health": node.get("health").cloned().unwrap_or(json!("unknown")),
                            }))?,
                            error: None,
                        })
                    } else {
                        Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!("Node '{node_id}' not found in [nodes] config")),
                        })
                    }
                } else {
                    let summary: Vec<Value> = nodes
                        .iter()
                        .map(|node| {
                            json!({
                                "id": node.get("id"),
                                "health": node.get("health"),
                                "status": node.get("status"),
                                "enabled": node.get("enabled"),
                            })
                        })
                        .collect();

                    Ok(ToolResult {
                        success: true,
                        output: serde_json::to_string_pretty(&json!({
                            "mode": "stub",
                            "count": summary.len(),
                            "nodes": summary,
                        }))?,
                        error: None,
                    })
                }
            }
            "notify" => {
                if let Some(blocked) = self.require_write_access() {
                    return Ok(blocked);
                }

                let node_id = Self::require_string_arg(&args, "node")?;
                let message = Self::require_string_arg(&args, "message")?;

                if let Some(node) = Self::find_node(&nodes, node_id) {
                    Ok(ToolResult {
                        success: true,
                        output: serde_json::to_string_pretty(&json!({
                            "mode": "stub",
                            "delivered": false,
                            "action": "notify",
                            "node": node,
                            "message": message,
                            "note": "Stub implementation; no network call was made.",
                        }))?,
                        error: None,
                    })
                } else {
                    Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Node '{node_id}' not found in [nodes] config")),
                    })
                }
            }
            "invoke" => {
                if let Some(blocked) = self.require_write_access() {
                    return Ok(blocked);
                }

                let node_id = Self::require_string_arg(&args, "node")?;
                let command = Self::require_string_arg(&args, "command")?;

                if let Some(node) = Self::find_node(&nodes, node_id) {
                    Ok(ToolResult {
                        success: true,
                        output: serde_json::to_string_pretty(&json!({
                            "mode": "stub",
                            "executed": false,
                            "action": "invoke",
                            "node": node,
                            "command": command,
                            "note": "Stub implementation; no remote command was executed.",
                        }))?,
                        error: None,
                    })
                } else {
                    Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Node '{node_id}' not found in [nodes] config")),
                    })
                }
            }
            other => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Unknown action '{other}'. Use: list, status, notify, invoke."
                )),
            }),
        }
    }
}
