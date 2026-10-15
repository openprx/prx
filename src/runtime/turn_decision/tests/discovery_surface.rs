#![cfg(test)]

//! V4.05: raw language does not grant the whole registry. A scripted model
//! supplies an exact catalog query; runtime still gates the returned offer.
use super::*;
use crate::tools::capability::search_tool::{CapabilitySearchTool, with_search_authority};

struct DiscoverySurfaceTool {
    name: &'static str,
    dispatches: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for DiscoverySurfaceTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "Read project capability metadata"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{}})
    }
    async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.dispatches.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("discovery must not dispatch a business tool")
    }
}

#[tokio::test]
async fn default_greeting_keeps_production_discovery_closed() {
    let workspace = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_owned();
    config.cost.prices.insert(
        "prx-local-model".to_owned(),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    config.providers.models.push(
        toml::from_str(
            "provider = \"ollama\"\nmodel = \"prx-local-model\"\noperations = [\"chat\"]\nnative_tools = true\n",
        )
        .unwrap(),
    );
    let generation = crate::config::new_shared(config.clone()).pin();
    let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, workspace.path()));
    let dispatches = Arc::new(AtomicUsize::new(0));
    let mut tools: Vec<Box<dyn Tool>> = ["file_read", "skills_list", "skill_read", "shell"]
        .into_iter()
        .map(|name| {
            Box::new(DiscoverySurfaceTool {
                name,
                dispatches: Arc::clone(&dispatches),
            }) as Box<dyn Tool>
        })
        .collect();
    tools.push(Box::new(CapabilitySearchTool));
    let registry = Arc::new(tools);
    let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
        Arc::clone(&registry),
        Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
        Arc::new(DenyApprovalStrategy),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    ));
    let provider_catalog = ProviderCatalogView::from_config(&config);
    let transports = CountingTransports(AtomicUsize::new(0));
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).unwrap());
    let service = build_turn_decision_service(
        TurnSurfaceKind::AgentLoop,
        TurnServiceBuild {
            parent_grant: None,
            generation: Arc::clone(&generation),
            registry,
            execution,
            catalog_sources: sources(&generation, &config),
            surface_policy: SurfacePolicy {
                direct_names: BTreeSet::from(["file_read".to_owned()]),
                ..SurfacePolicy::default()
            },
            provider_catalog: &provider_catalog,
            transports: &transports,
            events: Arc::new(SilentEvents),
            memory: Arc::clone(&memory),
            security,
        },
    )
    .unwrap();
    let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_input(
        &generation,
        Arc::clone(&memory),
        "hello",
        "prx_greeting_discovery",
    )
    .await;

    let context = service
        .decide_agent_initial(&proof, AgentTurnKind::Repl, &CancellationToken::new())
        .await
        .unwrap();
    assert!(matches!(context.directive(), TurnExecutionDirective::AnswerDirectly));
    let selected = service.agent_execution_specs(&proof, &context).unwrap();
    assert_eq!(
        selected.specs.iter().map(|spec| spec.name.as_str()).collect::<Vec<_>>(),
        ["file_read"]
    );
    assert!(
        service
            .agent_capability_search_spec(&proof, &context)
            .unwrap()
            .is_none()
    );
    assert!(service.capability_search_port(&proof, &context).is_err());
    assert!(
        service
            .prepare_discovered_tool(&proof, &context, "skills_list")
            .unwrap()
            .is_none()
    );
    assert!(
        service
            .admit_discovered_tool(&proof, &context, "skills_list")
            .unwrap()
            .is_none()
    );
    assert!(
        service
            .agent_appended_execution_specs(&proof, &context)
            .unwrap()
            .specs
            .is_empty()
    );
    assert_eq!(dispatches.load(Ordering::SeqCst), 0);
    assert_eq!(transports.0.load(Ordering::SeqCst), 0);
    context.terminal_execution_trace();
    service.shutdown(Instant::now() + Duration::from_secs(5)).await;
}

