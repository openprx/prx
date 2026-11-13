#![cfg(test)]

//! V4.05: raw language does not grant the whole registry. A scripted model
//! supplies an exact catalog query; runtime still gates the returned offer.
use super::*;
use crate::tools::capability::search_tool::{CapabilitySearchTool, with_search_authority};

struct DiscoverySurfaceTool {
    name: &'static str,
    dispatches: Arc<AtomicUsize>,
}

#[tokio::test]
async fn named_memory_recall_becomes_ready_after_selected_discovery_without_publishing_peers() {
    crate::runtime::turn_decision::with_scored_prediction(async {
        for excluded in [false, true] {
            let workspace = tempfile::tempdir().unwrap();
            let mut config = crate::config::Config::default();
    config.decision_engine.enabled = true;
    config.causal_tree.backend.kind = crate::config::schema::causal_tree::BackendSelection::Fixed(crate::config::schema::causal_tree::BackendKind::Builtin);
            config.workspace_dir = workspace.path().to_owned();
            config.causal_tree.enabled = true;
            config.causal_tree.policy.extra_latency_budget_ms = 5_000;
            config.providers.models.push(
                toml::from_str(
                    "provider = \"ollama\"\nmodel = \"prx-local-model\"\noperations = [\"chat\"]\nnative_tools = true\n",
                )
                .unwrap(),
            );
            let generation = crate::config::new_shared(config.clone()).pin();
            let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).unwrap());
            memory
                .store(
                    "prx_recall_probe",
                    "durable fixture value",
                    crate::memory::MemoryCategory::Core,
                    None,
                )
                .await
                .unwrap();
            let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, workspace.path()));
            let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![
                // Production publishes this retrieval source alongside recall.
                // Omitting it hides the Retrieve -> Discover -> hop-limit failure.
                Box::new(crate::tools::memory_search::MemorySearchTool::new(
                    workspace.path().to_owned(),
                    Arc::clone(&memory),
                    false,
                )),
                Box::new(crate::tools::memory_recall::MemoryRecallTool::new(
                    Arc::clone(&memory),
                    false,
                )),
                Box::new(crate::tools::memory_get::MemoryGetTool::new(
                    workspace.path().to_owned(),
                    Arc::clone(&memory),
                    false,
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
            let service = build_turn_decision_service(
                TurnSurfaceKind::AgentLoop,
                TurnServiceBuild {
                    parent_grant: None,
                    generation: Arc::clone(&generation),
                    registry,
                    execution,
                    catalog_sources: sources(&generation, &config),
                    surface_policy: SurfacePolicy {
                        excluded_names: if excluded {
                            BTreeSet::from(["memory_recall".to_owned()])
                        } else {
                            BTreeSet::new()
                        },
                        ..SurfacePolicy::default()
                    },
                    provider_catalog: &provider_catalog,
                    transports: &transports,
                    events: Arc::new(SilentEvents),
                    memory: Arc::clone(&memory),
                    security,
                },
            )
            .await
            .unwrap();
            let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_selection(
                &generation,
                Arc::clone(&memory),
                "Use memory_recall to search durable memory for prx_recall_probe and quote its stored content. Do not use workspace files or guess.",
                "prx_named_memory_discovery",
                ("ollama", "prx-local-model"),
            ).await;
            let context = service
                .open_issued_turn_context(&proof, AgentTurnKind::Repl, &CancellationToken::new())
                .await
                .unwrap();
            let published = service.agent_execution_specs(&proof, &context);
            if excluded {
                assert!(published.is_err() || published.unwrap().specs.iter().all(|spec| spec.name != "memory_recall"));
            } else {
                assert!(
                    matches!(context.directive(), TurnExecutionDirective::Execute { .. }),
                    "{:?}",
                    context.directive()
                );
                let specs = published.unwrap();
                assert!(specs.specs.iter().any(|spec| spec.name == "memory_recall"));
                assert!(
                    specs.specs.iter().all(|spec| spec.name != "memory_get"),
                    "discovery is one exact ref, not a family grant"
                );
                assert!(specs.specs.iter().all(|spec| spec.name != "memory_search"));
                assert!(
                    context
                        .engine()
                        .state
                        .observed
                        .completed_steps()
                        .iter()
                        .any(|step| step.kind() == ObservedStepKind::Discover && step.status() == ObservedStepStatus::Ok)
                );
                assert_eq!(context.hop_ids().len(), 2, "discover, then execute the exact ref");
            }
            assert_eq!(transports.0.load(Ordering::SeqCst), 0);
            context.terminal_execution_trace();
            service.shutdown(Instant::now() + Duration::from_secs(5)).await;
        }
    }).await;
}

