//! agents_list — list configured delegate agents.
//!
//! Returns the names and configurations of agents defined in the
//! `[agents]` section of the config. These are the agents that can be
//! targeted by `sessions_spawn` (task delegation).
//!
//! Aligns with OpenClaw's `agents_list` tool.

use super::traits::{Tool, ToolResult};
use crate::config::DelegateAgentConfig;
use async_trait::async_trait;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

/// Tool that lists all configured delegate agents.
pub struct AgentsListTool {
    agents: Arc<HashMap<String, DelegateAgentConfig>>,
}

impl AgentsListTool {
    pub fn new(agents: HashMap<String, DelegateAgentConfig>) -> Self {
        Self {
            agents: Arc::new(agents),
        }
    }
}

#[async_trait]
impl Tool for AgentsListTool {
    fn name(&self) -> &str {
        "agents_list"
    }

    fn description(&self) -> &str {
        "List the agent IDs you can target with sessions_spawn for delegating tasks. \
         Shows each agent's name, provider, model, and capabilities."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
        if self.agents.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: "No delegate agents configured. \
                         Add agents under [agents.*] in your config to enable delegation."
                    .into(),
                error: None,
            });
        }

        let mut lines: Vec<String> = self
            .agents
            .iter()
            .map(|(name, cfg)| {
                let agentic_tag = if cfg.agentic { " [agentic]" } else { "" };
                format!(
                    "• **{name}**{agentic_tag}\n  Provider: {} / Model: {}\n  Max depth: {}",
                    cfg.provider, cfg.model, cfg.max_depth
                )
            })
            .collect();

        // Sort for deterministic output
        lines.sort();

        Ok(ToolResult {
            success: true,
            output: format!(
                "Configured agents ({} total):\n\n{}",
                self.agents.len(),
                lines.join("\n\n")
            ),
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DelegateAgentConfig;

    fn make_agent(provider: &str, model: &str) -> DelegateAgentConfig {
        DelegateAgentConfig {
            provider: provider.to_string(),
            model: model.to_string(),
            system_prompt: None,
            api_key: None,
            temperature: None,
            max_depth: 3,
            agentic: false,
            allowed_tools: Vec::new(),
            max_iterations: 10,
        }
    }

    #[test]
    fn name_and_description() {
        let tool = AgentsListTool::new(HashMap::new());
        assert_eq!(tool.name(), "agents_list");
        assert!(!tool.description().is_empty());
    }

    #[tokio::test]
    async fn no_agents_returns_helpful_message() {
        let tool = AgentsListTool::new(HashMap::new());
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("No delegate agents"));
    }

    #[tokio::test]
    async fn lists_agents() {
        let mut agents = HashMap::new();
        agents.insert("researcher".to_string(), make_agent("anthropic", "claude-3-5-sonnet"));
        agents.insert("coder".to_string(), make_agent("openai", "gpt-4o"));
        let tool = AgentsListTool::new(agents);

        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("researcher"));
        assert!(result.output.contains("coder"));
        assert!(result.output.contains("2 total"));
    }

    #[tokio::test]
    async fn agentic_flag_shown() {
        let mut agents = HashMap::new();
        agents.insert(
            "super-agent".to_string(),
            DelegateAgentConfig {
                agentic: true,
                ..make_agent("openai", "gpt-4o")
            },
        );
        let tool = AgentsListTool::new(agents);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.output.contains("[agentic]"));
    }
}
