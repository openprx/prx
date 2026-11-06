#![cfg(test)]
//! Decision-free ingress model and publication admission. These tests reach the
//! production turn service (`AdmittedTurn`) and, for the refusal path, `run_turn`
//! itself, instead of rebuilding the surface by hand.
use super::*;
use crate::agent::turn_contracts::ChatMode;
use crate::agent::turn_engine::run_turn;
use crate::agent::turn_request::{TurnDelivery, TurnRequest, TurnSchedule, TurnServices};
use crate::agent::turn_tests::admission::AdmittedTurn;
use crate::tools::{ToolExecutionCommand, ToolExecutionStatus};

fn workspace_config(workspace: &std::path::Path) -> crate::config::Config {
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.to_path_buf();
    config
}

fn project_tool_registry(dispatches: &Arc<Mutex<Vec<String>>>, names: &[&str]) -> Arc<Vec<Box<dyn Tool>>> {
    Arc::new(
        names
            .iter()
            .map(|name| {
                Box::new(RecordingTool {
                    name: (*name).to_owned(),
                    execution_order: Arc::clone(dispatches),
                }) as Box<dyn Tool>
            })
            .collect(),
    )
}

struct SurfaceProvider {
    sends: AtomicUsize,
}
#[async_trait]
impl Provider for SurfaceProvider {
    async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
        anyhow::bail!("fixture requires a structured request")
    }
    async fn chat(&self, request: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        assert!(request.tools.expect("explicit publication").is_empty());
        Ok(ChatResponse {
            text: Some("ok".into()),
            tool_calls: Vec::new(),
            reasoning_content: None,
        })
    }
}

/// An execution model outside the ingress binding is refused by the real turn,
/// before any provider request or tool dispatch.
#[tokio::test]
async fn execution_model_differs_from_bound_model_is_refused() {
    let workspace = tempfile::tempdir().expect("test workspace");
    let dispatches = Arc::new(Mutex::new(Vec::new()));
    let registry = project_tool_registry(&dispatches, &["project_tool"]);
    let admitted = AdmittedTurn::with_config(workspace_config(workspace.path()), "reply", Arc::clone(&registry)).await;
    let decision = admitted.decision();
    assert!(
        decision
            .service
            .validate_agent_execution_model(decision.proof, "ollama", "prx-local-model")
            .is_ok(),
        "the ingress binding must admit itself"
    );
    for (provider_name, model) in [("ollama", "prx-other-model"), ("openai", "prx-local-model")] {
        assert!(
            decision
                .service
                .validate_agent_execution_model(decision.proof, provider_name, model)
                .is_err(),
            "{provider_name}/{model} is outside the ingress binding"
        );
        let provider = NativeScriptedProvider::new(vec![ChatResponse {
            text: Some("done".into()),
            tool_calls: Vec::new(),
            reasoning_content: None,
        }]);
        let mut history = vec![ChatMessage::system("sys"), ChatMessage::user("reply")];
        let result = run_turn(
            &mut history,
            TurnRequest {
                advice: None,
                channel_name: "cli",
                scope: None,
                mode: ChatMode::default(),
                silent: true,
                expose_stay_silent: false,
                multimodal: &crate::config::MultimodalConfig::default(),
                schedule: TurnSchedule {
                    read_only_tool_concurrency_window: 1,
                    priority_scheduling_enabled: false,
                    low_priority_tool_names: Vec::new(),
                },
                compaction: None,
                decision: admitted.decision(),
            },
            TurnServices {
                provider: &provider,
                provider_name,
                model,
                temperature: 0.0,
                tools_registry: Arc::clone(&registry),
                observer: &crate::observability::NoopObserver,
                hooks: &crate::hooks::HookManager::new(workspace.path().to_path_buf()),
                approval: None,
                approval_resolver: None,
                runtime_adapter: Some(admitted.adapter()),
                memory: admitted.memory_runtime(),
            },
            TurnDelivery {
                cancellation: None,
                on_delta: None,
                on_tool_call: None,
            },
        )
        .await;
        assert!(result.is_err(), "run_turn accepted {provider_name}/{model}");
        assert!(provider.requests().is_empty(), "a refused model reached the provider");
    }
    assert!(dispatches.lock().unwrap().is_empty());
}