#[tokio::test]
async fn named_skill_and_loader_select_the_skill_without_competing_targets() {
    crate::runtime::turn_decision::with_scored_prediction(async {
        for (excluded, shell_excluded) in [(false, false), (true, false), (false, true)] {
            let workspace = tempfile::tempdir().unwrap();
            let mut config = crate::config::Config::default();
    config.decision_engine.enabled = true;
    config.causal_tree.backend.kind = crate::config::schema::causal_tree::BackendSelection::Fixed(crate::config::schema::causal_tree::BackendKind::Builtin);
            config.workspace_dir = workspace.path().to_owned();
            config.causal_tree.enabled = true;
            config.causal_tree.policy.extra_latency_budget_ms = 5_000;
            config.providers.models.push(
                toml::from_str(
                    "provider = \"ollama\"\nmodel = \"prx-local-model\"\noperations = [\"chat\"]\nnative_tools = true\n",
                )
                .unwrap(),
            );
            let skill_dir = workspace.path().join("skills/docx");
            std::fs::create_dir_all(&skill_dir).unwrap();
            std::fs::write(
                skill_dir.join("SKILL.md"),
                "---\nname: docx\ndescription: Create Word documents\n---\nUse the installed Node document package.\n",
            )
            .unwrap();
            config.skills.open_skills_dir = Some(workspace.path().join("missing-open-skills").display().to_string());
            config.skills.openclaw_skills_dir =
                Some(workspace.path().join("missing-openclaw-skills").display().to_string());
            let generation = crate::config::new_shared(config.clone()).pin();
            let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).unwrap());
            let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, workspace.path()));
            let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![
                // Keep the real retrieval source that exposed the bounded-hop failure.
                // Omitting it hides the Retrieve -> Discover -> hop-limit failure.
                Box::new(crate::tools::memory_search::MemorySearchTool::new(
                    workspace.path().to_owned(),
                    Arc::clone(&memory),
                    false,
                )),
                Box::new(crate::tools::SkillReadTool::new(
                    workspace.path().to_owned(),
                    config.clone(),
                )),
                Box::new(DiscoverySurfaceTool {
                    name: "shell",
                    dispatches: Arc::new(AtomicUsize::new(0)),
                }),
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
            let service = build_turn_decision_service(
                TurnSurfaceKind::AgentLoop,
                TurnServiceBuild {
                    parent_grant: None,
                    generation: Arc::clone(&generation),
                    registry,
                    execution,
                    catalog_sources: sources(&generation, &config),
                    surface_policy: SurfacePolicy {
                        direct_names: BTreeSet::from(["shell".to_owned()]),
                        excluded_names: if excluded {
                            BTreeSet::from(["docx".to_owned()])
                        } else if shell_excluded {
                            BTreeSet::from(["shell".to_owned()])
                        } else {
                            BTreeSet::new()
                        },
                        ..SurfacePolicy::default()
                    },
                    provider_catalog: &provider_catalog,
                    transports: &transports,
                    events: Arc::new(SilentEvents),
                    memory: Arc::clone(&memory),
                    security,
                },
            )
            .await
            .unwrap();
            let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_selection(
                &generation,
                Arc::clone(&memory),
                "Use skill_read to read the installed docx skill, then follow it. Create named.docx with title Capability regression. Save a reusable implementation as named.js, generate the document, and verify the actual document text.",
                "prx_named_skill_loader",
                ("ollama", "prx-local-model"),
            ).await;
            let context = service
                .open_issued_turn_context(&proof, AgentTurnKind::Repl, &CancellationToken::new())
                .await
                .unwrap();
            let published = service.agent_execution_specs(&proof, &context);
            if excluded {
                assert!(published.is_err() || published.unwrap().specs.iter().all(|spec| spec.name != "docx"));
            } else {
                assert!(
                    matches!(context.directive(), TurnExecutionDirective::Execute { .. }),
                    "{:?}",
                    context.directive()
                );
                let specs = published.unwrap();
                assert!(
                    specs.specs.iter().all(|spec| spec.name != "shell"),
                    "a skill selection is not a shell grant"
                );
                let port = service.capability_search_port(&proof, &context).unwrap();
                let result = with_search_authority(
                    Some(port),
                    CapabilitySearchTool.execute(serde_json::json!({"query":"shell","limit":1})),
                )
                .await
                .unwrap();
                let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();
                let hits = output.get("hits").unwrap().as_array().unwrap();
                assert_eq!(
                    hits.len(),
                    usize::from(!shell_excluded),
                    "Skill discovery must preserve exclusions"
                );
                if !shell_excluded {
                    assert_eq!(
                        hits.first()
                            .unwrap()
                            .pointer("/schema/name")
                            .and_then(serde_json::Value::as_str),
                        Some("shell")
                    );
                }

                assert!(specs.specs.iter().any(|spec| spec.name == "docx"));
                assert!(
                    specs.specs.iter().all(|spec| spec.name != "skill_read"),
                    "publish the selected skill, not its generic loader"
                );
                assert!(specs.specs.iter().all(|spec| spec.name != "memory_search"));
                assert!(
                    context.hop_ids().len() <= 2,
                    "selection stays within the existing hop budget"
                );
            }
            assert_eq!(transports.0.load(Ordering::SeqCst), 0);
            context.terminal_execution_trace();
            service.shutdown(Instant::now() + Duration::from_secs(5)).await;
        }
    }).await;
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
async fn default_greeting_exposes_search_without_pre_admitting_hits() {
    let workspace = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.decision_engine.enabled = true;
    config.causal_tree.backend.kind = crate::config::schema::causal_tree::BackendSelection::Fixed(
        crate::config::schema::causal_tree::BackendKind::Builtin,
    );
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
    .await
    .unwrap();
    let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_input(
        &generation,
        Arc::clone(&memory),
        "hello",
        "prx_greeting_discovery",
    )
    .await;

    let context = service
        .open_issued_turn_context(&proof, AgentTurnKind::Repl, &CancellationToken::new())
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
            .is_some()
    );
    assert!(service.capability_search_port(&proof, &context).is_ok());
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
async fn selected_native_write_can_discover_read_peer_without_offering_shell() {
    crate::runtime::turn_decision::with_scored_prediction(async {
        let workspace = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.decision_engine.enabled = true;
        config.causal_tree.backend.kind = crate::config::schema::causal_tree::BackendSelection::Fixed(
            crate::config::schema::causal_tree::BackendKind::Builtin,
        );
        config.workspace_dir = workspace.path().to_owned();
        config.causal_tree.enabled = true;
        config.causal_tree.policy.extra_latency_budget_ms = 5_000;
        config.autonomy.level = crate::security::AutonomyLevel::Full;
        config.providers.models.push(
            toml::from_str(
                "provider = \"ollama\"\nmodel = \"prx-local-model\"\noperations = [\"chat\"]\nnative_tools = true\n",
            )
            .unwrap(),
        );
        let generation = crate::config::new_shared(config.clone()).pin();
        let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, workspace.path()));
        let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![
            Box::new(crate::tools::file_write::FileWriteTool::new(Arc::clone(&security))),
            Box::new(crate::tools::file_edit::FileEditTool::new(Arc::clone(&security))),
            Box::new(crate::tools::file_read::FileReadTool::new(Arc::clone(&security), false)),
            Box::new(crate::tools::shell::ShellTool::new(
                Arc::new(crate::runtime::NativeRuntime::new()),
                workspace.path().to_owned(),
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
        let service = build_turn_decision_service(
            TurnSurfaceKind::AgentLoop,
            TurnServiceBuild {
                parent_grant: None,
                generation: Arc::clone(&generation),
                registry,
                execution,
                catalog_sources: sources(&generation, &config),
                surface_policy: SurfacePolicy {
                    direct_names: BTreeSet::from([
                        "file_write".to_owned(),
                        "file_read".to_owned(),
                        "file_edit".to_owned(),
                        "shell".to_owned(),
                    ]),
                    ..SurfacePolicy::default()
                },
                provider_catalog: &provider_catalog,
                transports: &transports,
                events: Arc::new(SilentEvents),
                memory: Arc::clone(&memory),
                security,
            },
        )
        .await
        .unwrap();
        let cli = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_selection(
            &generation,
            memory,
            "Use file_write to create proof.txt with PRX marker, then read it back.",
            "prx_native_file_peer",
            ("ollama", "prx-local-model"),
        )
        .await;
        let proof = crate::runtime::turn_source::OwnedTurnSourceProof::capture(&cli);
        let context = service
            .open_issued_turn_context(&proof, AgentTurnKind::Repl, &CancellationToken::new())
            .await
            .unwrap();
        assert!(matches!(context.directive(), TurnExecutionDirective::Execute { .. }));
        let core = service.agent_execution_specs(&proof, &context).unwrap();
        assert!(core.specs.iter().any(|spec| spec.name == "file_write"));
        assert!(
            core.specs
                .iter()
                .all(|spec| spec.name != "file_read" && spec.name != "file_edit" && spec.name != "shell")
        );
        let port = service.capability_search_port(&proof, &context).unwrap();
        for (query, expected) in [
            ("file_read", Some("file_read")),
            ("file_edit", Some("file_edit")),
            ("shell", None),
        ] {
            let result = with_search_authority(
                Some(port.clone()),
                CapabilitySearchTool.execute(
                    serde_json::json!({"query":query,"limit":crate::tools::capability::search_tool::MAX_SEARCH_HITS}),
                ),
            )
            .await
            .unwrap();
            let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();
            let hits = output.get("hits").unwrap().as_array().unwrap();
            assert_eq!(hits.len(), usize::from(expected.is_some()));
            if let Some(expected) = expected {
                assert_eq!(
                    hits.first().unwrap().get("schema").unwrap().get("name").unwrap(),
                    expected
                );
            }
        }
        assert_eq!(service.admit_offered_discoveries(&proof, &context, |_, _| false), 2);
        let appended = service.agent_appended_execution_specs(&proof, &context).unwrap();
        let names: BTreeSet<_> = appended.specs.iter().map(|spec| spec.name.as_str()).collect();
        assert_eq!(names, BTreeSet::from(["file_read", "file_edit"]));
        assert!(
            !workspace.path().join("proof.txt").exists(),
            "metadata discovery executes nothing"
        );
        assert_eq!(transports.0.load(Ordering::SeqCst), 0);
        context.terminal_execution_trace();
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    })
    .await;
}

#[tokio::test]
async fn fixed_catalog_without_search_control_remains_valid() -> anyhow::Result<()> {
    for include_reader in [false, true] {
        let workspace = tempfile::tempdir()?;
        let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(workspace.path())?);
        let tools: Vec<Box<dyn Tool>> = if include_reader {
            vec![Box::new(crate::tools::memory_get::MemoryGetTool::new(
                workspace.path().to_owned(),
                memory,
                false,
            ))]
        } else {
            Vec::new()
        };
        let admitted =
            crate::agent::test_admission::AdmittedTurn::with_tools(workspace.path(), "hi", Arc::new(tools)).await;
        let decision = admitted.decision();
        let result = decision
            .service
            .agent_capability_search_spec(decision.proof, decision.context);
        anyhow::ensure!(result?.is_none(), "a fixed catalog must not invent a discovery control");
        decision
            .service
            .agent_execution_specs(decision.proof, decision.context)?;
    }
    Ok(())
}
