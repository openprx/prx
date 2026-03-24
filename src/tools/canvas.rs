use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::sync::Arc;

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

                let mut state = self.state.lock();
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

                let mut state = self.state.lock();
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

                let mut state = self.state.lock();
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

                let mut state = self.state.lock();
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
                let mut state = self.state.lock();
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
    fn tier(&self) -> ToolTier {
        ToolTier::Extended
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::Media]
    }
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::AutonomyLevel;

    fn test_security(level: AutonomyLevel, max_actions: u32) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: level,
            max_actions_per_hour: max_actions,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        })
    }

    fn canvas(level: AutonomyLevel) -> CanvasTool {
        CanvasTool::new(test_security(level, 1000))
    }

    // ── Metadata ────────────────────────────────────────────────

    #[test]
    fn tool_name() {
        assert_eq!(canvas(AutonomyLevel::Full).name(), "canvas");
    }

    #[test]
    fn tool_description_is_non_empty() {
        assert!(!canvas(AutonomyLevel::Full).description().is_empty());
    }

    #[test]
    fn tool_schema_requires_action() {
        let schema = canvas(AutonomyLevel::Full).parameters_schema();
        let required = schema["required"].as_array().expect("test: required array");
        assert!(required.iter().any(|v| v == "action"));
    }

    // ── parse_action validation ─────────────────────────────────

    #[test]
    fn parse_action_missing() {
        let args = json!({});
        let err = CanvasTool::parse_action(&args).unwrap_err();
        assert!(!err.success);
        assert!(err.error.as_deref().unwrap_or("").contains("action"));
    }

    #[test]
    fn parse_action_empty_string() {
        let args = json!({"action": ""});
        let err = CanvasTool::parse_action(&args).unwrap_err();
        assert!(err.error.as_deref().unwrap_or("").contains("action"));
    }

    #[test]
    fn parse_action_whitespace_only() {
        let args = json!({"action": "   "});
        let err = CanvasTool::parse_action(&args).unwrap_err();
        assert!(err.error.is_some());
    }

    #[test]
    fn parse_action_valid() {
        let args = json!({"action": "present"});
        assert_eq!(CanvasTool::parse_action(&args).unwrap(), "present");
    }

    #[test]
    fn parse_action_trims_whitespace() {
        let args = json!({"action": "  hide  "});
        assert_eq!(CanvasTool::parse_action(&args).unwrap(), "hide");
    }

    // ── Security: read-only blocks mutations ────────────────────

    #[tokio::test]
    async fn readonly_blocks_present() {
        let tool = canvas(AutonomyLevel::ReadOnly);
        let result = tool
            .execute(json!({"action": "present", "content": "hi"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("read-only"));
    }

    #[tokio::test]
    async fn readonly_blocks_hide() {
        let tool = canvas(AutonomyLevel::ReadOnly);
        let result = tool.execute(json!({"action": "hide"})).await.unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn readonly_blocks_navigate() {
        let tool = canvas(AutonomyLevel::ReadOnly);
        let result = tool
            .execute(json!({"action": "navigate", "url": "https://x.com"}))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn readonly_blocks_eval() {
        let tool = canvas(AutonomyLevel::ReadOnly);
        let result = tool.execute(json!({"action": "eval", "script": "1+1"})).await.unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn readonly_allows_snapshot() {
        let tool = canvas(AutonomyLevel::ReadOnly);
        let result = tool.execute(json!({"action": "snapshot"})).await.unwrap();
        assert!(result.success);
    }

    // ── Security: rate limiting ─────────────────────────────────

    #[tokio::test]
    async fn rate_limit_blocks_after_exhaustion() {
        let tool = CanvasTool::new(test_security(AutonomyLevel::Full, 1));
        // First call consumes the budget
        let r1 = tool
            .execute(json!({"action": "present", "content": "a"}))
            .await
            .unwrap();
        assert!(r1.success);
        // Second call blocked by rate limit
        let r2 = tool
            .execute(json!({"action": "present", "content": "b"}))
            .await
            .unwrap();
        assert!(!r2.success);
        assert!(r2.error.as_deref().unwrap_or("").contains("rate limit"));
    }

    // ── Action: present ─────────────────────────────────────────

    #[tokio::test]
    async fn present_sets_visible_and_content() {
        let tool = canvas(AutonomyLevel::Full);
        let result = tool
            .execute(json!({"action": "present", "content": "Hello World"}))
            .await
            .unwrap();
        assert!(result.success);
        let out: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(out["action"], "present");
        assert_eq!(out["visible"], true);
        assert_eq!(out["content"], "Hello World");
    }

    #[tokio::test]
    async fn present_without_content_uses_empty_default() {
        let tool = canvas(AutonomyLevel::Full);
        let result = tool.execute(json!({"action": "present"})).await.unwrap();
        assert!(result.success);
        let out: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(out["content"], "");
    }

    // ── Action: hide ────────────────────────────────────────────

    #[tokio::test]
    async fn hide_clears_visible() {
        let tool = canvas(AutonomyLevel::Full);
        let _ = tool.execute(json!({"action": "present", "content": "x"})).await;
        let result = tool.execute(json!({"action": "hide"})).await.unwrap();
        assert!(result.success);
        let out: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(out["visible"], false);
    }

    // ── Action: navigate ────────────────────────────────────────

    #[tokio::test]
    async fn navigate_sets_url_and_visible() {
        let tool = canvas(AutonomyLevel::Full);
        let result = tool
            .execute(json!({"action": "navigate", "url": "https://example.com"}))
            .await
            .unwrap();
        assert!(result.success);
        let out: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(out["current_url"], "https://example.com");
        assert_eq!(out["visible"], true);
    }

    #[tokio::test]
    async fn navigate_missing_url_fails() {
        let tool = canvas(AutonomyLevel::Full);
        let result = tool.execute(json!({"action": "navigate"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("url"));
    }

    #[tokio::test]
    async fn navigate_empty_url_fails() {
        let tool = canvas(AutonomyLevel::Full);
        let result = tool.execute(json!({"action": "navigate", "url": ""})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("url"));
    }

    #[tokio::test]
    async fn navigate_whitespace_url_fails() {
        let tool = canvas(AutonomyLevel::Full);
        let result = tool.execute(json!({"action": "navigate", "url": "   "})).await.unwrap();
        assert!(!result.success);
    }

    // ── Action: eval ────────────────────────────────────────────

    #[tokio::test]
    async fn eval_returns_stub_result() {
        let tool = canvas(AutonomyLevel::Full);
        let result = tool
            .execute(json!({"action": "eval", "script": "document.title"}))
            .await
            .unwrap();
        assert!(result.success);
        let out: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(out["action"], "eval");
        assert!(out["result"]["value"].as_str().unwrap_or("").contains("14 characters"));
    }

    #[tokio::test]
    async fn eval_missing_script_fails() {
        let tool = canvas(AutonomyLevel::Full);
        let result = tool.execute(json!({"action": "eval"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("script"));
    }

    #[tokio::test]
    async fn eval_empty_script_fails() {
        let tool = canvas(AutonomyLevel::Full);
        let result = tool.execute(json!({"action": "eval", "script": ""})).await.unwrap();
        assert!(!result.success);
    }

    // ── Action: snapshot ────────────────────────────────────────

    #[tokio::test]
    async fn snapshot_returns_full_state() {
        let tool = canvas(AutonomyLevel::Full);
        let _ = tool.execute(json!({"action": "present", "content": "snap-test"})).await;
        let _ = tool
            .execute(json!({"action": "navigate", "url": "https://x.com"}))
            .await;

        let result = tool.execute(json!({"action": "snapshot"})).await.unwrap();
        assert!(result.success);
        let out: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(out["action"], "snapshot");
        assert_eq!(out["visible"], true);
        assert_eq!(out["content"], "snap-test");
        assert_eq!(out["current_url"], "https://x.com");
        assert!(
            out["snapshot_id"]
                .as_str()
                .unwrap_or("")
                .starts_with("canvas-snapshot-")
        );
    }

    #[tokio::test]
    async fn snapshot_version_increments() {
        let tool = canvas(AutonomyLevel::Full);
        let r1 = tool.execute(json!({"action": "snapshot"})).await.unwrap();
        let r2 = tool.execute(json!({"action": "snapshot"})).await.unwrap();
        let o1: Value = serde_json::from_str(&r1.output).unwrap();
        let o2: Value = serde_json::from_str(&r2.output).unwrap();
        assert_eq!(o1["snapshot_id"], "canvas-snapshot-1");
        assert_eq!(o2["snapshot_id"], "canvas-snapshot-2");
    }

    // ── Unknown action ──────────────────────────────────────────

    #[tokio::test]
    async fn unknown_action_fails() {
        let tool = canvas(AutonomyLevel::Full);
        let result = tool.execute(json!({"action": "destroy"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("Unsupported"));
    }

    // ── State continuity across actions ─────────────────────────

    #[tokio::test]
    async fn present_then_hide_then_snapshot_tracks_state() {
        let tool = canvas(AutonomyLevel::Full);
        let _ = tool.execute(json!({"action": "present", "content": "hello"})).await;
        let _ = tool.execute(json!({"action": "hide"})).await;

        let result = tool.execute(json!({"action": "snapshot"})).await.unwrap();
        let out: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(out["visible"], false);
        assert_eq!(out["content"], "hello");
    }

    #[tokio::test]
    async fn eval_then_snapshot_captures_eval_result() {
        let tool = canvas(AutonomyLevel::Full);
        let _ = tool.execute(json!({"action": "eval", "script": "test()"})).await;

        let result = tool.execute(json!({"action": "snapshot"})).await.unwrap();
        let out: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(out["last_eval_script"], "test()");
        assert!(out["last_eval_result"].is_object());
    }
}
