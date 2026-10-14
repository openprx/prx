//! Publish one ingress-owned capability generation through the shared runtime.
use crate::tools;
use anyhow::Result;
use std::sync::Arc;

pub(crate) fn surface_policy(
    surface: crate::causal_tree::state::TurnSurfaceKind,
    configured: &crate::config::ToolTieringConfig,
) -> crate::tools::capability::catalog::SurfacePolicy {
    let channel_surface = surface == crate::causal_tree::state::TurnSurfaceKind::Channel;
    let tiering = if channel_surface {
        std::borrow::Cow::Owned(configured.for_channel_surface())
    } else {
        std::borrow::Cow::Borrowed(configured)
    };
    let excluded_names = tiering.always_exclude.iter().cloned().collect();
    let mut direct_names: std::collections::BTreeSet<_> = tiering
        .always_include
        .iter()
        .filter(|name| !tiering.always_exclude.contains(name))
        .cloned()
        .collect();
    // Interactive Chat is itself the local session-orchestration
    // entrypoint: `/sessions` observes work started by the model-facing
    // `sessions_spawn` tool. Keep that one bounded orchestration schema in
    // its direct core when policy permits it. Other surfaces must opt in
    // through `always_include`, and `always_exclude` still wins here.
    if surface == crate::causal_tree::state::TurnSurfaceKind::Interactive
        && !tiering.always_exclude.iter().any(|name| name == "sessions_spawn")
    {
        direct_names.insert("sessions_spawn".to_owned());
    }
    crate::tools::capability::catalog::SurfacePolicy {
        allowed_tools: None,
        excluded_names,
        direct_names,
        channel_allowed: channel_surface,
    }
}

pub(crate) struct TurnDecisionPublication {
    pub(crate) generation: Arc<crate::config::ConfigGeneration>,
    pub(crate) registry: Arc<Vec<Box<dyn tools::Tool>>>,
    pub(crate) mcp_tool: Option<Arc<tools::McpTool>>,
    pub(crate) security: Arc<crate::security::SecurityPolicy>,
    pub(crate) observer: Arc<dyn crate::observability::Observer>,
    pub(crate) hooks: Arc<crate::hooks::HookManager>,
    pub(crate) memory: Arc<dyn crate::memory::Memory>,
    pub(crate) plugin_generation: u64,
}

impl TurnDecisionPublication {
    pub(crate) async fn publish(
        self,
        surface: crate::causal_tree::state::TurnSurfaceKind,
    ) -> Result<Arc<super::turn_decision::TurnDecisionService>> {
        self.publish_with_memory_loader(surface, None).await
    }

