use super::traits::{Tool, ToolResult};
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
struct CanvasState {
    visible: bool,
    current_url: Option<String>,
    content: Option<String>,
    last_eval_script: Option<String>,
    last_eval_result: Option<Value>,
    snapshot_version: u64,
}

pub struct CanvasTool {
    security: Arc<SecurityPolicy>,
    state: Mutex<CanvasState>,
}

impl CanvasTool {
    pub fn new(security: Arc<SecurityPolicy>) -> Self {
        Self {
            security,
            state: Mutex::new(CanvasState::default()),
        }
    }

    fn mutation_block_result(&self) -> Option<ToolResult> {
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

    fn parse_action<'a>(args: &'a Value) -> Result<&'a str, ToolResult> {
        args.get("action")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ToolResult {
                success: false,
                output: String::new(),
                error: Some("Missing or invalid 'action' parameter".into()),
            })
    }
}

#[async_trait]
impl Tool for CanvasTool {
    fn name(&self) -> &str {
        "canvas"
    }

    fn description(&self) -> &str {
        "Manage a canvas session for presenting content and browser-like operations. \
         Actions: present, hide, navigate, eval, snapshot. Current backend is an in-memory stub."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["present", "hide", "navigate", "eval", "snapshot"],
                    "description": "Canvas action to perform."
                },
                "content": {
                    "type": "string",
                    "description": "Content to show for the 'present' action."
                },
                "url": {
                    "type": "string",
                    "description": "URL to set for the 'navigate' action."
                },
                "script": {
                    "type": "string",
                    "description": "JavaScript source for the 'eval' action."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = match Self::parse_action(&args) {
            Ok(action) => action,
            Err(result) => return Ok(result),
        };

        match action {
            "present" => {
                if let Some(blocked) = self.mutation_block_result() {
                    return Ok(blocked);
                }

                let content = args
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();

                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                state.visible = true;
                state.content = Some(content.clone());

                Ok(ToolResult {
                    success: true,
                    output: serde_json::to_string_pretty(&json!({
                        "mode": "stub",
                        "action": "present",
                        "visible": state.visible,
                        "content": content,
                    }))?,
                    error: None,
                })
            }
            "hide" => {
                if let Some(blocked) = self.mutation_block_result() {
                    return Ok(blocked);
                }

                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                state.visible = false;

                Ok(ToolResult {
                    success: true,
                    output: serde_json::to_string_pretty(&json!({
                        "mode": "stub",
                        "action": "hide",
                        "visible": state.visible,
                    }))?,
                    error: None,
                })
            }
            "navigate" => {
                if let Some(blocked) = self.mutation_block_result() {
                    return Ok(blocked);
                }

                let Some(url) = args.get("url").and_then(Value::as_str).map(str::trim) else {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("Missing or invalid 'url' parameter".into()),
                    });
                };
                if url.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("Missing or invalid 'url' parameter".into()),
                    });
                }

                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                state.current_url = Some(url.to_string());
                state.visible = true;

                Ok(ToolResult {
                    success: true,
                    output: serde_json::to_string_pretty(&json!({
                        "mode": "stub",
                        "action": "navigate",
                        "current_url": state.current_url,
                        "visible": state.visible,
                    }))?,
                    error: None,
                })
            }
            "eval" => {
                if let Some(blocked) = self.mutation_block_result() {
                    return Ok(blocked);
                }

                let Some(script) = args.get("script").and_then(Value::as_str).map(str::trim) else {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("Missing or invalid 'script' parameter".into()),
                    });
                };
                if script.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("Missing or invalid 'script' parameter".into()),
                    });
                }

                let mock_result = json!({
                    "kind": "stub_eval_result",
                    "value": format!("stub: evaluated {} characters", script.len()),
                });

                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                state.last_eval_script = Some(script.to_string());
                state.last_eval_result = Some(mock_result.clone());

                Ok(ToolResult {
                    success: true,
                    output: serde_json::to_string_pretty(&json!({
                        "mode": "stub",
                        "action": "eval",
                        "script": script,
                        "result": mock_result,
                    }))?,
                    error: None,
                })
            }
            "snapshot" => {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                state.snapshot_version += 1;

                Ok(ToolResult {
                    success: true,
                    output: serde_json::to_string_pretty(&json!({
                        "mode": "stub",
                        "action": "snapshot",
                        "snapshot_id": format!("canvas-snapshot-{}", state.snapshot_version),
                        "visible": state.visible,
                        "current_url": state.current_url,
                        "content": state.content,
                        "last_eval_script": state.last_eval_script,
                        "last_eval_result": state.last_eval_result,
                    }))?,
                    error: None,
                })
            }
            other => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Unsupported action '{other}'. Expected one of: present, hide, navigate, eval, snapshot"
                )),
            }),
        }
    }
}
