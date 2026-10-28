#![cfg(test)]

use super::base_manifest;
use crate::config::Config;
use crate::session_worker::protocol::WorkerManifest;
use crate::session_worker::runner::context::with_manifest_spawn_context;
use crate::session_worker::runner::prompt::build_worker_system_prompt;
use crate::session_worker::runner::tools::{
    WORKER_ORCHESTRATION_TOOL_NAMES, select_tools_for_manifest, select_tools_for_worker,
    verify_worker_orchestration_tool_names,
};
use crate::tools::Tool;
use std::path::Path;
use std::sync::Arc;

#[tokio::test]
async fn worker_wildcard_preserves_its_source_registry() {
    let source = crate::tools::default_tools(Arc::new(crate::security::SecurityPolicy::default()));
    let expected = source.len();
    let selected = select_tools_for_worker(source, &["*".to_string()], &[])
        .await
        .expect("wildcard selection");
    assert_eq!(selected.len(), expected);
}

/// One plain tool under a fixed public name, to build a registry whose
/// membership is exactly what a test wants to reason about.
struct NamedWorkerTool(&'static str);

#[async_trait::async_trait]
impl Tool for NamedWorkerTool {
    fn name(&self) -> &str {
        self.0
    }

    fn description(&self) -> &str {
        "worker surface fixture"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        Ok(crate::tools::ToolResult {
            success: true,
            output: self.0.to_string(),
            error: None,
        })
    }
}

fn worker_surface_fixture() -> Vec<Box<dyn Tool>> {
    [
        "gateway",
        "proxy_config",
        "config_reload",
        "nodes",
        "git_operations",
        "shell",
    ]
    .into_iter()
    .map(|name| Box::new(NamedWorkerTool(name)) as Box<dyn Tool>)
    .collect()
}

async fn worker_tool_names(allowed: &[String], excluded: &[String]) -> Vec<String> {
    select_tools_for_worker(worker_surface_fixture(), allowed, excluded)
        .await
        .expect("worker tool selection")
        .iter()
        .map(|tool| tool.name().to_string())
        .collect()
}

#[tokio::test]
async fn a_process_worker_cannot_regain_the_operations_surface_its_channel_hides() {
    // Fixture validity first: the exclusions come from the real default
    // channel surface, not from a list written for this test, so the test
    // fails if `channel_exclude` ever stops hiding the operations tools.
    let surface = crate::config::ToolTieringConfig::default().for_channel_surface();
    let hidden = surface.always_exclude.clone();
    for name in ["gateway", "proxy_config", "config_reload", "nodes", "git_operations"] {
        assert!(
            hidden.iter().any(|entry| entry == name),
            "the default channel surface must hide '{name}' for this fixture to mean anything"
        );
    }

    let filtered = worker_tool_names(&[], &hidden).await;
    for name in ["gateway", "proxy_config", "config_reload", "nodes", "git_operations"] {
        assert!(
            !filtered.iter().any(|entry| entry == name),
            "a worker started from an IM turn must not publish '{name}': {filtered:?}"
        );
    }
    assert!(
        filtered.iter().any(|entry| entry == "shell"),
        "the rest of the surface is untouched: {filtered:?}"
    );

    // The paired half: without an inherited list the same worker publishes
    // the whole registry, so the assertion above is about the list and not
    // about the fixture being empty.
    let unfiltered = worker_tool_names(&[], &[]).await;
    assert!(
        unfiltered.iter().any(|entry| entry == "gateway"),
        "an operator-rooted worker keeps the operations surface: {unfiltered:?}"
    );

    // An explicit `allowed_tools` is the agent's own contract and cannot
    // re-open the channel boundary.
    let named = worker_tool_names(&["gateway".to_string()], &hidden).await;
    assert!(
        named.is_empty(),
        "naming a hidden tool in allowed_tools must not publish it: {named:?}"
    );

    // An operator who re-enables one keeps it: `for_channel_surface` never
    // folds an `always_include` name into the hidden set.
    let mut reopened = crate::config::ToolTieringConfig::default();
    reopened.always_include.push("gateway".to_string());
    let reopened = reopened.for_channel_surface().always_exclude;
    let names = worker_tool_names(&[], &reopened).await;
    assert!(
        names.iter().any(|entry| entry == "gateway"),
        "always_include must still reach a process worker: {names:?}"
    );
}

async fn manifest_tool_names(manifest: &WorkerManifest) -> Vec<String> {
    select_tools_for_manifest(worker_surface_fixture(), manifest)
        .await
        .expect("worker tool selection")
        .iter()
        .map(|tool| tool.name().to_string())
        .collect()
}

/// The manifest's channel exclusions must reach the worker's tool filter.
///
/// `a_process_worker_cannot_regain_the_operations_surface_its_channel_hides`
/// hands `select_tools_for_worker` a list the test computed itself, so it
/// pins the filter and not the production wiring: with
/// `run_validated_manifest` passing `&[]` in place of
/// `&manifest.channel_excluded_tools` that test stayed green (independent
/// audit 2026-09-16 §1.4). This one goes through
/// `select_tools_for_manifest`, the single binding `run_validated_manifest`
/// uses, so the wiring itself is what is asserted.
///
/// MUTATION GUARD: replace `&manifest.channel_excluded_tools` in
/// `select_tools_for_manifest` with `&[]` and the first assertion fails.
#[tokio::test]
async fn a_worker_manifest_binds_its_channel_exclusions_to_the_tool_filter() {
    // Fixture validity first: the hidden set is the real default channel
    // surface, so the test dies if `channel_exclude` stops hiding these.
    let hidden = crate::config::ToolTieringConfig::default()
        .for_channel_surface()
        .always_exclude;
    for name in ["gateway", "proxy_config", "config_reload", "nodes", "git_operations"] {
        assert!(
            hidden.iter().any(|entry| entry == name),
            "the default channel surface must hide '{name}' for this fixture to mean anything"
        );
    }

    let mut manifest = base_manifest(Path::new("/tmp/worker-manifest-exclusions"), "capability");
    manifest.allowed_tools = Vec::new();
    manifest.channel_excluded_tools = hidden;

    let filtered = manifest_tool_names(&manifest).await;
    for name in ["gateway", "proxy_config", "config_reload", "nodes", "git_operations"] {
        assert!(
            !filtered.iter().any(|entry| entry == name),
            "a worker resumed from this manifest must not publish '{name}': {filtered:?}"
        );
    }
    assert!(
        filtered.iter().any(|entry| entry == "shell"),
        "the rest of the surface is untouched: {filtered:?}"
    );

    // The paired half: the same manifest with an empty exclusion list
    // publishes the operations surface, so the assertion above is about the
    // manifest field and not about the fixture being empty.
    manifest.channel_excluded_tools = Vec::new();
    let unfiltered = manifest_tool_names(&manifest).await;
    assert!(
        unfiltered.iter().any(|entry| entry == "gateway"),
        "an operator-rooted manifest keeps the operations surface: {unfiltered:?}"
    );

    // The manifest's own `allowed_tools` reaches the same binding.
    manifest.allowed_tools = vec!["shell".to_string()];
    let allowed = manifest_tool_names(&manifest).await;
    assert_eq!(
        allowed,
        vec!["shell".to_string()],
        "allowed_tools must come from the manifest too: {allowed:?}"
    );
}

#[tokio::test]
async fn a_worker_hands_its_inherited_exclusions_to_everything_it_spawns() {
    let workspace = tempfile::TempDir::new().expect("tempdir");
    let mut manifest = base_manifest(workspace.path(), "capability");
    manifest.session_scope_key = "sessions_spawn:parent".to_string();
    manifest.channel_excluded_tools = vec!["gateway".to_string()];
    let inherited = with_manifest_spawn_context(&manifest, None, None, None, None, async {
        crate::tools::sessions_spawn::ambient_channel_excluded_tools()
    })
    .await;
    assert_eq!(
        inherited,
        vec!["gateway".to_string()],
        "a grandchild spawned inside the worker must see the same boundary"
    );

    manifest.channel_excluded_tools.clear();
    let operator = with_manifest_spawn_context(&manifest, None, None, None, None, async {
        crate::tools::sessions_spawn::ambient_channel_excluded_tools()
    })
    .await;
    assert!(operator.is_empty(), "an operator-rooted worker seeds no exclusions");
}

struct WorkerDynamicAliases;

#[async_trait::async_trait]
impl Tool for WorkerDynamicAliases {
    fn name(&self) -> &str {
        "dynamic_router"
    }

    fn description(&self) -> &str {
        "Dynamic worker alias fixture"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    fn specs(&self) -> Vec<crate::tools::ToolSpec> {
        ["dynamic__one", "dynamic__two"]
            .into_iter()
            .map(|name| crate::tools::ToolSpec {
                name: name.to_string(),
                description: name.to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {"value": {"type": "string"}},
                    "required": ["value"]
                }),
            })
            .collect()
    }

    fn supports_name(&self, name: &str) -> bool {
        matches!(name, "dynamic_router" | "dynamic__one" | "dynamic__two")
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        Ok(crate::tools::ToolResult {
            success: false,
            output: String::new(),
            error: Some("root execution is outside this fixture's contract".to_string()),
        })
    }

    async fn execute_named(&self, name: &str, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        Ok(crate::tools::ToolResult {
            success: true,
            output: name.to_string(),
            error: None,
        })
    }
}