    pub(crate) async fn publish_with_memory_loader(
        self,
        surface: crate::causal_tree::state::TurnSurfaceKind,
        loader: Option<Arc<dyn crate::agent::memory_loader::MemoryLoader>>,
    ) -> Result<Arc<super::turn_decision::TurnDecisionService>> {
        let registry = &self.registry;
        let config_generation = &self.generation;
        let config = &config_generation.effective;
        let security = &self.security;
        let observer = &self.observer;
        let hooks = &self.hooks;
        let memory = &self.memory;
        let mcp_tool = &self.mcp_tool;
        let mcp = mcp_tool.as_ref();
        let mcp_snapshot = match mcp {
            Some(mcp) => {
                // Synchronize local metadata without invoking a remote tool.
                mcp.refresh_catalog_config();
                mcp.capability_snapshot()
            }
            None => {
                // A static injected registry does not need an artificial MCP
                // instance. An executable MCP root still requires its actual
                // metadata owner, including roots hidden behind an alias.
                anyhow::ensure!(
                    !registry.iter().any(|tool| tool.name() == "mcp_call"
                        || tool.supports_name("mcp_call")
                        || tool.catalog_specs().iter().any(|spec| spec.name == "mcp_call")),
                    "decision publication requires its registry MCP instance"
                );
                Vec::new()
            }
        };
        let mcp_signature = |items: &[crate::tools::mcp::McpCapabilitySpec]| -> Result<Vec<_>> {
            items
                .iter()
                .map(|item| {
                    Ok((
                        item.server.clone(),
                        item.tool.clone(),
                        item.state_digest,
                        serde_json::to_value(&item.spec)?,
                    ))
                })
                .collect()
        };
        let before = mcp_signature(&mcp_snapshot)?;
        let sources = crate::tools::capability::catalog::CatalogSources::from_runtime_inputs(
            config_generation.id.0.to_string(),
            &config,
            mcp_snapshot,
            &hooks.diagnostics(),
            self.plugin_generation,
        );
        let surface_policy = surface_policy(surface, &config.tool_tiering);
        let execution = Arc::new(
            tools::ToolExecutionService::from_shared_boxed_registry(
                Arc::clone(registry),
                Arc::new(tools::SecurityEffectPolicy::new(Arc::clone(&security))),
                Arc::new(tools::DenyApprovalStrategy),
                Arc::new(tools::AdapterOwnedPreparation),
                Arc::new(tools::TracingToolExecutionAudit),
            )
            .with_idempotency_memory(Arc::clone(memory)),
        );
        let events: Arc<dyn crate::decision::telemetry::CausalEventSink> = Arc::new(
            crate::decision::telemetry::ObserverCausalSink::new(Arc::clone(&observer)),
        );
        let provider_catalog = crate::providers::selection::catalog_for(&config);
        let service = super::turn_decision::build_turn_decision_service(
            surface,
            super::turn_decision::TurnServiceBuild {
                parent_grant: None,
                generation: Arc::clone(&config_generation),
                registry: Arc::clone(registry),
                execution,
                catalog_sources: sources,
                surface_policy,
                provider_catalog: &provider_catalog,
                transports: &crate::decision::compatible_http::HttpDecisionTransports,
                events,
                memory: Arc::clone(memory),
                security: Arc::clone(&security),
            },
        )?;
        let service = service.with_loop_runtime(super::turn_decision::TurnLoopRuntime {
            hooks: Arc::clone(hooks),
            observer: Arc::clone(observer),
        })?;
        let service = service.with_memory_loader(loader)?;
        let after = match mcp.map_or_else(|| Ok(Vec::new()), |mcp| mcp_signature(&mcp.capability_snapshot())) {
            Ok(after) => after,
            Err(error) => {
                service
                    .shutdown(std::time::Instant::now() + std::time::Duration::from_secs(2))
                    .await;
                return Err(error);
            }
        };
        if before != after {
            service
                .shutdown(std::time::Instant::now() + std::time::Duration::from_secs(2))
                .await;
            anyhow::bail!("MCP capability state changed during decision publication");
        }
        super::ingress_recovery::register_service(&service)?;
        super::ingress_recovery::service_ready(&service).await?;
        super::control_ladder::report_decision_service_status(&service);
        Ok(service)
    }
}

/// A child publishes its own executable registry against the parent's sealed
/// catalog. No MCP refresh or auxiliary catalog enumeration occurs here.
pub(crate) struct ChildDecisionPublication {
    pub(crate) allowed_tools: Option<std::collections::BTreeSet<String>>,
    pub(crate) grant: Arc<super::child_source::IssuedChildGrant>,
    pub(crate) registry: Arc<Vec<Box<dyn tools::Tool>>>,
    pub(crate) observer: Arc<dyn crate::observability::Observer>,
}

impl ChildDecisionPublication {
    pub(crate) async fn publish(self) -> Result<Arc<super::turn_decision::TurnDecisionService>> {
        let owner = super::turn_cleanup::current_tracker()
            .ok_or_else(|| anyhow::anyhow!("child decision publication requires a cleanup owner"))?;
        let generation = Arc::clone(self.grant.generation());
        let config = &generation.effective;
        let security = Arc::clone(self.grant.security());
        let memory = self.grant.source_fabric().memory();
        let execution = Arc::new(
            tools::ToolExecutionService::from_shared_boxed_registry(
                Arc::clone(&self.registry),
                Arc::new(tools::SecurityEffectPolicy::new(Arc::clone(&security))),
                Arc::new(tools::DenyApprovalStrategy),
                Arc::new(tools::AdapterOwnedPreparation),
                Arc::new(tools::TracingToolExecutionAudit),
            )
            .with_idempotency_memory(Arc::clone(&memory)),
        );
        let surface_policy = crate::tools::capability::catalog::SurfacePolicy {
            allowed_tools: self.allowed_tools,
            excluded_names: config.tool_tiering.always_exclude.iter().cloned().collect(),
            direct_names: config
                .tool_tiering
                .always_include
                .iter()
                .filter(|name| !config.tool_tiering.always_exclude.contains(name))
                .cloned()
                .collect(),
            channel_allowed: false,
        };
        let events = Arc::new(crate::decision::telemetry::ObserverCausalSink::new(self.observer));
        let service = super::turn_decision::build_turn_decision_service(
            crate::causal_tree::state::TurnSurfaceKind::Worker,
            super::turn_decision::TurnServiceBuild {
                parent_grant: Some(Arc::clone(&self.grant)),
                generation: Arc::clone(&generation),
                registry: self.registry,
                execution,
                // Inheritance uses the exact parent catalog; these sources
                // carry only the generation check and are never rebuilt.
                catalog_sources: crate::tools::capability::catalog::CatalogSources {
                    config_generation_id: generation.id.0.to_string(),
                    ..Default::default()
                },
                surface_policy,
                provider_catalog: self.grant.provider_catalog(),
                transports: &crate::decision::compatible_http::HttpDecisionTransports,
                events,
                memory,
                security,
            },
        )?;
        if let Err(error) = owner.retain_child_service(Arc::clone(&service)) {
            service
                .shutdown(std::time::Instant::now() + std::time::Duration::from_secs(2))
                .await;
            return Err(error);
        }
        super::control_ladder::report_decision_service_status(&service);
        Ok(service)
    }
}

