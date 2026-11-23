//! Fresh child continuation authority from a verified, adopted mailbox input.
mod approvals;
mod driver;
mod publication;
pub(crate) use approvals::no_surface;
pub(crate) use driver::watch;
#[derive(Debug, thiserror::Error)]
#[error("child_session_busy")]
pub(crate) struct ChildBusy;
use crate::memory::child_sessions::ChildSessionRecord;
use crate::messaging::runtime::{MessageInput, MessagingRuntime};
use crate::runtime::turn_source::TurnSourceProof;
use anyhow::{Context, Result, ensure};
use std::sync::Arc;

pub(crate) struct MailboxChildSource {
    source: crate::runtime::operator_spawn::ChildLaunchSource,
    args: serde_json::Value,
}
/// Only this module can construct the sealed mailbox source proof.
pub(crate) struct MailboxChildProof {
    session_key: String,
    id: uuid::Uuid,
    record: ChildSessionRecord,
    event: String,
    input: String,
    generation: Arc<crate::config::ConfigGeneration>,
    binding: crate::agent::entry::AgentModelBinding,
    memory: crate::memory::principal::MemoryWriteContext,
    principal: crate::memory::MemoryPrincipal,
}
impl MailboxChildSource {
    pub(crate) async fn admit(
        parent: &Arc<crate::runtime::turn_publication::TurnRuntimeService>,
        record: ChildSessionRecord,
        input: MessageInput,
        live: Option<crate::agent::live_frames::LiveTurnRef>,
    ) -> Result<Self> {
        let runtime = MessagingRuntime::for_config(&parent.generation.effective)?;
        input.verify(&runtime)?;
        let mailbox_scope = runtime.resolve_scope(&input.message.to)?;
        let memory = Arc::clone(&parent.memory);
        let stored = memory
            .child_session_store()
            .context("child storage unavailable")?
            .sessions(&record.workspace, &record.owner)?
            .into_iter()
            .find(|candidate| candidate.session_key == record.session_key)
            .context("child_session_unknown")?;
        ensure!(
            stored.parent_session == record.parent_session && stored.agent == record.agent,
            "child receiver changed"
        );
        let record = stored;
        ensure!(
            input.auto_start()
                && input.message.to.owner == record.owner
                && input.message.to.session == record.session_key
                && input.message.to.agent == record.agent,
            "mailbox receiver differs from persisted child"
        );
        let pinned = &parent.generation.effective;
        let mut current = if pinned.config_path.exists() {
            let directory = pinned
                .config_path
                .parent()
                .and_then(std::path::Path::to_str)
                .context("receiver config directory unavailable")?;
            crate::config::Config::load_existing_read_only_with_config_dir(Some(directory)).await?
        } else {
            pinned.as_ref().clone()
        };
        current.workspace_dir = pinned.workspace_dir.clone();
        current.config_path = pinned.config_path.clone();
        let config = &current;
        let root_agent = runtime
            .store
            .logical_agent(&record.workspace, &record.owner, &record.parent_session)?
            .unwrap_or_else(|| "main".to_owned());
        let root_level = if root_agent == "main" {
            config.autonomy.level
        } else {
            let root = config
                .agents
                .get(&root_agent)
                .filter(|agent| agent.enabled)
                .context("root agent unavailable")?;
            crate::security::agent_level::agent_level(root.autonomy, config.autonomy.level)
        };
        let now = crate::security::agent_level::named_child_level(config, Some(&record.agent), root_level)
            .unwrap_or(root_level);
        let level = record.level.min(now);
        let publication = publication::fresh(parent, level, &current)?;
        let catalog = &publication.provider_catalog;
        let named = config.agents.get(&record.agent).filter(|agent| {
            crate::providers::ProviderId::of(&agent.provider) == crate::providers::ProviderId::of(&record.provider)
        });
        let credential = named.map_or_else(
            || crate::providers::selection::credential_for(config, &record.provider),
            |agent| {
                agent.credential_ref.as_ref().map_or_else(
                    || {
                        if agent.api_key.as_ref().is_some_and(|key| !key.trim().is_empty()) {
                            crate::providers::credentials::CredentialRef::LegacyInline(
                                crate::providers::credentials::LegacySlot::Agent(record.agent.clone()),
                            )
                        } else {
                            crate::providers::selection::credential_for(config, &record.provider)
                        }
                    },
                    |id| crate::providers::credentials::CredentialRef::Profile { id: id.clone() },
                )
            },
        );
        let resolved = crate::providers::selection::check_with(
            config,
            catalog,
            &record.provider,
            &record.model,
            credential,
            &crate::providers::catalog::Requirement::chat(),
            crate::providers::catalog::UnknownPolicy::Invocation,
        )?;
        let route = crate::providers::selection::route_target(catalog, &record.provider, &record.model)?.cloned();
        let provider = Arc::from(crate::providers::create_routed_provider_for(
            &resolved,
            catalog,
            config,
            &crate::providers::provider_runtime_options_from_config(config),
        )?);
        let event_id = input
            .receipt
            .adopted_message_event_id
            .context("mailbox adoption missing")?
            .to_string();
        let event = memory
            .find_owned_message_event(&record.workspace, &record.owner, &event_id)
            .await?
            .context("mailbox adoption was erased")?;
        ensure!(
            event.content == input.text && event.role == "user" && event.source.as_str() == "prx_mailbox",
            "adopted source differs"
        );
        let scope = crate::memory::principal::MemoryWriteContext {
            workspace_id: Some(record.workspace.clone()),
            owner_id: Some(record.owner.clone()),
            session_key: Some(record.session_key.clone()),
            channel: Some("mailbox".into()),
            chat_type: Some("private".into()),
            chat_id: Some(record.parent_session.clone()),
            sender_id: Some("session".into()),
            raw_sender: Some("session".into()),
            require_owner_match: true,
            agent_partition: Some(runtime.store.partition(&mailbox_scope)?),
        };
        let principal = crate::memory::MemoryPrincipal {
            workspace_id: record.workspace.clone(),
            owner_id: Some(record.owner.clone()),
            session_key: Some(record.session_key.clone()),
            agent_id: Some(record.agent.clone()),
            persona_id: None,
            channel: scope.channel.clone(),
            sender: scope.raw_sender.clone(),
            require_owner_match: true,
            legacy_session_key: None,
            agent_partition: scope.agent_partition.clone(),
        };
        let proof = MailboxChildProof {
            session_key: record.parent_session.clone(),
            id: input.message.id,
            record: record.clone(),
            event: event_id,
            input: input.text.clone(),
            generation: Arc::clone(&publication.generation),
            binding: crate::agent::entry::AgentModelBinding::from_resolved(&resolved, route.clone()),
            memory: scope,
            principal,
        };
        let source = crate::runtime::operator_spawn::ChildLaunchSource::from_mailbox(
            &proof,
            publication,
            provider,
            resolved,
            route,
            record.clone(),
            input.clone(),
            live,
        )
        .await?;
        let mut args = serde_json::json!({"action":"continue", "session_key":record.session_key,
            "task":input.text,"provider":record.provider,"model":record.model,"mode":record.mode});
        if record.agent != "main" {
            args.as_object_mut()
                .context("continuation arguments must be an object")?
                .insert("agent".into(), record.agent.into());
        }
        Ok(Self { source, args })
    }
    pub(crate) async fn continue_turn(self, tools: &[Box<dyn crate::tools::Tool>]) -> Result<String> {
        let tool = tools
            .iter()
            .find(|tool| tool.supports_name("sessions_spawn"))
            .context("child execution unavailable")?;
        let result = self
            .source
            .scope(tool.execute_named("sessions_spawn", self.args))
            .await?;
        ensure!(
            result.success,
            "{}",
            result.error.as_deref().unwrap_or("mailbox child continuation refused")
        );
        Ok(result.output)
    }
}
impl TurnSourceProof for MailboxChildProof {
    fn cost_root_agent(&self) -> Option<crate::cost::types::RootAgent> {
        Some(crate::cost::types::RootAgent {
            owner_id: self.record.owner.clone(),
            name: self.record.agent.clone(),
        })
    }
    fn mailbox_child_source_id(&self) -> Option<uuid::Uuid> {
        Some(self.id)
    }
    fn owner_isolated(&self) -> bool {
        true
    }
    fn source_memory_principal(&self) -> crate::memory::MemoryPrincipal {
        self.principal.clone()
    }
    fn workspace_id(&self) -> &str {
        &self.record.workspace
    }
    fn owner_id(&self) -> &str {
        &self.record.owner
    }
    fn session_key(&self) -> &str {
        &self.session_key
    }
    // Lineage identifies the durable root; authority is newly issued from id.
    fn run_id(&self) -> &str {
        &self.record.parent_run
    }
    fn routing_input(&self) -> &str {
        &self.input
    }
    fn provider_name(&self) -> &str {
        &self.record.provider
    }
    fn model_name(&self) -> &str {
        &self.record.model
    }
    fn config_source_revision(&self) -> &str {
        &self.generation.source_revision.fingerprint_sha256
    }
    fn source_event_id(&self) -> Option<&str> {
        Some(&self.event)
    }
    fn memory_scope(&self) -> &crate::memory::principal::MemoryWriteContext {
        &self.memory
    }
    fn model_binding(&self) -> &crate::agent::entry::AgentModelBinding {
        &self.binding
    }
    fn config_generation_id(&self) -> u64 {
        self.generation.id.0
    }
    fn surface(&self) -> crate::tools::capability::turn_state::TurnSurfaceKind {
        crate::tools::capability::turn_state::TurnSurfaceKind::Server
    }
    fn policy_channel(&self) -> &str {
        "mailbox"
    }
    fn policy_sender(&self) -> &str {
        "session"
    }
    fn policy_chat_type(&self) -> &str {
        "private"
    }
}
