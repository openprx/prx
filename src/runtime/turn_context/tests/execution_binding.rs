//! Bind-loop schema pin for 03 R2. Drives `bind_agent_execution_with_runtime`.
use std::sync::Arc;

use async_trait::async_trait;

use crate::agent::test_admission::{AdmittedTurn, registry_with_search};
use crate::memory::Memory;
use crate::tools::Tool;
use crate::tools::traits::ToolResult;

struct SchemaProbeTool;

#[async_trait]
impl Tool for SchemaProbeTool {
    fn name(&self) -> &str {
        "project_tool"
    }

    fn description(&self) -> &str {
        "project tool"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"value": {"type": "integer"}},
            "required": ["value"]
        })
    }

    async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolResult> {
        Ok(ToolResult {
            success: true,
            output: "ok".into(),
            error: None,
        })
    }
}

#[tokio::test]
async fn discovered_spec_with_changed_schema_still_fails_bind() {
    let workspace = tempfile::tempdir().expect("test: schema-bind workspace");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_owned();
    config.causal_tree.enabled = true;
    config.tool_tiering.always_include.push("project_tool".into());
    let memory: Arc<dyn Memory> =
        Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: schema memory"));
    let registry = registry_with_search(vec![Box::new(SchemaProbeTool)]);
    let admitted = AdmittedTurn::with_enabled_builtin_model(
        config,
        "run the project tool",
        Arc::clone(&registry),
        memory,
        ("ollama", "prx-local-model"),
    )
    .await;
    let decision = admitted.decision();
    let selected = decision
        .service
        .agent_execution_specs(decision.proof, decision.context)
        .expect("test: admitted project_tool selection");
    let mut spec = selected
        .specs
        .iter()
        .find(|item| item.name == "project_tool")
        .cloned()
        .expect("test: project_tool spec");
    spec.parameters = serde_json::json!({
        "type": "object",
        "properties": {"tampered": {"type": "string"}},
        "required": ["tampered"]
    });
    let incoming = admitted.adapter().tool_execution_context;
    let error = match decision.service.bind_agent_execution_with_runtime(
        decision.proof,
        decision.context,
        &incoming,
        std::slice::from_ref(&spec),
        None,
    ) {
        Ok(_) => panic!("changed schema must fail bind"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("execution schema differs from the admitted definition"),
        "{error:#}"
    );
}