/// Publish an ordinary process against its restored catalog and the actual
/// local executor. The driver separately owns its authority lifetime guard.
pub(crate) struct OrdinaryProcessDecisionPublication {
    pub(crate) authority: Arc<super::child_source::ordinary_process::OrdinaryProcessAuthority>,
    pub(crate) registry: Arc<Vec<Box<dyn tools::Tool>>>,
    pub(crate) security: Arc<crate::security::SecurityPolicy>,
    pub(crate) runtime: super::turn_decision::TurnLoopRuntime,
    pub(crate) surface_policy: crate::tools::capability::catalog::SurfacePolicy,
}

impl OrdinaryProcessDecisionPublication {
    pub(crate) async fn publish(self) -> Result<Arc<super::turn_decision::TurnDecisionService>> {
        let owner = super::turn_cleanup::current_tracker()
            .ok_or_else(|| anyhow::anyhow!("ordinary process publication requires a cleanup owner"))?;
        self.authority.verify()?;
        let generation = Arc::clone(&self.authority.snapshots().generation);
        let memory = self.authority.source_fabric().memory();
        let execution = Arc::new(
            tools::ToolExecutionService::from_shared_boxed_registry(
                Arc::clone(&self.registry),
                Arc::new(tools::SecurityEffectPolicy::new(Arc::clone(&self.security))),
                Arc::new(tools::DenyApprovalStrategy),
                Arc::new(tools::AdapterOwnedPreparation),
                Arc::new(tools::TracingToolExecutionAudit),
            )
            .with_idempotency_memory(Arc::clone(&memory)),
        );
        let events = Arc::new(crate::decision::telemetry::ObserverCausalSink::new(Arc::clone(
            &self.runtime.observer,
        )));
        let service = super::turn_decision::TurnDecisionService::ordinary_process_service(
            Arc::clone(&self.authority),
            super::turn_decision::TurnServiceBuild {
                parent_grant: None,
                generation: Arc::clone(&generation),
                registry: self.registry,
                execution,
                catalog_sources: crate::tools::capability::catalog::CatalogSources {
                    config_generation_id: generation.id.0.to_string(),
                    ..Default::default()
                },
                surface_policy: self.surface_policy,
                provider_catalog: &self.authority.snapshots().provider_catalog,
                transports: &crate::decision::compatible_http::HttpDecisionTransports,
                events,
                memory,
                security: self.security,
            },
            self.runtime,
        )?;
        if let Err(error) = owner.retain_child_service(Arc::clone(&service)) {
            service
                .shutdown(std::time::Instant::now() + std::time::Duration::from_secs(2))
                .await;
            return Err(error);
        }
        super::control_ladder::report_decision_service_status(&service);
        Ok(service)
    }
}

/// Publish a worker from authenticated explicit operator authority.
pub(crate) struct OperatorProcessDecisionPublication {
    pub(crate) authority: Arc<super::child_source::operator_process::OperatorProcessAuthority>,
    pub(crate) registry: Arc<Vec<Box<dyn tools::Tool>>>,
    pub(crate) security: Arc<crate::security::SecurityPolicy>,
    pub(crate) runtime: super::turn_decision::TurnLoopRuntime,
    pub(crate) surface_policy: crate::tools::capability::catalog::SurfacePolicy,
}