/// A model without verified native tools gets an empty surface from the
/// production builder, so the provider sees no tools and a guessed name is denied.
#[tokio::test]
async fn model_without_native_tools_gets_no_tool_surface() {
    let workspace = tempfile::tempdir().expect("test workspace");
    let mut config = workspace_config(workspace.path());
    config.providers.models.push(
        serde_json::from_value(serde_json::json!({
            "provider":"ollama", "model":"prx-local-model", "operations":["chat"], "native_tools":false,
        }))
        .expect("fixture model descriptor"),
    );
    let dispatches = Arc::new(Mutex::new(Vec::new()));
    let registry = project_tool_registry(&dispatches, &["project_tool"]);
    let admitted = AdmittedTurn::with_config(config, "reply", registry).await;
    let decision = admitted.decision();
    let surface = decision.service.turn_tool_surface(decision.proof, &[]);
    assert!(!surface.contains("project_tool"));
    let adapter = admitted.adapter();
    let service = adapter
        .tool_execution_service
        .clone()
        .expect("fixture execution service");
    let specs: Vec<crate::tools::ToolSpec> = service
        .descriptors()
        .into_iter()
        .filter(|descriptor| surface.contains(&descriptor.public_name))
        .map(|descriptor| crate::tools::ToolSpec {
            name: descriptor.public_name,
            description: descriptor.description,
            parameters: descriptor.parameters,
        })
        .collect();
    assert!(specs.is_empty(), "no native-tool model may receive a tool schema");
    let provider = SurfaceProvider {
        sends: AtomicUsize::new(0),
    };
    provider
        .chat(
            ChatRequest {
                messages: &[ChatMessage::user("reply")],
                tools: Some(&specs),
            },
            "prx-local-model",
            0.0,
        )
        .await
        .expect("provider call with an empty surface");
    let mut context = adapter.tool_execution_context.clone();
    assert!(context.execution_bindings.is_none());
    context.turn_surface = Some(Arc::new(surface));
    let outcome = service
        .execute(
            ToolExecutionCommand::new("project_tool", serde_json::json!({})),
            context,
            None,
        )
        .await;
    assert_eq!(outcome.status, ToolExecutionStatus::Denied);
    assert_eq!(provider.sends.load(Ordering::SeqCst), 1);
    assert!(dispatches.lock().unwrap().is_empty());
}

/// A `hint:` route executes a different spelling, but the operator allowlist is
/// keyed on the model pinned at ingress.
#[tokio::test]
async fn hint_routed_turn_applies_the_pinned_model_allowlist() {
    let workspace = tempfile::tempdir().expect("test workspace");
    let mut config = workspace_config(workspace.path());
    config.model_routes = vec![crate::config::ModelRouteConfig {
        settings: Default::default(),
        hint: "fast".to_string(),
        provider: "ollama".to_string(),
        model: "prx-local-model".to_string(),
        api_key: None,
        credential_ref: None,
    }];
    // The route destination must verify NativeTools, the same as a pinned turn.
    config.providers.models.push(
        serde_json::from_value(serde_json::json!({
            "provider":"ollama", "model":"prx-local-model", "operations":["chat"], "native_tools":true,
        }))
        .expect("fixture route destination descriptor"),
    );
    config
        .tool_tiering
        .model_allowlists
        .insert("prx-local-model".into(), vec!["project_tool".into()]);
    let dispatches = Arc::new(Mutex::new(Vec::new()));
    let registry = project_tool_registry(&dispatches, &["project_tool", "other_tool"]);
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace.path()).expect("test memory"));
    let admitted = AdmittedTurn::with_model(config, "reply", registry, memory, ("ollama", "hint:fast")).await;
    let decision = admitted.decision();
    assert_eq!(decision.proof.model_name(), "hint:fast");
    assert_eq!(decision.proof.model_binding().pinned_model(), "prx-local-model");
    assert!(
        decision.service.model_can_call_tools(decision.proof),
        "the route destination must verify NativeTools"
    );
    let surface = decision.service.turn_tool_surface(decision.proof, &[]);
    assert!(surface.contains("project_tool"), "the allowed tool was not published");
    assert!(
        !surface.contains("other_tool"),
        "the hint spelling bypassed the pinned model allowlist"
    );
}
