use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::memory::Memory;
use crate::security::op_id;
use crate::security::policy::{ApprovalGrant, ResourceRiskLevel};
use crate::security::{SecurityPolicy, SideEffectGate};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

const PURPOSE_MAX_CHARS: usize = 300;
const NOTES_MAX_CHARS: usize = 1024;
const TAGS_MAX: usize = 10;
const TAG_MAX_CHARS: usize = 64;

pub struct ChatProfileUpdateTool {
    memory: Arc<dyn Memory>,
    security: Arc<SecurityPolicy>,
}

impl ChatProfileUpdateTool {
    pub fn new(memory: Arc<dyn Memory>, security: Arc<SecurityPolicy>) -> Self {
        Self { memory, security }
    }
}

fn truncate_chars(value: &str, max_chars: usize) -> (String, bool) {
    let mut out = String::new();
    let mut truncated = false;
    for (idx, ch) in value.chars().enumerate() {
        if idx >= max_chars {
            truncated = true;
            break;
        }
        out.push(ch);
    }
    (out, truncated)
}

fn trusted_scope(args: &serde_json::Value) -> anyhow::Result<(&str, &str, &str)> {
    if !args
        .get("_zc_scope_trusted")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        anyhow::bail!("chat_profile_update requires trusted runtime scope");
    }
    let scope = args
        .get("_zc_scope")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("chat_profile_update requires trusted runtime scope"))?;
    let channel = scope
        .get("channel")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("trusted runtime scope is missing channel"))?;
    let chat_id = scope
        .get("chat_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("trusted runtime scope is missing chat_id"))?;
    let chat_kind = scope
        .get("chat_type")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("direct");
    Ok((channel, chat_id, chat_kind))
}

fn parse_tags(args: &serde_json::Value, notices: &mut Vec<String>) -> anyhow::Result<Option<Vec<String>>> {
    let Some(value) = args.get("tags") else {
        return Ok(None);
    };
    let array = value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("tags must be an array of strings"))?;
    let mut tags = Vec::new();
    let mut truncated_count = false;
    for raw in array {
        let Some(text) = raw.as_str() else {
            anyhow::bail!("tags must be an array of strings");
        };
        if tags.len() >= TAGS_MAX {
            truncated_count = true;
            break;
        }
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (tag, truncated) = truncate_chars(trimmed, TAG_MAX_CHARS);
        if truncated {
            notices.push(format!("tag truncated to {TAG_MAX_CHARS} chars"));
        }
        tags.push(tag);
    }
    if truncated_count {
        notices.push(format!("tags truncated to {TAGS_MAX} items"));
    }
    Ok(Some(tags))
}

#[async_trait]
impl Tool for ChatProfileUpdateTool {
    fn name(&self) -> &str {
        "chat_profile_update"
    }

