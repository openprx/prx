#![cfg(test)]

//! V4.05: raw language does not grant the whole registry. A scripted model
//! supplies an exact catalog query; runtime still gates the returned offer.
//! Turns open prediction-free: discovery happens only through the model's
//! `capability_search` call, never through a decision step.
use super::*;
use crate::tools::capability::search_tool::{CapabilitySearchPort, CapabilitySearchTool, with_search_authority};

/// Whether the turn's execution-owned published surface carries the named
/// catalog capability.
fn publishes(service: &TurnDecisionService, context: &TurnDecisionContext, name: &str) -> bool {
    service.catalog().entry_for_public_name(name).is_some_and(|entry| {
        context
            .execution_refs()
            .iter()
            .any(|reference| reference.id == entry.id)
    })
}

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
            let service = build_publication(
                TurnSurfaceKind::AgentLoop,
                PublicationInputs {
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
                    publishes(&service, &context, "memory_recall"),
                    "{:?}",
                    context.execution_refs()
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
            let service = build_publication(
                TurnSurfaceKind::AgentLoop,
                PublicationInputs {
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
                    publishes(&service, &context, "docx"),
                    "{:?}",
                    context.execution_refs()
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
    let service = build_publication(
        TurnSurfaceKind::AgentLoop,
        PublicationInputs {
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
    assert!(context.is_run());
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
        let service = build_publication(
            TurnSurfaceKind::AgentLoop,
            PublicationInputs {
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
        assert!(publishes(&service, &context, "file_write"));
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

/// One prediction-free turn over the `file_read` / `skills_list` /
/// `skill_read` / `shell` registry. Discovery happens only through the
/// model's `capability_search` call; no decision step selects anything.
struct DiscoveryTurn {
    _workspace: tempfile::TempDir,
    memory: Arc<dyn Memory>,
    service: Arc<TurnDecisionService>,
    proof: crate::runtime::turn_source::OwnedTurnSourceProof,
    context: Arc<TurnDecisionContext>,
    dispatches: Arc<AtomicUsize>,
    transports: CountingTransports,
}

/// How a [`DiscoveryTurn`] is configured and admitted.
struct DiscoveryTurnSpec<'a> {
    autonomy: crate::security::AutonomyLevel,
    direct: &'a [&'a str],
    excluded: &'a [&'a str],
    peers: &'a [&'static str],
    plan: bool,
    input: &'a str,
    run: &'a str,
}

async fn discovery_turn(spec: DiscoveryTurnSpec<'_>) -> DiscoveryTurn {
    let workspace = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.decision_engine.enabled = true;
    config.causal_tree.backend.kind = crate::config::schema::causal_tree::BackendSelection::Fixed(
        crate::config::schema::causal_tree::BackendKind::Builtin,
    );
    config.workspace_dir = workspace.path().to_owned();
    config.causal_tree.enabled = true;
    config.causal_tree.policy.extra_latency_budget_ms = 5_000;
    config.autonomy.level = spec.autonomy;
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
    config
        .tool_tiering
        .always_exclude
        .extend(spec.excluded.iter().map(|name| (*name).to_owned()));
    let generation = crate::config::new_shared(config.clone()).pin();
    let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, workspace.path()));
    let dispatches = Arc::new(AtomicUsize::new(0));
    let mut tools: Vec<Box<dyn Tool>> = ["file_read", "skills_list", "skill_read", "shell"]
        .into_iter()
        .chain(spec.peers.iter().copied())
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
    let service = build_publication(
        if spec.plan {
            TurnSurfaceKind::Interactive
        } else {
            TurnSurfaceKind::AgentLoop
        },
        PublicationInputs {
            parent_grant: None,
            generation: Arc::clone(&generation),
            registry,
            execution,
            catalog_sources: sources(&generation, &config),
            surface_policy: SurfacePolicy {
                direct_names: spec.direct.iter().map(|name| (*name).to_owned()).collect(),
                excluded_names: config.tool_tiering.always_exclude.iter().cloned().collect(),
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
    let proof = if spec.plan {
        let (chat, _) = crate::chat::binding::tests::recorded_approval_source(
            &generation,
            Arc::clone(&memory),
            spec.input,
            crate::agent::turn_contracts::ChatMode::Plan,
            "prx_plan_discovery_run",
            1,
        )
        .await;
        crate::runtime::turn_source::OwnedTurnSourceProof::capture(chat.as_ref())
    } else {
        let cli = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_input(
            &generation,
            Arc::clone(&memory),
            spec.input,
            spec.run,
        )
        .await;
        crate::runtime::turn_source::OwnedTurnSourceProof::capture(&cli)
    };
    let context = service
        .open_issued_turn_context(&proof, AgentTurnKind::Repl, &CancellationToken::new())
        .await
        .unwrap();
    // Prediction-free opening: the turn runs, and the decision backend is
    // never asked to select a step.
    assert!(context.is_run());
    assert_eq!(service.prediction_port_calls(), 0);
    DiscoveryTurn {
        _workspace: workspace,
        memory,
        service,
        proof,
        context,
        dispatches,
        transports,
    }
}

/// The model's `capability_search` call: run the published search control
/// under the turn's search authority and return its hits.
async fn model_search(port: &CapabilitySearchPort, query: &str, limit: usize) -> Vec<serde_json::Value> {
    let result = with_search_authority(
        Some(port.clone()),
        CapabilitySearchTool.execute(serde_json::json!({"query": query, "limit": limit})),
    )
    .await
    .unwrap();
    assert!(result.success, "{query}: {:?}", result.error);
    let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();
    output.get("hits").unwrap().as_array().unwrap().clone()
}

fn appended_names(turn: &DiscoveryTurn) -> Vec<String> {
    turn.service
        .agent_appended_execution_specs(&turn.proof, &turn.context)
        .unwrap()
        .specs
        .into_iter()
        .map(|spec| spec.name)
        .collect()
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
            let turn = discovery_turn(DiscoveryTurnSpec {
                autonomy: crate::security::AutonomyLevel::ReadOnly,
                direct: &["file_read"],
                excluded: if exclude_target { &["skills_list"] } else { &[] },
                peers: &[],
                plan,
                input,
                run: "prx_discovery_surface",
            })
            .await;
            let DiscoveryTurn {
                service,
                proof,
                context,
                memory,
                ..
            } = &turn;
            let selected = service.agent_execution_specs(proof, context).unwrap();
            let core: Vec<_> = selected.specs.iter().map(|spec| spec.name.clone()).collect();
            assert_eq!(core, ["file_read"]);
            let original_refs = context.execution_refs().clone();
            assert!(
                service
                    .prepare_discovered_tool(proof, context, "skills_list")
                    .unwrap()
                    .is_none()
            );
            assert!(appended_names(&turn).is_empty());
            if plan {
                assert!(proof.intercepts_writes());
                assert!(
                    context
                        .inherited_constraint_codes()
                        .iter()
                        .any(|code| code.as_str() == "intercepts_writes")
                );
                // Plan intercepts the only deferred peer in this fixture.
                // The admitted core read remains available without search.
                assert!(
                    service
                        .agent_capability_search_spec(proof, context)
                        .expect("test: Plan search exposure")
                        .is_none()
                );
                assert!(
                    service
                        .admit_discovered_tool(proof, context, "skills_list")
                        .unwrap()
                        .is_none()
                );
                assert_eq!(context.execution_refs(), &original_refs);
                assert!(appended_names(&turn).is_empty());
                let event = memory
                    .find_owned_message_event(proof.workspace_id(), proof.owner_id(), proof.source_event_id().unwrap())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(event.content, input);
                context.terminal_execution_trace();
                assert_eq!(turn.dispatches.load(Ordering::SeqCst), 0);
                assert_eq!(turn.transports.0.load(Ordering::SeqCst), 0);
                service.shutdown(Instant::now() + Duration::from_secs(5)).await;
                continue;
            }
            let port = service.capability_search_port(proof, context).unwrap();
            let hits = model_search(&port, "skills_list", 1).await;
            assert_eq!(hits.len(), usize::from(!exclude_target));
            // A returned schema is still only an offer until admission.
            assert!(appended_names(&turn).is_empty());
            if !exclude_target {
                assert_eq!(
                    *hits.first().unwrap().get("schema").unwrap().get("name").unwrap(),
                    "skills_list"
                );
                let proposal = service
                    .admit_discovered_tool(proof, context, "skills_list")
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    proposal.admission,
                    crate::tools::surface_owner::publication::PublicationAdmission::Passed
                );
                let entries = service
                    .catalog
                    .resolve(std::slice::from_ref(&proposal.reference))
                    .unwrap();
                assert_eq!(
                    entries.first().unwrap().family,
                    crate::tools::capability::contract::CapabilityTarget::NativeTool
                );
                let appended = service.agent_appended_execution_specs(proof, context).unwrap();
                assert_eq!(
                    appended.specs.iter().map(|spec| spec.name.as_str()).collect::<Vec<_>>(),
                    ["skills_list"]
                );
                assert!(selected.specs.len() + appended.specs.len() < service.registry.len());
            } else {
                assert!(
                    service
                        .admit_discovered_tool(proof, context, "skills_list")
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
                    CapabilitySearchTool.execute(serde_json::json!({"query":"skills_list","limit":1})),
                )
                .await
                .is_err()
            );
            assert_eq!(turn.dispatches.load(Ordering::SeqCst), 0);
            assert_eq!(turn.transports.0.load(Ordering::SeqCst), 0);
            service.shutdown(Instant::now() + Duration::from_secs(5)).await;
        }
    }
}

/// A search hit is only an offer. `admit_offered_discoveries` puts it through
/// the same admission as a call by name and publishes it for the next request:
/// excluded, restricted or over-limit hits never become declared tools.
#[tokio::test]
async fn search_hits_are_admitted_for_the_next_request_through_the_same_gates() {
    for (autonomy, direct_shell) in [
        (crate::security::AutonomyLevel::ReadOnly, false),
        (crate::security::AutonomyLevel::Full, false),
        (crate::security::AutonomyLevel::ReadOnly, true),
        (crate::security::AutonomyLevel::Full, true),
    ] {
        let turn = discovery_turn(DiscoveryTurnSpec {
            autonomy,
            direct: if direct_shell {
                &["file_read", "shell"]
            } else {
                &["file_read"]
            },
            excluded: &[],
            peers: &[],
            plan: false,
            input: if direct_shell {
                "Use file_read and shell."
            } else {
                "could you make that sound a little friendlier please"
            },
            run: "prx_discovery_next_request",
        })
        .await;
        let DiscoveryTurn {
            service,
            proof,
            context,
            ..
        } = &turn;
        let full = autonomy == crate::security::AutonomyLevel::Full;
        let mut core_before: Vec<_> = service
            .agent_execution_specs(proof, context)
            .unwrap()
            .specs
            .iter()
            .map(|spec| spec.name.clone())
            .collect();
        core_before.sort();
        // Prediction-free publication: every Direct entry that passes the
        // gates is core. A direct `shell` passes only at Full autonomy.
        if direct_shell && full {
            assert_eq!(core_before, ["file_read", "shell"], "both direct tools are core");
        } else {
            assert_eq!(core_before, ["file_read"]);
        }
        let original_refs = context.execution_refs().clone();
        // Nothing has been searched, so there is nothing to admit.
        assert_eq!(service.admit_offered_discoveries(proof, context, |_, _| false), 0);

        let port = service.capability_search_port(proof, context).unwrap();
        // An exact query for an already-declared core must return that schema,
        // not fuzzy unrelated offers; it adds no new executable declaration.
        let hits = model_search(
            &port,
            "file_read",
            crate::tools::capability::search_tool::MAX_SEARCH_HITS,
        )
        .await;
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits.first().unwrap().get("schema").unwrap().get("name").unwrap(),
            "file_read"
        );
        assert_eq!(service.admit_offered_discoveries(proof, context, |_, _| false), 0);
        assert_eq!(context.execution_refs(), &original_refs);
        let queries = if direct_shell {
            vec!["shell"]
        } else {
            vec!["skills_list", "shell"]
        };
        for query in queries {
            let hits = model_search(&port, query, 1).await;
            // Read-only autonomy does not even make the process-execution
            // family discoverable.
            let expected = usize::from(query == "skills_list" || autonomy == crate::security::AutonomyLevel::Full);
            assert_eq!(hits.len(), expected, "{query}");
        }
        // The search itself declares nothing: the current request is unchanged.
        assert!(appended_names(&turn).is_empty());
        // An exclusion (here the inherited channel exclusion) blocks the hit.
        assert_eq!(
            service.admit_offered_discoveries(proof, context, |name, _| name == "skills_list" || name == "shell"),
            0
        );
        assert!(appended_names(&turn).is_empty());
        // Without the exclusion, every hit that passes the gates is declared.
        let admitted = service.admit_offered_discoveries(proof, context, |_, _| false);
        let appended = appended_names(&turn);
        if direct_shell {
            // At Full the `shell` hit is an offer of a tool that is already
            // core, so it admits nothing new; at read-only it is not even
            // offered.
            assert_eq!(admitted, 0, "a core shell offer is no new admission");
            assert!(appended.is_empty(), "{appended:?}");
        } else if autonomy == crate::security::AutonomyLevel::ReadOnly {
            assert_eq!(admitted, 1);
            assert_eq!(
                appended,
                ["skills_list"],
                "shell must not be admitted at read-only autonomy"
            );
        } else {
            assert_eq!(admitted, 2);
            assert_eq!(appended, ["shell", "skills_list"]);
        }
        // Admission is idempotent, never rewrites the turn's exact refs, and
        // dispatches nothing.
        assert_eq!(service.admit_offered_discoveries(proof, context, |_, _| false), 0);
        assert_eq!(context.execution_refs(), &original_refs);
        assert_eq!(turn.dispatches.load(Ordering::SeqCst), 0);
        context.terminal_execution_trace();
        assert_eq!(service.admit_offered_discoveries(proof, context, |_, _| false), 0);
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    }
}

/// Admission is bounded per turn across repeated searches and follows the
/// order the searches returned hits, not the alphabet. The production cap is
/// reached by real searches and admissions, not by seeding turn state.
#[tokio::test]
async fn discovered_tools_are_capped_per_turn_and_admitted_in_search_rank_order() {
    // Eight further deferred peers: with `skills_list` and `shell` they fill
    // the production cap of ten; `skill_read` is then the eleventh.
    const PEERS: [&str; 8] = [
        "prx_peer_a",
        "prx_peer_b",
        "prx_peer_c",
        "prx_peer_d",
        "prx_peer_e",
        "prx_peer_f",
        "prx_peer_g",
        "prx_peer_h",
    ];
    let turn = discovery_turn(DiscoveryTurnSpec {
        autonomy: crate::security::AutonomyLevel::Full,
        direct: &["file_read"],
        excluded: &[],
        peers: &PEERS,
        plan: false,
        input: "could you make that sound a little friendlier please",
        run: "prx_discovery_cap",
    })
    .await;
    let DiscoveryTurn {
        service,
        proof,
        context,
        ..
    } = &turn;
    let original_refs = context.execution_refs().clone();
    let port = service.capability_search_port(proof, context).unwrap();
    // `skills_list` ranks first although `shell` sorts first alphabetically.
    for name in ["skills_list", "shell"] {
        let hits = model_search(&port, name, 1).await;
        assert_eq!(hits.len(), 1, "{name}");
    }
    // A turn cap of one, counted across both searches' offers and every pass.
    let mut admitted = 0;
    for _ in 0..3 {
        admitted += service.admit_offered_discoveries_up_to(proof, context, 1, |_, _| false);
    }
    assert_eq!(admitted, 1);
    assert_eq!(
        appended_names(&turn),
        ["skills_list"],
        "the first hit in search order, not alphabetically"
    );
    // The production cap: ten per turn, counted over every admission.
    let core = {
        let entry = service
            .catalog
            .entry_for_public_name("file_read")
            .expect("catalog entry");
        CapabilityRef {
            id: entry.id.clone(),
            fingerprint: entry.fingerprint,
            generation: entry.generation,
        }
    };
    assert!(context.execution_refs().contains(&core));
    // A core capability offered again is never counted as newly admitted;
    // the pending `shell` offer is the only new admission.
    let hits = model_search(&port, "file_read", 1).await;
    assert_eq!(hits.len(), 1);
    assert_eq!(service.admit_offered_discoveries(proof, context, |_, _| false), 1);
    assert!(context.runtime().discovery.admission("file_read").is_none());
    assert!(context.runtime().discovery.admission("shell").is_some());
    assert_eq!(crate::tools::capability::search_tool::MAX_DISCOVERED_TOOLS_PER_TURN, 10);
    for peer in PEERS {
        let hits = model_search(&port, peer, 1).await;
        assert_eq!(hits.len(), 1, "{peer}");
        assert_eq!(hits.first().unwrap().get("schema").unwrap().get("name").unwrap(), peer);
    }
    // At most `MAX_SEARCH_HITS` new admissions per pass: two passes admit the
    // eight peers and bring the turn to the production cap.
    let mut filled = 0;
    for _ in 0..2 {
        filled += service.admit_offered_discoveries(proof, context, |_, _| false);
    }
    assert_eq!(filled, PEERS.len());
    assert_eq!(
        appended_names(&turn).len(),
        crate::tools::capability::search_tool::MAX_DISCOVERED_TOOLS_PER_TURN
    );
    let hits = model_search(&port, "skill_read", 1).await;
    assert_eq!(hits.len(), 1);
    assert_eq!(
        service.admit_offered_discoveries(proof, context, |_, _| false),
        0,
        "the production cap refuses an eleventh admission"
    );
    assert!(context.runtime().discovery.admission("skill_read").is_none());
    // Control: the same offer passes once the cap is raised.
    assert_eq!(
        service.admit_offered_discoveries_up_to(proof, context, 11, |_, _| false),
        1
    );
    assert!(context.runtime().discovery.admission("skill_read").is_some());
    assert_eq!(context.execution_refs(), &original_refs);
    assert_eq!(turn.dispatches.load(Ordering::SeqCst), 0);
    context.terminal_execution_trace();
    service.shutdown(Instant::now() + Duration::from_secs(5)).await;
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
    config.decision_engine.enabled = true;
    config.causal_tree.backend.kind = crate::config::schema::causal_tree::BackendSelection::Fixed(
        crate::config::schema::causal_tree::BackendKind::Builtin,
    );
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
    let service = build_publication(
        TurnSurfaceKind::AgentLoop,
        PublicationInputs {
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
        "inspect the available release metadata capability",
        "prx_skill_discovery",
    )
    .await;
    let cancellation = CancellationToken::new();
    let context = service
        .open_issued_turn_context(&proof, AgentTurnKind::Repl, &cancellation)
        .await
        .unwrap();
    // Prediction-free opening: nothing selects the Skill before the model's
    // own search.
    assert!(context.is_run());
    assert_eq!(service.prediction_port_calls(), 0);
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
    assert_eq!(
        proposal.admission,
        crate::tools::surface_owner::publication::PublicationAdmission::Passed
    );
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
