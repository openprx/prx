//! Publish one ingress-owned capability generation through the shared runtime.
//!
//! R26 (ruling R26-HOLDER): the publication builders live here. They build
//! the publication-scoped `TurnRuntimeHolder` and the `DecisionAdvisory`;
//! every turn then opens its own `TurnRuntimeContext` from that holder. The
//! assembled service remains until Part 2 deletes it.
mod build_error;
mod builder;
mod operator_process;
mod ordinary_process;

pub(crate) use build_error::PublicationBuildError;
pub(crate) use builder::{PublicationInputs, PublicationLoopRuntime, build_publication};

use crate::tools;
use anyhow::Result;
use std::sync::Arc;

/// The bounded native schemas kept on ordinary model turns. These are the
/// common file and shell operations from the standard progressive-disclosure
/// contract; publication still applies source, model, surface, security and
/// approval gates before any one of them can execute.
const DEFAULT_DIRECT_TOOLS: [&str; 4] = ["file_edit", "file_read", "file_write", "shell"];

fn direct_tool_names(configured: &crate::config::ToolTieringConfig) -> std::collections::BTreeSet<String> {
    DEFAULT_DIRECT_TOOLS
        .into_iter()
        .map(str::to_owned)
        .chain(configured.always_include.iter().cloned())
        .filter(|name| !configured.always_exclude.contains(name))
        .collect()
}

pub(crate) fn surface_policy(
    surface: crate::tools::capability::turn_state::TurnSurfaceKind,
    configured: &crate::config::ToolTieringConfig,
) -> crate::tools::capability::catalog::SurfacePolicy {
    let channel_surface = surface == crate::tools::capability::turn_state::TurnSurfaceKind::Channel;
    let tiering = if channel_surface {
        std::borrow::Cow::Owned(configured.for_channel_surface())
    } else {
        std::borrow::Cow::Borrowed(configured)
    };
    let excluded_names = tiering.always_exclude.iter().cloned().collect();
    let direct_names = direct_tool_names(&tiering);
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

/// The tool surface a targeted agent turn is published with: an optional
/// allowlist plus names removed from every allowlist, wildcard included.
#[derive(Clone, Debug, Default)]
pub(crate) struct TargetedSurface {
    pub(crate) allowed_tools: Option<std::collections::BTreeSet<String>>,
    pub(crate) excluded_names: std::collections::BTreeSet<String>,
}

impl TurnDecisionPublication {
    pub(crate) async fn publish(
        self,
        surface: crate::tools::capability::turn_state::TurnSurfaceKind,
    ) -> Result<Arc<super::turn_decision::TurnDecisionService>> {
        self.publish_with_memory_loader(surface, None).await
    }

    pub(crate) async fn publish_with_memory_loader(
        self,
        surface: crate::tools::capability::turn_state::TurnSurfaceKind,
        loader: Option<Arc<dyn crate::agent::memory_loader::MemoryLoader>>,
    ) -> Result<Arc<super::turn_decision::TurnDecisionService>> {
        self.publish_scoped(surface, loader, None).await
    }

    /// Publish with the surface of one targeted agent applied on top of the
    /// host surface policy. `None` is the ordinary host publication.
    pub(crate) async fn publish_scoped(
        self,
        surface: crate::tools::capability::turn_state::TurnSurfaceKind,
        loader: Option<Arc<dyn crate::agent::memory_loader::MemoryLoader>>,
        targeted: Option<TargetedSurface>,
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
                // Discover configured servers before pinning the catalog. The
                // turn engine refreshes selected tools before exposing their
                // schemas; deferring initial discovery until then would add
                // aliases to the executor after this publication was sealed.
                // Discovery only initializes servers and lists tools; it never
                // invokes a remote tool.
                mcp.refresh_catalog_config();
                tools::Tool::refresh(mcp.as_ref()).await?;
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
        let mut surface_policy = surface_policy(surface, &config.tool_tiering);
        if let Some(targeted) = targeted {
            surface_policy.allowed_tools = targeted.allowed_tools;
            surface_policy.excluded_names.extend(targeted.excluded_names);
        }
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
        let events: Arc<dyn crate::observability::causal_event::CausalEventSink> = Arc::new(
            crate::observability::causal_event::ObserverCausalSink::new(Arc::clone(&observer)),
        );
        let provider_catalog = crate::providers::selection::catalog_for(&config);
        let service = build_publication(
            surface,
            PublicationInputs {
                generation: Arc::clone(&config_generation),
                registry: Arc::clone(registry),
                execution,
                catalog_sources: sources,
                surface_policy,
                provider_catalog: &provider_catalog,
                transports: &crate::decision_engine::prediction::compatible_http::HttpDecisionTransports,
                events,
                memory: Arc::clone(memory),
                security: Arc::clone(&security),
            },
        )
        .await?;
        let service = service.with_loop_runtime(PublicationLoopRuntime {
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
        super::ingress_recovery::service_ready(&service).await?;
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
    pub(crate) runtime: PublicationLoopRuntime,
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
        let events = Arc::new(crate::observability::causal_event::ObserverCausalSink::new(Arc::clone(
            &self.runtime.observer,
        )));
        let service = ordinary_process::ordinary_process_publication(
            Arc::clone(&self.authority),
            PublicationInputs {
                generation: Arc::clone(&generation),
                registry: self.registry,
                execution,
                catalog_sources: crate::tools::capability::catalog::CatalogSources {
                    config_generation_id: generation.id.0.to_string(),
                    ..Default::default()
                },
                surface_policy: self.surface_policy,
                provider_catalog: &self.authority.snapshots().provider_catalog,
                transports: &crate::decision_engine::prediction::compatible_http::HttpDecisionTransports,
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
    pub(crate) runtime: PublicationLoopRuntime,
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
        let events = Arc::new(crate::observability::causal_event::ObserverCausalSink::new(Arc::clone(
            &self.runtime.observer,
        )));
        let service = operator_process::operator_process_publication(
            Arc::clone(&self.authority),
            PublicationInputs {
                generation: Arc::clone(&generation),
                registry: self.registry,
                execution,
                catalog_sources: crate::tools::capability::catalog::CatalogSources {
                    config_generation_id: generation.id.0.to_string(),
                    ..Default::default()
                },
                surface_policy: self.surface_policy,
                provider_catalog: &self.authority.snapshots().provider_catalog,
                transports: &crate::decision_engine::prediction::compatible_http::HttpDecisionTransports,
                events,
                memory,
                security: self.security,
            },
            self.runtime,
        )
        .await?;
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
        .publish(crate::tools::capability::turn_state::TurnSurfaceKind::AgentLoop)
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
                .publish(crate::tools::capability::turn_state::TurnSurfaceKind::AgentLoop)
                .await;
            assert!(matches!(result, Err(error) if error.to_string().contains("registry MCP instance")));
        }
    }
}
