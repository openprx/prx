#![cfg(test)]
//! Diagnostic reproduction; original integration DAS body remains untouched.
use super::Agent;
use crate::providers::{ChatRequest, ChatResponse, Provider, ToolCall};
use crate::tools::{DelegateTool, Tool, ToolResult};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
struct Parent;
#[async_trait::async_trait]
impl Provider for Parent {
    fn supports_native_tools(&self) -> bool {
        true
    }
    async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
        anyhow::bail!("depth fixture requires its actual native tool loop")
    }
    async fn chat(&self, request: ChatRequest<'_>, model: &str, _: f64) -> anyhow::Result<ChatResponse> {
        anyhow::ensure!(model == "prx-local-model", "pinned original chat model");
        anyhow::ensure!(
            request
                .tools
                .is_some_and(|tools| tools.iter().any(|tool| tool.name == "delegate")),
            "provider receives actually admitted tool schema"
        );
        let completed = request.messages.iter().any(|message| message.role == "tool");
        Ok(ChatResponse {
            text: completed.then(|| "depth fixture complete".to_owned()),
            tool_calls: if completed {
                Vec::new()
            } else {
                vec![ToolCall {
                    id: "prx_depth_delegate".to_owned(),
                    name: "delegate".to_owned(),
                    arguments: json!({"agent":"deep-agent", "prompt":"go deeper"}).to_string(),
                }]
            },
            reasoning_content: None,
        })
    }
}
struct Captor {
    inner: DelegateTool,
    result: Arc<parking_lot::Mutex<Option<ToolResult>>>,
    calls: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl Tool for Captor {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        self.inner.parameters_schema()
    }
    fn tier(&self) -> crate::tools::traits::ToolTier {
        self.inner.tier()
    }
    fn categories(&self) -> &'static [crate::tools::traits::ToolCategory] {
        self.inner.categories()
    }
    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let result = self.inner.execute(args).await?;
        *self.result.lock() = Some(result.clone());
        Ok(result)
    }
}
#[inline(never)]
fn turn<'a>(
    agent: &'a mut Agent,
) -> std::pin::Pin<Box<impl std::future::Future<Output = anyhow::Result<String>> + 'a>> {
    Box::pin(agent.turn("delegate depth boundary"))
}
#[tokio::test]
async fn genuine_depth_denial_retains_parent_cleanup_ack_evidence() -> anyhow::Result<()> {
    use futures::FutureExt as _;
    use std::os::unix::fs::PermissionsExt as _;
    let mut workspace = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    workspace.disable_cleanup(true);
    let path = workspace.path().join("cleanup-scalars.json");
    eprintln!("owned cleanup diagnostic: {}", path.display());
    let recorder = crate::runtime::turn_cleanup::diagnostic::Recorder::new(path);
    let driver = super::native_decision_fixture::Driver::start_depth_denial()
        .await
        .unwrap();
    let endpoint = driver.endpoint().to_owned();
    let result = Arc::new(parking_lot::Mutex::new(None));
    let calls = Arc::new(AtomicUsize::new(0));
    let turn_state = Arc::new(parking_lot::Mutex::new(None));
    let outcome = std::panic::AssertUnwindSafe(driver.run_factory(true, || {
        recorder.scope(async {
            let mut config = crate::config::Config {
                workspace_dir: workspace.path().to_owned(),
                config_path: workspace.path().join("config.toml"),
                default_model: Some("prx-local-model".to_owned()),
                ..Default::default()
            };
            config.memory.auto_save = false;
            config.autonomy.level = crate::security::AutonomyLevel::Full;
            config.tool_tiering.always_include = vec!["delegate".to_owned()];
            config.providers.models.push(
                serde_json::from_value(json!({
                    "provider":"ollama","model":"prx-local-model","operations":["chat"],"native_tools":true
                }))
                .unwrap(),
            );
            config.cost.prices.insert(
                "prx-local-model".to_owned(),
                crate::config::schema::ModelPricing {
                    input: 1.0,
                    output: 1.0,
                    cache_write: 1.0,
                    cache_read: 1.0,
                },
            );
            let depth_config = crate::config::DelegateAgentConfig {
                provider: "test".to_owned(),
                model: "test-model".to_owned(),
                system_prompt: None,
                api_key: None,
                credential_ref: None,
                temperature: None,
                max_depth: 2,
                agentic: false,
                allowed_tools: vec![],
                identity_dir: None,
                memory_scope: None,
                spawn_enabled: None,
            };
            config.agents.insert("deep-agent".to_owned(), depth_config);
            super::native_decision_fixture::configure(&mut config, &endpoint).unwrap();
            assert_eq!(
                config.causal_tree.rehearsal.timeout_ms,
                super::native_decision_fixture::REHEARSAL_TIMEOUT_MS
            );
            assert_eq!(config.causal_tree.policy.extra_latency_budget_ms, 500);
            let delegate = DelegateTool::with_depth(
                config.agents.clone(),
                Arc::new(crate::security::SecurityPolicy::default()),
                2,
            );
            let capture = Captor {
                inner: delegate,
                result: Arc::clone(&result),
                calls: Arc::clone(&calls),
            };
            let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).unwrap());
            let mut agent = Agent::builder()
                .runtime_config(config, "ollama")
                .provider(Box::new(Parent))
                .tools(vec![Box::new(capture)])
                .memory(memory)
                .observer(Arc::new(crate::observability::NoopObserver {}))
                .tool_dispatcher(Box::new(crate::agent::dispatcher::NativeToolDispatcher))
                .build()
                .unwrap();
            let actual_turn = turn(&mut agent).await;
            *turn_state.lock() = Some(actual_turn.is_ok());
            drop(agent);
            actual_turn
        })
    }))
    .catch_unwind()
    .await;
    // Panics drop Agent/work scopes before reaching here. Their real Drop-drain tasks
    // retain a completion witness; wait for those tasks' end, not merely Arc retention.
    recorder.wait_for_completion().await;
    let actual_result = result.lock().take();
    let actual_calls = calls.load(Ordering::SeqCst);
    let depth_limit = actual_result
        .as_ref()
        .and_then(|r| r.error.as_deref())
        .is_some_and(|error| error.contains("depth limit"));
    recorder.fixture_result(crate::runtime::turn_cleanup::diagnostic::FixtureSummary {
        calls: actual_calls,
        result_present: actual_result.is_some(),
        success: actual_result.as_ref().map(|r| r.success),
        depth_limit,
        turn_ok: *turn_state.lock(),
        panicked: outcome.is_err(),
    });
    let flushed = recorder.flush();
    if flushed.is_err() {
        eprintln!("owned cleanup diagnostic flush failed");
    }
    let actual_turn = match outcome {
        Err(panic) => std::panic::resume_unwind(panic),
        Ok(actual_turn) => actual_turn,
    };
    // Keep the genuine parent Err intact, including when diagnostic flushing failed.
    actual_turn?;
    flushed?;
    assert_eq!(actual_calls, 1, "actual depth fixture backend executes exactly once");
    let actual_result = actual_result.expect("actual depth fixture result absent");
    assert!(!actual_result.success, "delegation at max depth should fail");
    assert!(depth_limit, "actual denial must preserve depth boundary");
    Ok(())
}