#[tokio::test]
async fn non_english_discovery_reaches_admitted_family_without_full_registry() {
    // Preserve the three original F6 inputs, plus an unspaced-script request.
    let unspaced: String = [0x8bf7, 0x7f29, 0x77ed, 0x8fd9, 0x6bb5, 0x6587, 0x5b57]
        .into_iter()
        .map(|point| char::from_u32(point).expect("valid fixture scalar"))
        .collect();
    let inputs = [
        "haz que esto quede un poco m\u{e1}s breve",
        "\u{43f}\u{43e}\u{436}\u{430}\u{43b}\u{443}\u{439}\u{441}\u{442}\u{430}, \u{441}\u{43e}\u{43a}\u{440}\u{430}\u{442}\u{438} \u{44d}\u{442}\u{43e}",
        "could you make that sound a little friendlier please",
        unspaced.as_str(),
    ];
    for input in inputs {
        for (exclude_target, plan) in [(false, false), (true, false), (false, true)] {
            let workspace = tempfile::tempdir().unwrap();
            let mut config = crate::config::Config::default();
            config.workspace_dir = workspace.path().to_owned();
            config.causal_tree.enabled = true;
            config.causal_tree.policy.extra_latency_budget_ms = 5_000;
            config.autonomy.level = crate::security::AutonomyLevel::ReadOnly;
            config.cost.prices.insert(
                "prx-local-model".to_owned(),
                crate::config::schema::ModelPricing {
                    input: 1.0,
                    output: 1.0,
                    cache_write: 1.0,
                    cache_read: 1.0,
                },
            );
            config.providers.models.push(toml::from_str(
                "provider = \"ollama\"\nmodel = \"prx-local-model\"\noperations = [\"chat\"]\nnative_tools = true\n",
            ).unwrap());
            if exclude_target {
                config.tool_tiering.always_exclude.push("skills_list".to_owned());
            }
            let generation = crate::config::new_shared(config.clone()).pin();
            let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, workspace.path()));
            let dispatches = Arc::new(AtomicUsize::new(0));
            let mut tools: Vec<Box<dyn Tool>> = ["file_read", "skills_list", "skill_read", "shell"]
                .into_iter()
                .map(|name| {
                    Box::new(DiscoverySurfaceTool {
                        name,
                        dispatches: Arc::clone(&dispatches),
                    }) as Box<dyn Tool>
                })
                .collect();
            tools.push(Box::new(CapabilitySearchTool));
            let registry = Arc::new(tools);
            let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
                Arc::clone(&registry),
                Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
                Arc::new(DenyApprovalStrategy),
                Arc::new(AdapterOwnedPreparation),
                Arc::new(TracingToolExecutionAudit),
            ));
            let catalog = ProviderCatalogView::from_config(&config);
            let transports = CountingTransports(AtomicUsize::new(0));
            let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).unwrap());
            let mut service = build_turn_decision_service(
                if plan {
                    TurnSurfaceKind::Interactive
                } else {
                    TurnSurfaceKind::AgentLoop
                },
                TurnServiceBuild {
                    parent_grant: None,
                    generation: Arc::clone(&generation),
                    registry,
                    execution,
                    catalog_sources: sources(&generation, &config),
                    surface_policy: SurfacePolicy {
                        direct_names: BTreeSet::from(["file_read".to_owned()]),
                        excluded_names: config.tool_tiering.always_exclude.iter().cloned().collect(),
                        ..SurfacePolicy::default()
                    },
                    provider_catalog: &catalog,
                    transports: &transports,
                    events: Arc::new(SilentEvents),
                    memory: Arc::clone(&memory),
                    security,
                },
            )
            .unwrap();
            // Deterministic local decision fixture, not hosted-backend admission
            // or a claim about builtin multilingual semantic understanding.
            let predictions = Arc::new(AtomicUsize::new(0));
            let owning = Arc::get_mut(&mut service).unwrap();
            owning.factory = DecisionBackendFactory::from_slots(
                vec![Box::new(ExecuteContextProbeBackend {
                    identity: BackendIdentity {
                        kind: BackendKind::Mock,
                        provider: None,
                        configured_model: Arc::from("prx-local-model"),
                    },
                    calls: Arc::clone(&predictions),
                })],
                Arc::clone(&owning.state_key),
                OutboundSecrets::default(),
                SlotLimits::default(),
            );
            let proof = if plan {
                let chat = crate::chat::decision_source::admission_failure_tests::recorded_plan_source(
                    &generation,
                    Arc::clone(&memory),
                    input,
                )
                .await;
                crate::runtime::turn_source::OwnedTurnSourceProof::capture(chat.as_ref())
            } else {
                let cli = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_input(
                    &generation,
                    Arc::clone(&memory),
                    input,
                    "prx_discovery_surface",
                )
                .await;
                crate::runtime::turn_source::OwnedTurnSourceProof::capture(&cli)
            };
            let context = service
                .decide_agent_initial(&proof, AgentTurnKind::Repl, &CancellationToken::new())
                .await
                .unwrap();
            assert!(predictions.load(Ordering::SeqCst) > 0);
            assert!(matches!(context.directive(), TurnExecutionDirective::Execute { .. }));
            let selected = service.agent_execution_specs(&proof, &context).unwrap();
            let core: Vec<_> = selected.specs.iter().map(|spec| spec.name.clone()).collect();
            assert_eq!(core, ["file_read"]);
            let original_refs = context.execution_refs().clone();
            assert!(
                service
                    .prepare_discovered_tool(&proof, &context, "skills_list")
                    .unwrap()
                    .is_none()
            );
            assert!(
                service
                    .agent_appended_execution_specs(&proof, &context)
                    .unwrap()
                    .specs
                    .is_empty()
            );
            if plan {
                assert!(proof.intercepts_writes());
                assert!(
                    context
                        .inherited_constraint_codes()
                        .iter()
                        .any(|code| code.as_str() == "intercepts_writes")
                );
                // The existing Plan classifier excludes skills_list/skill_read
                // and shell. Direct file_read is not a discoverable family.
                assert!(
                    service
                        .agent_capability_search_spec(&proof, &context)
                        .unwrap()
                        .is_none()
                );
                assert!(service.capability_search_port(&proof, &context).is_err());
                assert!(
                    service
                        .admit_discovered_tool(&proof, &context, "skills_list")
                        .unwrap()
                        .is_none()
                );
                assert_eq!(context.execution_refs(), &original_refs);
                assert!(
                    service
                        .agent_appended_execution_specs(&proof, &context)
                        .unwrap()
                        .specs
                        .is_empty()
                );
                let event = memory
                    .find_owned_message_event(proof.workspace_id(), proof.owner_id(), proof.source_event_id().unwrap())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(event.content, input);
                context.terminal_execution_trace();
                assert_eq!(dispatches.load(Ordering::SeqCst), 0);
                assert_eq!(transports.0.load(Ordering::SeqCst), 0);
                service.shutdown(Instant::now() + Duration::from_secs(5)).await;
                continue;
            }
            let port = service.capability_search_port(&proof, &context).unwrap();
            let result = with_search_authority(
                Some(port.clone()),
                CapabilitySearchTool.execute(serde_json::json!({"query":"skills_list","limit":1})),
            )
            .await
            .unwrap();
            assert!(result.success);
            let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();
            let hits = output.get("hits").unwrap().as_array().unwrap();
            assert_eq!(hits.len(), usize::from(!exclude_target));
            // A returned schema is still only an offer until admission.
            assert!(
                service
                    .agent_appended_execution_specs(&proof, &context)
                    .unwrap()
                    .specs
                    .is_empty()
            );
            if !exclude_target {
                assert_eq!(
                    *hits.first().unwrap().get("schema").unwrap().get("name").unwrap(),
                    "skills_list"
                );
                let proposal = service
                    .admit_discovered_tool(&proof, &context, "skills_list")
                    .unwrap()
                    .unwrap();
                assert_eq!(proposal.admission, crate::causal_tree::gates::GateOutcome::Passed);
                let entries = service
                    .catalog
                    .resolve(std::slice::from_ref(&proposal.reference))
                    .unwrap();
                assert_eq!(entries.first().unwrap().family, CapabilityTarget::NativeTool);
                let appended = service.agent_appended_execution_specs(&proof, &context).unwrap();
                assert_eq!(
                    appended.specs.iter().map(|spec| spec.name.as_str()).collect::<Vec<_>>(),
                    ["skills_list"]
                );
                assert!(selected.specs.len() + appended.specs.len() < service.registry.len());
            } else {
                assert!(
                    service
                        .admit_discovered_tool(&proof, &context, "skills_list")
                        .unwrap()
                        .is_none()
                );
            }
            assert_eq!(context.execution_refs(), &original_refs);
            let event = memory
                .find_owned_message_event(proof.workspace_id(), proof.owner_id(), proof.source_event_id().unwrap())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(event.content, input);
            context.terminal_execution_trace();
            assert!(
                with_search_authority(
                    Some(port),
                    CapabilitySearchTool.execute(serde_json::json!({"query":"skills_list","limit":1}),)
                )
                .await
                .is_err()
            );
            assert_eq!(dispatches.load(Ordering::SeqCst), 0);
            assert_eq!(transports.0.load(Ordering::SeqCst), 0);
            service.shutdown(Instant::now() + Duration::from_secs(5)).await;
        }
    }
}

