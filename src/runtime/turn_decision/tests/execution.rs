//! Bind-loop schema pin for 03 R2. Drives `bind_agent_execution_with_runtime`.
#![cfg(test)]

use super::*;
use crate::agent::test_admission::{AdmittedTurn, registry_with_search};
use crate::memory::Memory;
use crate::tools::Tool;
use async_trait::async_trait;
use std::sync::Arc;

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

#[tokio::test]
async fn child_zero_budget_publishes_no_costed_direct_ref() -> anyhow::Result<()> {
    let child = crate::agent::turn_tests::engine_switch::admit_priced_bound_child_segment().await;
    let turn = child.turn.routing();
    let scope = turn.proof.child_scope().expect("test: authenticated child scope");
    let (incoming, original_parent) = scope.gate_inputs();
    let mut parent = original_parent.clone();
    parent.authority.max_cost_cents = 0;
    let manifest = crate::tools::capability::child::ChildCapabilityManifest::derive_strict_subset(
        &parent,
        &incoming.allowed.iter().cloned().collect::<Vec<_>>(),
    )?;
    let model = child.service.resolve_agent_model(turn.proof)?;
    let costs = &child.service.generation.effective.cost;
    let view = child.service.catalog_view();
    let runtime = turn.context.runtime();
    let mut facts = runtime.admission.clone();
    let estimate = |bytes| crate::cost::estimate::estimate_cost(&model, costs, bytes);
    // The costed Direct ref is the configured delegator, which a child
    // publishes only with its sealed outgoing manifest (V-16 integrity, also
    // in Full), so both cells publish for a child holding one.
    let funded = crate::tools::surface_owner::publication::PublicationRules {
        trusted: &runtime.trusted,
        facts: &facts,
        catalog: &view,
    }
    .direct_surface(true, &estimate);
    assert!(
        !funded.is_empty(),
        "test: funded child publishes a real Direct capability"
    );
    assert!(
        funded.iter().any(|reference| view
            .entry_any(&reference.id)
            .and_then(|entry| estimate(entry.schema_bytes))
            .is_some_and(|cost| cost.micro_usd > 0)),
        "test: positive control includes a costed Direct ref"
    );
    facts.child_scope = Some((Arc::new(manifest), Arc::new(parent)));
    let exhausted = crate::tools::surface_owner::publication::PublicationRules {
        trusted: &runtime.trusted,
        facts: &facts,
        catalog: &view,
    }
    .direct_surface(true, &estimate);
    assert!(
        exhausted.is_empty(),
        "test: zero-budget child publishes no costed Direct refs: {exhausted:?}"
    );
    child.shutdown().await;
    Ok(())
}