    fn description(&self) -> &str {
        "Maintain the current conversation profile: what this group or direct chat is for, useful notes, and short tags. The target is always the current trusted runtime chat."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "purpose": {
                    "type": "string",
                    "description": "What the current conversation is for. Max 300 characters."
                },
                "notes": {
                    "type": "string",
                    "description": "Brief operational notes about the current conversation. Max 1024 characters."
                },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Up to 10 short tags for the current conversation."
                }
            },
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        if args.get("channel").is_some() || args.get("chat_id").is_some() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("chat_profile_update target comes from trusted runtime scope, not model parameters".into()),
            });
        }

        let (channel, chat_id, chat_kind) = match trusted_scope(&args) {
            Ok(scope) => scope,
            Err(error) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(error.to_string()),
                });
            }
        };

        let mut notices = Vec::new();
        let purpose = args.get("purpose").and_then(serde_json::Value::as_str).map(|value| {
            let (value, truncated) = truncate_chars(value.trim(), PURPOSE_MAX_CHARS);
            if truncated {
                notices.push(format!("purpose truncated to {PURPOSE_MAX_CHARS} chars"));
            }
            value
        });
        let notes = args.get("notes").and_then(serde_json::Value::as_str).map(|value| {
            let (value, truncated) = truncate_chars(value.trim(), NOTES_MAX_CHARS);
            if truncated {
                notices.push(format!("notes truncated to {NOTES_MAX_CHARS} chars"));
            }
            value
        });
        let tags = match parse_tags(&args, &mut notices) {
            Ok(tags) => tags,
            Err(error) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(error.to_string()),
                });
            }
        };

        if purpose.is_none() && notes.is_none() && tags.is_none() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Provide at least one of purpose, notes, or tags".into()),
            });
        }

        let target_ref = format!("{channel}:{chat_id}");
        let operation_name = op_id::op_id(self.name(), "update", &[&target_ref]);
        let approval_grant = ApprovalGrant::from_runtime_args(self.name(), &args);
        if let Err(error) = SideEffectGate::new(&self.security).authorize_resource_operation(
            self.name(),
            &operation_name,
            ResourceRiskLevel::Medium,
            approval_grant.as_ref(),
        ) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            });
        }

        let tags_ref = tags.as_deref();
        match self
            .memory
            .update_chat_profile(
                channel,
                chat_id,
                chat_kind,
                purpose.as_deref(),
                notes.as_deref(),
                tags_ref,
                "agent",
            )
            .await
        {
            Ok(profile) => {
                let mut output = format!(
                    "Updated current conversation profile for {}:{} (updated_by={})",
                    profile.channel, profile.chat_id, profile.updated_by
                );
                if !notices.is_empty() {
                    output.push_str("; ");
                    output.push_str(&notices.join("; "));
                }
                Ok(ToolResult {
                    success: true,
                    output,
                    error: None,
                })
            }
            Err(error) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to update chat profile: {error}")),
            }),
        }
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Standard
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::Memory]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::SqliteMemory;
    use crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG;
    use tempfile::TempDir;

    fn approved_args(channel: &str, chat_id: &str, chat_type: &str) -> serde_json::Value {
        let target_ref = format!("{channel}:{chat_id}");
        let operation = op_id::op_id("chat_profile_update", "update", &[&target_ref]);
        json!({
            "_zc_scope_trusted": true,
            "_zc_scope": {
                "channel": channel,
                "chat_id": chat_id,
                "chat_type": chat_type,
            },
            RUNTIME_APPROVAL_GRANT_ARG: ApprovalGrant::for_resource_operation(
                "chat_profile_update",
                &operation,
                "test",
                None,
            )
        })
    }

    fn insert_arg(args: &mut serde_json::Value, key: &str, value: serde_json::Value) {
        args.as_object_mut().unwrap().insert(key.to_string(), value);
    }

    #[tokio::test]
    async fn group_turn_update_writes_agent_only_current_group_and_not_memories() {
        let tmp = TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        memory
            .upsert_chat_profile_metadata("telegram", "group-a", "group", Some("Group A"))
            .await
            .unwrap();
        memory
            .upsert_chat_profile_metadata("telegram", "group-b", "group", Some("Group B"))
            .await
            .unwrap();

        let tool = ChatProfileUpdateTool::new(memory.clone(), Arc::new(SecurityPolicy::default()));
        let mut args = approved_args("telegram", "group-a", "group");
        insert_arg(&mut args, "purpose", json!("release coordination"));
        insert_arg(&mut args, "notes", json!("Keep deploy chatter here"));
        insert_arg(&mut args, "tags", json!(["release", "ops"]));

        let result = tool.execute(args).await.unwrap();
        assert!(result.success, "{result:?}");

        let group_a = memory.get_chat_profile("telegram", "group-a").await.unwrap().unwrap();
        let group_b = memory.get_chat_profile("telegram", "group-b").await.unwrap().unwrap();
        assert_eq!(group_a.updated_by, "agent");
        assert_eq!(group_a.purpose.as_deref(), Some("release coordination"));
        assert_eq!(group_a.tags, vec!["release", "ops"]);
        assert_eq!(group_b.purpose, None);
        assert_eq!(group_b.updated_by, "auto");
        assert_eq!(
            memory.count().await.unwrap(),
            0,
            "chat profiles must not write memories table"
        );
    }

    #[tokio::test]
    async fn rejects_model_supplied_target_and_truncates_tags() {
        let tmp = TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        let tool = ChatProfileUpdateTool::new(memory.clone(), Arc::new(SecurityPolicy::default()));

        let mut rejected = approved_args("telegram", "group-a", "group");
        insert_arg(&mut rejected, "channel", json!("telegram"));
        insert_arg(&mut rejected, "chat_id", json!("group-b"));
        insert_arg(&mut rejected, "purpose", json!("wrong target"));
        let result = tool.execute(rejected).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("trusted runtime scope"));

        let mut args = approved_args("telegram", "group-a", "group");
        insert_arg(
            &mut args,
            "tags",
            json!(["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k"]),
        );
        let result = tool.execute(args).await.unwrap();
        assert!(result.success, "{result:?}");
        assert!(result.output.contains("tags truncated to 10 items"));
        let profile = memory.get_chat_profile("telegram", "group-a").await.unwrap().unwrap();
        assert_eq!(profile.tags.len(), 10);
    }
}
