#![cfg(test)]
//! Decision-free ingress model and publication admission.
use super::*;
use crate::runtime::turn_model;
use crate::tools::execution::admission::TurnSurface;

fn source(native_tools: bool) -> (crate::config::Config, crate::agent::entry::AgentTurnProof) {
    let mut config = crate::config::Config::default();
    config.causal_tree.enabled = false;
    config.providers.models.push(
        serde_json::from_value(serde_json::json!({
            "provider":"ollama", "model":"prx-local-model", "operations":["chat"], "native_tools":native_tools,
        }))
        .unwrap(),
    );
    let generation = crate::config::ConfigGeneration::for_disabled_child(&config).unwrap();
    let proof = crate::agent::entry::test_cli_agent_turn_proof(&generation, "ollama", "prx-local-model");
    (config, proof)
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

#[tokio::test]
async fn execution_model_differs_from_bound_model_is_refused() {
    let (_, proof) = source(true);
    let provider = SurfaceProvider {
        sends: AtomicUsize::new(0),
    };
    let dispatches = Arc::new(Mutex::new(Vec::new()));
    let service = crate::tools::ToolExecutionService::with_security_policy(
        vec![Box::new(RecordingTool {
            name: "project_tool".into(),
            execution_order: Arc::clone(&dispatches),
        })],
        Arc::new(crate::security::SecurityPolicy::default()),
    );
    for (provider_name, model) in [("ollama", "prx-other-model"), ("openai", "prx-local-model")] {
        let attempted = async {
            turn_model::validate_execution(&proof, provider_name, model)?;
            provider
                .chat(
                    ChatRequest {
                        messages: &[],
                        tools: Some(&[]),
                    },
                    model,
                    0.0,
                )
                .await?;
            service
                .execute(
                    crate::tools::ToolExecutionCommand::new("project_tool", serde_json::json!({})),
                    crate::tools::ToolExecutionContext::new(
                        crate::runtime::envelope::RuntimeEnvelope::console("prx_workspace", "prx_session"),
                        "dm",
                    ),
                    None,
                )
                .await;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        assert!(attempted.is_err());
    }
    assert_eq!(provider.sends.load(Ordering::SeqCst), 0);
    assert!(dispatches.lock().unwrap().is_empty());
}

#[tokio::test]
async fn model_without_native_tools_gets_no_tool_surface() {
    let (config, proof) = source(false);
    let dispatches = Arc::new(Mutex::new(Vec::new()));
    let registry: Vec<Box<dyn Tool>> = vec![Box::new(RecordingTool {
        name: "project_tool".into(),
        execution_order: Arc::clone(&dispatches),
    })];
    let service = crate::tools::ToolExecutionService::with_security_policy(
        registry,
        Arc::new(crate::security::SecurityPolicy::default()),
    );
    let catalog = crate::providers::catalog::CatalogView::from_config(&config);
    let allowed = turn_model::can_call_tools(
        &config,
        &catalog,
        &proof,
        crate::providers::selection::credential_for(&config, "ollama"),
    );
    assert!(!allowed);
    let surface =
        TurnSurface::build(&service, &config.tool_tiering, "prx-local-model", false, |_| true).with_tool_calls(allowed);
    let specs: Vec<_> = service
        .descriptors()
        .into_iter()
        .filter(|descriptor| surface.contains(&descriptor.public_name))
        .map(|descriptor| crate::tools::ToolSpec {
            name: descriptor.public_name,
            description: descriptor.description,
            parameters: descriptor.parameters,
        })
        .collect();
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
        .unwrap();
    let mut context = crate::tools::ToolExecutionContext::new(
        crate::runtime::envelope::RuntimeEnvelope::console("prx_workspace", "prx_session"),
        "dm",
    );
    assert!(context.execution_bindings.is_none());
    context.turn_surface = Some(Arc::new(surface));
    let outcome = service
        .execute(
            crate::tools::ToolExecutionCommand::new("project_tool", serde_json::json!({})),
            context,
            None,
        )
        .await;
    assert_eq!(outcome.status, crate::tools::ToolExecutionStatus::Denied);
    assert_eq!(provider.sends.load(Ordering::SeqCst), 1);
    assert!(dispatches.lock().unwrap().is_empty());
}
