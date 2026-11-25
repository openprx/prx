use async_trait::async_trait;
use serde_json::json;

use super::config::HookChange;
use super::event::HookEvent;
use super::manager::HookManager;

impl HookManager {
    pub fn manage_tool(
        self: &std::sync::Arc<Self>,
        security: std::sync::Arc<crate::security::SecurityPolicy>,
    ) -> Box<dyn crate::tools::Tool> {
        Box::new(HooksManageTool {
            manager: self.clone(),
            security,
        })
    }

    pub fn status_tool(self: &std::sync::Arc<Self>) -> Box<dyn crate::tools::Tool> {
        Box::new(HooksStatusTool { manager: self.clone() })
    }

    /// Shared control objects for registries that must preserve identity across
    /// top-level and delegated turns.
    pub fn control_tool_arcs(
        self: &std::sync::Arc<Self>,
        security: std::sync::Arc<crate::security::SecurityPolicy>,
    ) -> Vec<std::sync::Arc<dyn crate::tools::Tool>> {
        vec![
            std::sync::Arc::new(HooksStatusTool { manager: self.clone() }),
            std::sync::Arc::new(HooksManageTool {
                manager: self.clone(),
                security,
            }),
        ]
    }

    /// Append the model-visible control surface for this exact live manager.
    ///
    /// Every model-running entrypoint uses this helper so status/management
    /// cannot accidentally target a different hook generation, or be omitted
    /// while hook execution itself remains enabled.
    pub fn append_control_tools(
        self: &std::sync::Arc<Self>,
        tools: &mut Vec<Box<dyn crate::tools::Tool>>,
        security: std::sync::Arc<crate::security::SecurityPolicy>,
    ) {
        tools.push(self.status_tool());
        tools.push(self.manage_tool(security));
    }
}

pub struct HooksManageTool {
    pub(super) manager: std::sync::Arc<HookManager>,
    pub(super) security: std::sync::Arc<crate::security::SecurityPolicy>,
}

pub struct HooksStatusTool {
    manager: std::sync::Arc<HookManager>,
}

#[async_trait]
impl crate::tools::Tool for HooksStatusTool {
    fn live_hook_manager(&self) -> Option<std::sync::Arc<HookManager>> {
        Some(self.manager.clone())
    }

    fn name(&self) -> &str {
        "hooks_status"
    }

    fn description(&self) -> &str {
        "Inspect the active workspace lifecycle-hooks configuration, event coverage, action count, and validation error."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        Ok(crate::tools::ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&self.manager.diagnostics())?,
            error: None,
        })
    }

    fn tier(&self) -> crate::tools::ToolTier {
        crate::tools::ToolTier::Standard
    }

    fn categories(&self) -> &'static [crate::tools::ToolCategory] {
        &[
            crate::tools::ToolCategory::System,
            crate::tools::ToolCategory::Automation,
        ]
    }
}

impl HooksManageTool {
    fn authorize(&self, action: &str, args: &serde_json::Value) -> Result<(), crate::tools::ToolResult> {
        use crate::security::policy::{ApprovalGrant, SideEffectGate};
        // The one classification of hooks.json changes (D-096).
        let Some(change) = HookChange::of_manage_action(action) else {
            return Ok(());
        };
        let risk = change.risk();
        let grant = ApprovalGrant::from_runtime_args("hooks_manage", args);
        SideEffectGate::new(self.security.as_ref())
            .authorize_resource_operation("hooks_manage", &format!("hooks_manage:{action}"), risk, grant.as_ref())
            .map(|_| ())
            .map_err(|error| crate::tools::ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            })
    }

    fn required_string<'a>(args: &'a serde_json::Value, key: &str) -> anyhow::Result<&'a str> {
        args.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing non-empty '{key}' parameter"))
    }
}