#[tokio::test]
async fn production_discovery_selects_a_skill_and_loads_its_body_only_on_execution() {
    const SKILL_NAME: &str = "prx_skill";
    const BODY_MARKER: &str = "PRX-SKILL-BODY-MARKER";

    let workspace = tempfile::tempdir().unwrap();
    let skill_dir = workspace.path().join("skills").join(SKILL_NAME);
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: {SKILL_NAME}\ndescription: Inspect release metadata\n---\n{BODY_MARKER}\n"),
    )
    .unwrap();
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_owned();
    config.skills.open_skills_dir = Some(
        workspace
            .path()
            .join("missing-open-skills")
            .to_string_lossy()
            .to_string(),
    );
    config.skills.openclaw_skills_dir = Some(
        workspace
            .path()
            .join("missing-openclaw-skills")
            .to_string_lossy()
            .to_string(),
    );
    config.causal_tree.policy.extra_latency_budget_ms = 5_000;
    config.autonomy.level = crate::security::AutonomyLevel::ReadOnly;
    config.cost.prices.insert(
        "prx-local-model".to_owned(),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    config.providers.models.push(
        toml::from_str(
            "provider = \"ollama\"\nmodel = \"prx-local-model\"\noperations = [\"chat\"]\nnative_tools = true\n",
        )
        .unwrap(),
    );
    let generation = crate::config::new_shared(config.clone()).pin();
    let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, workspace.path()));
    let dispatches = Arc::new(AtomicUsize::new(0));
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![
        Box::new(DiscoverySurfaceTool {
            name: "file_read",
            dispatches: Arc::clone(&dispatches),
        }),
        Box::new(crate::tools::SkillReadTool::new(
            workspace.path().to_owned(),
            config.clone(),
        )),
        Box::new(CapabilitySearchTool),
    ]);
    let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
        Arc::clone(&registry),
        Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
        Arc::new(DenyApprovalStrategy),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    ));
    let provider_catalog = ProviderCatalogView::from_config(&config);
    let transports = CountingTransports(AtomicUsize::new(0));
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).unwrap());
    let mut service = build_turn_decision_service(
        TurnSurfaceKind::AgentLoop,
        TurnServiceBuild {
            parent_grant: None,
            generation: Arc::clone(&generation),
            registry,
            execution,
            catalog_sources: sources(&generation, &config),
            surface_policy: SurfacePolicy {
                direct_names: BTreeSet::from(["file_read".to_owned()]),
                ..SurfacePolicy::default()
            },
            provider_catalog: &provider_catalog,
            transports: &transports,
            events: Arc::new(SilentEvents),
            memory: Arc::clone(&memory),
            security,
        },
    )
    .unwrap();
    let owning = Arc::get_mut(&mut service).unwrap();
    owning.factory = DecisionBackendFactory::from_slots(
        vec![Box::new(ExecuteContextProbeBackend {
            identity: BackendIdentity {
                kind: BackendKind::Mock,
                provider: None,
                configured_model: Arc::from("prx-local-model"),
            },
            calls: Arc::new(AtomicUsize::new(0)),
        })],
        Arc::clone(&owning.state_key),
        OutboundSecrets::default(),
        SlotLimits::default(),
    );
    let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_input(
        &generation,
        Arc::clone(&memory),
        "inspect the available release metadata capability",
        "prx_skill_discovery",
    )
    .await;
    let cancellation = CancellationToken::new();
    let context = service
        .decide_agent_initial(&proof, AgentTurnKind::Repl, &cancellation)
        .await
        .unwrap();
    assert!(matches!(context.directive(), TurnExecutionDirective::Execute { .. }));
    let selected = service.agent_execution_specs(&proof, &context).unwrap();
    assert_eq!(
        selected.specs.iter().map(|spec| spec.name.as_str()).collect::<Vec<_>>(),
        ["file_read"]
    );

    let port = service.capability_search_port(&proof, &context).unwrap();
    let result = with_search_authority(
        Some(port),
        CapabilitySearchTool.execute(serde_json::json!({"query": SKILL_NAME, "limit": 1})),
    )
    .await
    .unwrap();
    assert!(result.success);
    assert!(
        !result.output.contains(BODY_MARKER),
        "discovery must keep the Skill body lazy"
    );
    let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();
    let hit = output
        .get("hits")
        .and_then(serde_json::Value::as_array)
        .and_then(|hits| hits.first())
        .unwrap();
    let schema = hit.get("schema").unwrap();
    assert_eq!(schema.get("name").and_then(serde_json::Value::as_str), Some(SKILL_NAME));
    assert_eq!(
        schema.get("description").and_then(serde_json::Value::as_str),
        Some("Inspect release metadata")
    );
    assert_eq!(hit.get("categories"), Some(&serde_json::json!(["system"])));
    assert_eq!(hit.get("effect").and_then(serde_json::Value::as_str), Some("read_only"));
    assert_eq!(hit.get("risk").and_then(serde_json::Value::as_str), Some("low"));
    assert_eq!(hit.get("approval").and_then(serde_json::Value::as_str), Some("none"));
    assert_eq!(
        hit.get("channel_eligible").and_then(serde_json::Value::as_bool),
        Some(false)
    );

    let envelope = crate::runtime::envelope::RuntimeEnvelope::agent(proof.workspace_id(), proof.run_id())
        .with_config_generation(&generation)
        .with_source_message_event_id(proof.source_event_id().unwrap());
    let execution_context =
        crate::tools::ToolExecutionContext::new(envelope, "private").with_chat_id(proof.session_key());
    let mut offered_but_unadmitted = selected.specs.clone();
    offered_but_unadmitted.push(serde_json::from_value(schema.clone()).unwrap());
    let bind_error = match service.bind_agent_execution(&proof, &context, &execution_context, &offered_but_unadmitted) {
        Ok(_) => panic!("a search offer must not create execution authority"),
        Err(error) => error,
    };
    assert!(
        bind_error.to_string().contains("unknown runtime dependency"),
        "unexpected pre-admission binding error: {bind_error:#}"
    );

    let proposal = service
        .admit_discovered_tool(&proof, &context, SKILL_NAME)
        .unwrap()
        .unwrap();
    assert_eq!(proposal.admission, crate::causal_tree::gates::GateOutcome::Passed);
    let appended = service.agent_appended_execution_specs(&proof, &context).unwrap();
    assert_eq!(
        appended.specs.iter().map(|spec| spec.name.as_str()).collect::<Vec<_>>(),
        [SKILL_NAME]
    );
    let mut published = selected.specs;
    published.extend(appended.specs);
    let (executor, bound) = service
        .bind_agent_execution(&proof, &context, &execution_context, &published)
        .unwrap();
    let outcome = executor
        .execute(
            crate::tools::ToolExecutionCommand::new(SKILL_NAME, serde_json::json!({"name": SKILL_NAME})),
            bound,
            Some(cancellation),
        )
        .await;
    assert!(
        outcome.succeeded(),
        "discovered Skill execution failed: {:?}",
        outcome.error
    );
    assert!(
        outcome.result.unwrap().output.contains(BODY_MARKER),
        "the selected Skill body must load through skill_read"
    );
    assert_eq!(dispatches.load(Ordering::SeqCst), 0);
    assert_eq!(transports.0.load(Ordering::SeqCst), 0);
    context.terminal_execution_trace();
    service.shutdown(Instant::now() + Duration::from_secs(5)).await;
}