#[tokio::test]
async fn worker_allowlist_can_select_multiple_aliases_from_one_dynamic_router() {
    let selected = select_tools_for_worker(
        vec![Box::new(WorkerDynamicAliases)],
        &["dynamic__one".to_string(), "dynamic__two".to_string()],
        &[],
    )
    .await
    .expect("dynamic aliases must resolve independently");

    assert_eq!(selected.len(), 2);
    for name in ["dynamic__one", "dynamic__two"] {
        let tool = selected
            .iter()
            .find(|tool| tool.name() == name)
            .expect("selected alias proxy");
        let result = tool
            .execute_named(name, serde_json::json!({"value": "ok"}))
            .await
            .expect("alias execution");
        assert_eq!(result.output, name);
    }
}

#[test]
fn worker_orchestration_contract_detects_missing_entrypoint_tools() {
    verify_worker_orchestration_tool_names(WORKER_ORCHESTRATION_TOOL_NAMES.iter().copied())
        .expect("complete orchestration contract");

    let error = verify_worker_orchestration_tool_names(["sessions_spawn", "gateway"])
        .expect_err("incomplete orchestration contract must fail");
    assert!(error.to_string().contains("sessions_list"));
    assert!(error.to_string().contains("message_send"));
}

#[tokio::test]
async fn worker_wildcard_must_be_exclusive() {
    let source = crate::tools::default_tools(Arc::new(crate::security::SecurityPolicy::default()));
    let error = match select_tools_for_worker(source, &["*".to_string(), "shell".to_string()], &[]).await {
        Ok(_) => panic!("mixed wildcard must be rejected"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("exclusively"));
}

#[tokio::test]
async fn worker_explicit_allowlist_inherits_transcript_recovery_dependency() {
    let mut source = crate::tools::default_tools(Arc::new(crate::security::SecurityPolicy::default()));
    source.push(Box::new(crate::tools::TranscriptHistoryLookupTool::new(Arc::new(
        crate::memory::NoneMemory::new(),
    ))));

    let selected = select_tools_for_worker(source, &["shell".to_string()], &[])
        .await
        .expect("restricted selection");
    let names = selected.iter().map(|tool| tool.name()).collect::<Vec<_>>();
    assert_eq!(names, vec!["shell", crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME]);
}

#[test]
fn worker_system_prompt_keeps_agent_instructions_without_a_static_tool_catalog() {
    let workspace = tempfile::TempDir::new().expect("workspace");
    let mut config = Config::default();
    config.workspace_dir = workspace.path().to_path_buf();
    let mut manifest = base_manifest(workspace.path(), "capability");
    manifest.model = "k3".to_string();
    manifest.system_prompt = Some("Use file tools and verify every artifact.".to_string());
    let identity = workspace.path().join("identity/worker");
    std::fs::create_dir_all(&identity).unwrap();
    std::fs::write(identity.join("SOUL.md"), "Worker role identity sentinel").unwrap();

    let prompt = build_worker_system_prompt(&manifest, &config, true);

    assert!(!prompt.contains("## Tools"));
    assert!(prompt.contains("Working directory: `"));
    assert!(prompt.contains("Use tools when the request requires action"));
    assert!(prompt.contains("## Delegated Agent Instructions"));
    assert!(prompt.contains("Use file tools and verify every artifact."));
    assert!(prompt.contains("Worker role identity sentinel"));
    assert!(prompt.contains("## Additional Role Instructions"));
    assert!(prompt.contains("Never claim that a file, command, or check succeeded"));
}