impl HooksManageTool {
    fn execution_schema(&self) -> &'static std::sync::Arc<serde_json::Value> {
        static SCHEMA: std::sync::LazyLock<std::sync::Arc<serde_json::Value>> = std::sync::LazyLock::new(|| {
            std::sync::Arc::new({
                crate::tools::schema::with_action_requirements(
                    json!({
                        "type": "object",
                        "properties": {
                            "action": {
                                "type": "string",
                                "enum": ["status", "refresh", "validate", "replace", "remove", "test"],
                                "description": "Action to perform: 'status' reports the active hooks configuration, 'refresh' reloads it from disk, 'validate' checks a candidate hooks.json, 'replace' installs one atomically, 'remove' deletes it, 'test' fires a synthetic event"
                            },
                            "content": {"type": "string", "description": "Complete hooks.json; required for 'validate' and 'replace'"},
                            "event": {
                                "type": "string",
                                "enum": HookEvent::ALL.map(HookEvent::as_str),
                                "description": "Lifecycle event to fire; required for 'test'"
                            },
                            "payload": {"type": "object", "description": "Synthetic payload for 'test'"}
                        },
                        "required": ["action"],
                        "additionalProperties": false
                    }),
                    "action",
                    &[
                        crate::tools::schema::ActionRequirement {
                            action: "validate",
                            required: &["content"],
                        },
                        crate::tools::schema::ActionRequirement {
                            action: "replace",
                            required: &["content"],
                        },
                        crate::tools::schema::ActionRequirement {
                            action: "test",
                            required: &["event"],
                        },
                    ],
                )
            })
        });
        &SCHEMA
    }
}

#[async_trait]
impl crate::tools::Tool for HooksManageTool {
    fn name(&self) -> &str {
        "hooks_manage"
    }

    fn description(&self) -> &str {
        "Manage workspace lifecycle hooks: 'status' reports the active configuration, 'refresh' reloads it from disk, 'validate' checks a candidate hooks.json, 'replace' installs one atomically, 'remove' deletes it, and 'test' fires a synthetic event. Mutating and test actions are approval-gated."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        crate::tools::schema::publish_flat_schema(self.execution_schema())
    }

    fn execution_parameters_schema(&self) -> Option<std::sync::Arc<serde_json::Value>> {
        Some(std::sync::Arc::clone(self.execution_schema()))
    }

    async fn execute(&self, mut args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        crate::tools::schema::validate_execution_contract(self.name(), self.execution_schema(), &mut args)?;
        use crate::tools::ToolResult;
        let action = Self::required_string(&args, "action")?;
        if let Err(result) = self.authorize(action, &args) {
            return Ok(result);
        }
        let result: anyhow::Result<serde_json::Value> = match action {
            "status" | "refresh" => serde_json::to_value(self.manager.diagnostics()).map_err(Into::into),
            "validate" => {
                let content = Self::required_string(&args, "content")?;
                self.manager
                    .validate_config(content)
                    .and_then(|status| serde_json::to_value(status).map_err(Into::into))
            }
            "replace" => {
                let content = Self::required_string(&args, "content")?;
                self.manager
                    .replace_config(content)
                    .and_then(|status| serde_json::to_value(status).map_err(Into::into))
            }
            "remove" => self
                .manager
                .remove_config()
                .map(|backup| json!({"removed": backup.is_some(), "recoverable_backup": backup})),
            "test" => {
                let event_name = Self::required_string(&args, "event")?;
                let event = HookEvent::from_name(event_name)
                    .ok_or_else(|| anyhow::anyhow!("Unknown hook event '{event_name}'"))?;
                let payload = args
                    .get("payload")
                    .cloned()
                    .unwrap_or_else(|| json!({"source": "hooks_manage", "test": true}));
                self.manager.test_event(event, payload).await;
                Ok(json!({"tested": true, "event": event.as_str()}))
            }
            _ => anyhow::bail!("Unsupported hooks_manage action: {action}"),
        };

        Ok(match result {
            Ok(output) => ToolResult {
                success: true,
                output: serde_json::to_string_pretty(&output)?,
                error: None,
            },
            Err(error) => ToolResult {
                success: false,
                output: String::new(),
                error: Some(error.to_string()),
            },
        })
    }

    fn tier(&self) -> crate::tools::ToolTier {
        crate::tools::ToolTier::Extended
    }

    fn categories(&self) -> &'static [crate::tools::ToolCategory] {
        &[
            crate::tools::ToolCategory::System,
            crate::tools::ToolCategory::Automation,
        ]
    }
}