impl OperatorProcessDecisionPublication {
    pub(crate) async fn publish(self) -> Result<Arc<super::turn_decision::TurnDecisionService>> {
        let owner = super::turn_cleanup::current_tracker()
            .ok_or_else(|| anyhow::anyhow!("operator process publication requires a cleanup owner"))?;
        self.authority.verify()?;
        let generation = Arc::clone(&self.authority.snapshots().generation);
        let memory = self.authority.source_fabric().memory();
        let execution = Arc::new(
            tools::ToolExecutionService::from_shared_boxed_registry(
                Arc::clone(&self.registry),
                Arc::new(tools::SecurityEffectPolicy::new(Arc::clone(&self.security))),
                Arc::new(tools::DenyApprovalStrategy),
                Arc::new(tools::AdapterOwnedPreparation),
                Arc::new(tools::TracingToolExecutionAudit),
            )
            .with_idempotency_memory(Arc::clone(&memory)),
        );
        let events = Arc::new(crate::decision::telemetry::ObserverCausalSink::new(Arc::clone(
            &self.runtime.observer,
        )));
        let service = super::turn_decision::TurnDecisionService::operator_process_service(
            Arc::clone(&self.authority),
            super::turn_decision::TurnServiceBuild {
                parent_grant: None,
                generation: Arc::clone(&generation),
                registry: self.registry,
                execution,
                catalog_sources: crate::tools::capability::catalog::CatalogSources {
                    config_generation_id: generation.id.0.to_string(),
                    ..Default::default()
                },
                surface_policy: self.surface_policy,
                provider_catalog: &self.authority.snapshots().provider_catalog,
                transports: &crate::decision::compatible_http::HttpDecisionTransports,
                events,
                memory,
                security: self.security,
            },
            self.runtime,
        )?;
        if let Err(error) = owner.retain_child_service(Arc::clone(&service)) {
            service
                .shutdown(std::time::Instant::now() + std::time::Duration::from_secs(2))
                .await;
            return Err(error);
        }
        super::control_ladder::report_decision_service_status(&service);
        Ok(service)
    }
}

#[cfg(test)]
mod static_publication_tests {
    use super::*;

    struct PublicationProbe {
        root: &'static str,
        mcp_alias: bool,
    }
    #[async_trait::async_trait]
    impl tools::Tool for PublicationProbe {
        fn name(&self) -> &str {
            self.root
        }
        fn description(&self) -> &str {
            "publication boundary fixture"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object","properties":{}})
        }
        fn supports_name(&self, name: &str) -> bool {
            name == self.root || (self.mcp_alias && name == "mcp_call")
        }
        async fn execute(&self, _: serde_json::Value) -> Result<tools::ToolResult> {
            anyhow::bail!("publication must not execute the fixture")
        }
    }

    fn publication(workspace: &std::path::Path, probe: PublicationProbe) -> TurnDecisionPublication {
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.to_owned();
        config.causal_tree.enabled = false;
        let security = Arc::new(crate::security::SecurityPolicy::from_config(
            &config.autonomy,
            workspace,
        ));
        TurnDecisionPublication {
            generation: crate::config::new_shared(config).pin(),
            registry: Arc::new(vec![Box::new(probe)]),
            mcp_tool: None,
            security,
            observer: Arc::new(crate::observability::noop::NoopObserver),
            hooks: Arc::new(crate::hooks::HookManager::new(workspace.to_owned())),
            memory: Arc::new(crate::memory::SqliteMemory::new(workspace).expect("fixture ledger")),
            plugin_generation: 0,
        }
    }

    #[tokio::test]
    async fn static_registry_publishes_without_a_synthetic_mcp_instance() {
        let workspace = tempfile::tempdir().unwrap();
        let service = publication(
            workspace.path(),
            PublicationProbe {
                root: "prx_probe",
                mcp_alias: false,
            },
        )
        .publish(crate::causal_tree::state::TurnSurfaceKind::AgentLoop)
        .await
        .expect("actual static registry publication");
        service.check_live_parity().expect("same executable registry");
        service
            .shutdown(std::time::Instant::now() + std::time::Duration::from_secs(2))
            .await;
    }

    #[tokio::test]
    async fn mcp_root_or_alias_requires_its_metadata_owner() {
        for probe in [
            PublicationProbe {
                root: "mcp_call",
                mcp_alias: false,
            },
            PublicationProbe {
                root: "prx_router",
                mcp_alias: true,
            },
        ] {
            let workspace = tempfile::tempdir().unwrap();
            let result = publication(workspace.path(), probe)
                .publish(crate::causal_tree::state::TurnSurfaceKind::AgentLoop)
                .await;
            assert!(matches!(result, Err(error) if error.to_string().contains("registry MCP instance")));
        }
    }
}
