//! Source ownership for the public embedded Agent API, separate from CLI authority.
use crate::config::ConfigGeneration;
use crate::memory::{Memory, MemoryFabric, MemoryVisibility, MessageEventSource};
use crate::runtime::envelope::RuntimeEnvelope;
use crate::runtime::turn_source::TurnSourceProof;
use anyhow::Context;
use std::sync::Arc;

pub(super) struct EmbeddedProviderInput {
    pub(super) provider_name: String,
    pub(super) model_name: String,
    pub(super) provider: Arc<dyn crate::providers::Provider>,
}
struct EmbeddedProviderBinding {
    choice: super::entry::AgentModelChoice,
    provider: Arc<dyn crate::providers::Provider>,
}
#[derive(Clone)]
pub(crate) struct SelectedEmbeddedModel {
    choice: super::entry::AgentModelChoice,
    provider: Arc<dyn crate::providers::Provider>,
}

impl SelectedEmbeddedModel {
    #[cfg(feature = "llm-router")]
    pub(crate) fn provider(&self) -> Arc<dyn crate::providers::Provider> {
        Arc::clone(&self.provider)
    }

    #[cfg(feature = "llm-router")]
    pub(crate) const fn choice(&self) -> &super::entry::AgentModelChoice {
        &self.choice
    }
}

pub(super) struct EmbeddedAgentBinding {
    generation: Arc<ConfigGeneration>,
    provider: Arc<dyn crate::providers::Provider>,
    choice: super::entry::AgentModelChoice,
    requested_model: String,
    route_providers: Vec<EmbeddedProviderBinding>,
    memory: Arc<dyn Memory>,
    session: String,
}

impl EmbeddedAgentBinding {
    #[cfg(test)]
    pub(super) fn capture(
        generation: Arc<ConfigGeneration>,
        provider: Arc<dyn crate::providers::Provider>,
        selection: (&str, &str),
        memory: Arc<dyn Memory>,
    ) -> anyhow::Result<Arc<Self>> {
        Self::capture_with_providers(generation, provider, selection, memory, Vec::new())
    }

    pub(super) fn capture_with_providers(
        generation: Arc<ConfigGeneration>,
        provider: Arc<dyn crate::providers::Provider>,
        selection: (&str, &str),
        memory: Arc<dyn Memory>,
        inputs: Vec<EmbeddedProviderInput>,
    ) -> anyhow::Result<Arc<Self>> {
        let selected =
            super::entry::resolve_agent_model_choice(&generation.effective, Some(selection.0), Some(selection.1))?;
        let (provider_name, model_name) = selected.resolved_selection();
        anyhow::ensure!(
            provider_name == selection.0,
            "embedded Agent initial route differs from the injected provider label"
        );
        anyhow::ensure!(
            selected.factory_credential_source_matches(&generation.effective),
            "embedded Agent provider credential origin differs from its model selection"
        );
        let choice = match super::entry::resolve_agent_model_choice(
            &generation.effective,
            Some(provider_name),
            Some(model_name),
        ) {
            Ok(concrete)
                if selected.same_provider_connection(&concrete)
                    && concrete.factory_credential_source_matches(&generation.effective) =>
            {
                concrete
            }
            // A valid initial route may own credentials that the root slot
            // does not. Retain that proof and use its resolved wire model.
            _ => selected,
        };
        let mut route_providers: Vec<EmbeddedProviderBinding> = Vec::with_capacity(inputs.len());
        for input in inputs {
            let route_choice = super::entry::resolve_agent_model_choice(
                &generation.effective,
                Some(&input.provider_name),
                Some(&input.model_name),
            )?;
            anyhow::ensure!(
                route_choice.resolved_selection().0 == input.provider_name
                    && route_choice.factory_credential_source_matches(&generation.effective),
                "embedded Agent route provider differs from its declared connection"
            );
            anyhow::ensure!(
                !route_choice.same_provider_connection(&choice)
                    && !route_providers
                        .iter()
                        .any(|bound| bound.choice.same_provider_connection(&route_choice)),
                "embedded Agent route provider connection is already bound"
            );
            route_providers.push(EmbeddedProviderBinding {
                choice: route_choice,
                provider: input.provider,
            });
        }
        Ok(Arc::new(Self {
            generation,
            provider,
            choice,
            requested_model: selection.1.to_owned(),
            route_providers,
            memory,
            session: format!("agent_api:{}", uuid::Uuid::now_v7()),
        }))
    }

    pub(super) fn verify(
        &self,
        generation: &Arc<ConfigGeneration>,
        provider: &Arc<dyn crate::providers::Provider>,
        model: &str,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            Arc::ptr_eq(generation, &self.generation)
                && Arc::ptr_eq(provider, &self.provider)
                && model == self.requested_model,
            "embedded Agent configuration, provider instance or model changed"
        );
        Ok(())
    }

    fn select_model(&self, input: &str) -> anyhow::Result<Option<SelectedEmbeddedModel>> {
        let config = &self.generation.effective;
        let task_target = if config.task_routing.rules.is_empty()
            && config.task_routing.default_intent == crate::config::TaskRoutingIntentConfig::Simple
        {
            None
        } else {
            let task = super::classifier::classify_intent(&config.task_routing, input);
            // A delegated model hint selects the child, never this parent
            // connection. Execution still requires a gated Delegate context.
            if task.intent == super::classifier::TaskIntent::Delegate {
                return Ok(None);
            }
            task.model_hint
        };
        let target = task_target.or_else(|| super::classifier::classify(&config.query_classification, input));
        let Some(target) = target else {
            return Ok(None);
        };
        let target = target.trim();
        anyhow::ensure!(!target.is_empty(), "embedded Agent selected model is empty");
        let hint = config.model_routes.iter().any(|route| route.hint == target);
        let target = if hint {
            format!("hint:{target}")
        } else {
            target.to_owned()
        };
        let selected =
            super::entry::resolve_agent_model_choice(config, Some(self.choice.provider_name()), Some(&target))?;
        self.bind_selected_model(selected).map(Some)
    }

    fn bind_selected_model(&self, selected: super::entry::AgentModelChoice) -> anyhow::Result<SelectedEmbeddedModel> {
        let config = &self.generation.effective;
        anyhow::ensure!(
            selected.factory_credential_source_matches(config),
            "embedded Agent selected credential source differs from the provider factory"
        );
        let bound_provider = if selected.same_provider_connection(&self.choice) {
            Arc::clone(&self.provider)
        } else {
            let bound = self.route_providers.iter().find(|bound| selected.same_provider_connection(&bound.choice))
                .context("embedded Agent selected route requires a different provider connection; register its actual instance")?;
            Arc::clone(&bound.provider)
        };
        // Preserve a route-specific credential proof when canonicalizing the
        // spelling would change its credential slot. Execution uses the
        // resolved destination accepted by validate_agent_execution_model.
        let (provider, model) = selected.resolved_selection();
        let choice = match super::entry::resolve_agent_model_choice(config, Some(provider), Some(model)) {
            Ok(concrete)
                if concrete.same_provider_connection(&selected)
                    && concrete.factory_credential_source_matches(config) =>
            {
                concrete
            }
            // The original route was already resolved and connection-matched.
            // Do not require its credential slot to exist at the root too.
            _ => selected,
        };
        Ok(SelectedEmbeddedModel {
            choice,
            provider: bound_provider,
        })
    }

    #[cfg(test)]
    pub(super) async fn record(self: &Arc<Self>, input: &str) -> anyhow::Result<EmbeddedAgentSource> {
        self.record_input(
            input,
            &uuid::Uuid::now_v7().to_string(),
            false,
            self.select_model(input)?,
            false,
            None,
        )
        .await
    }

    pub(super) async fn record_guarded(
        self: &Arc<Self>,
        input: &str,
        owner: &Arc<crate::runtime::turn_cleanup::CleanupTracker>,
        cancellation: &tokio_util::sync::CancellationToken,
        defer_model_selection: bool,
    ) -> anyhow::Result<(Arc<EmbeddedAgentSource>, EmbeddedAdmissionGuard)> {
        // Register before spawning or polling a backend write. The Agent's
        // CleanupState cannot admit its next turn while this ticket is live.
        let selected_choice = if defer_model_selection {
            None
        } else {
            self.select_model(input)?
        };
        let ticket = owner.register(cancellation.clone())?;
        let fabric = MemoryFabric::new(
            Arc::clone(&self.memory),
            self.generation.effective.workspace_dir.to_string_lossy(),
        )
        .with_event_recording(self.generation.effective.memory.event_recording_config())
        .with_ingress_recovery(&self.generation.effective, &ticket)
        .await?;
        let binding = Arc::clone(self);
        let input = input.to_owned();
        let run = uuid::Uuid::now_v7().to_string();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(15),
                binding.record_input(
                    &input,
                    &run,
                    false,
                    selected_choice.clone(),
                    defer_model_selection,
                    Some(&fabric),
                ),
            )
            .await
            .map_err(|_| anyhow::anyhow!("embedded input persistence timed out"))
            .and_then(std::convert::identity);
            let result = match result {
                Ok(source) => {
                    let source = Arc::new(source);
                    let guard = EmbeddedAdmissionGuard {
                        source: Some(Arc::clone(&source)),
                        ticket: Some(ticket),
                        pending: None,
                    };
                    Ok((source, guard))
                }
                Err(_error) if fabric.ingress_already_accepted() => {
                    ticket.finish(true);
                    Err(crate::runtime::ingress_recovery::AlreadyAccepted.into())
                }
                Err(error) => {
                    // Lookup only; never repeat the append or invent a source
                    // proof when the original operation did not return.
                    let recovery = tokio::time::timeout(
                        std::time::Duration::from_secs(15),
                        binding.record_input(
                            &input,
                            &run,
                            true,
                            selected_choice,
                            defer_model_selection,
                            Some(&fabric),
                        ),
                    )
                    .await;
                    if let Ok(Ok(source)) = recovery {
                        let mut guard = EmbeddedAdmissionGuard {
                            source: Some(Arc::new(source)),
                            ticket: Some(ticket),
                            pending: None,
                        };
                        let _ = tokio::time::timeout(
                            std::time::Duration::from_secs(15),
                            guard.finish_failure(sender.is_closed()),
                        )
                        .await;
                    } else {
                        ticket.finish(false);
                    }
                    Err(error)
                }
            };
            let _ = sender.send(result);
        });
        receiver.await.context("embedded input owner ended without a receipt")?
    }

    async fn record_input(
        self: &Arc<Self>,
        input: &str,
        run: &str,
        recover: bool,
        selected_choice: Option<SelectedEmbeddedModel>,
        router_learning: bool,
        fabric: Option<&MemoryFabric>,
    ) -> anyhow::Result<EmbeddedAgentSource> {
        #[cfg(not(feature = "llm-router"))]
        let _ = router_learning;
        let mut envelope = RuntimeEnvelope::agent(self.generation.effective.workspace_dir.to_string_lossy(), run)
            .with_channel("agent_api")
            .with_sender("embedded-caller")
            .with_recipient(self.session.clone())
            .with_config_generation(&self.generation);
        envelope.session_key.clone_from(&self.session);
        let fabric = fabric.cloned().unwrap_or_else(|| {
            MemoryFabric::new(Arc::clone(&self.memory), envelope.workspace_id.clone())
                .with_event_recording(self.generation.effective.memory.event_recording_config())
        });
        let input_key = format!("agent_api:{run}:input");
        let event = if recover {
            self.memory
                .find_owned_message_event_by_idempotency_key(
                    &envelope.workspace_id,
                    &envelope.resolved_owner_id(),
                    &input_key,
                )
                .await?
                .context("embedded input commit is unconfirmed")?
        } else {
            fabric
                .record_inbound_user_message(envelope.message_scope(), input, Some(input_key), None)
                .await?
        };
        #[cfg(feature = "llm-router")]
        let mut issued_input = None;
        if event.id > 0 {
            anyhow::ensure!(
                event.workspace_id == envelope.workspace_id
                    && event.owner_id.as_deref() == Some(envelope.resolved_owner_id().as_str())
                    && event.source == MessageEventSource::Agent
                    && event.channel.as_deref() == Some("agent_api")
                    && event.sender.as_deref() == Some("embedded-caller")
                    && event.recipient == envelope.recipient
                    && event.session_key.as_deref() == Some(self.session.as_str())
                    && event.run_id.as_deref() == Some(run)
                    && event.parent_session_key.is_none()
                    && event.parent_run_id.is_none()
                    && event.agent_id.is_none()
                    && event.persona_id.is_none()
                    && event.attempt_id.is_none()
                    && event.lease_epoch.is_none()
                    && event.subject.is_none()
                    && event.causation_event_id.is_none()
                    && event.role == "user"
                    && event.event_type == "message.created"
                    && event.config_generation_id == Some(self.generation.id.0)
                    && event.config_source_revision.as_deref()
                        == Some(self.generation.source_revision.fingerprint_sha256.as_str())
                    && event.visibility == MemoryVisibility::Session
                    && event.content == input
                    && event.idempotency_key.as_deref() == Some(format!("agent_api:{run}:input").as_str())
                    && event.goal_id == envelope.message_scope().goal_id
                    && event.correlation_id == envelope.message_scope().correlation_id
                    && !event.event_id.is_empty(),
                "embedded Agent input receipt differs from its source"
            );
            let stored = self
                .memory
                .find_owned_message_event(&envelope.workspace_id, &envelope.resolved_owner_id(), &event.event_id)
                .await?
                .context("embedded Agent input is not durable")?;
            anyhow::ensure!(
                serde_json::to_value(&stored)? == serde_json::to_value(&event)?,
                "embedded Agent input readback differs from its receipt"
            );
            // Both normal append and recovery lookup converge here. A stored
            // wrapper is verified again after exact readback, before issuing a witness.
            fabric.verify_ingress_input(&event)?;
            #[cfg(feature = "llm-router")]
            if router_learning {
                issued_input = Some(Arc::new(VerifiedEmbeddedInputMetadata::from_verified(&event)?));
            }
            envelope = envelope.with_source_message_event_id(event.event_id);
        } else {
            anyhow::ensure!(
                !self.generation.effective.causal_tree.enabled,
                "enabled embedded Agent requires durable original input"
            );
        }
        Ok(EmbeddedAgentSource {
            owner: envelope.resolved_owner_id(),
            scope: envelope.memory_write_context("private"),
            envelope,
            input: input.to_owned(),
            binding: Arc::clone(self),
            selected_choice,
            fabric,
            #[cfg(feature = "llm-router")]
            issued_input,
        })
    }
}

/// Own pre-decision cleanup until a real terminal guard accepts the ticket.
/// This guard grants no execution authority and never publishes a decision.
pub(super) struct EmbeddedAdmissionGuard {
    source: Option<Arc<EmbeddedAgentSource>>,
    ticket: Option<crate::runtime::turn_cleanup::CleanupTicket>,
    pending: Option<bool>,
}
impl EmbeddedAdmissionGuard {
    #[cfg(feature = "llm-router")]
    pub(super) fn rebind_model(&mut self, source: Arc<EmbeddedAgentSource>) -> anyhow::Result<()> {
        let original = self.source.as_ref().context("embedded admission source missing")?;
        anyhow::ensure!(
            self.pending.is_none()
                && Arc::ptr_eq(&original.binding, &source.binding)
                && original.input == source.input
                && original.envelope == source.envelope,
            "embedded Router attempted to change its recorded source"
        );
        self.source = Some(source);
        Ok(())
    }

    pub(super) fn into_terminal_ticket(mut self) -> anyhow::Result<crate::runtime::turn_cleanup::CleanupTicket> {
        anyhow::ensure!(self.pending.is_none(), "embedded admission failure is already pending");
        let ticket = self
            .ticket
            .take()
            .context("embedded admission cleanup ticket missing")?;
        self.source = None;
        Ok(ticket)
    }

    pub(super) async fn finish_failure(&mut self, cancelled: bool) -> anyhow::Result<()> {
        anyhow::ensure!(self.pending.is_none(), "embedded admission failure is already pending");
        self.pending = Some(cancelled);
        let result = self
            .source
            .as_ref()
            .context("embedded admission source missing")?
            .record_admission_failure(cancelled)
            .await;
        self.source = None;
        if let Some(ticket) = self.ticket.take() {
            ticket.finish(result.is_ok());
        }
        result
    }
}
impl Drop for EmbeddedAdmissionGuard {
    fn drop(&mut self) {
        let Some(source) = self.source.take() else {
            return;
        };
        let Some(ticket) = self.ticket.take() else {
            return;
        };
        let cancelled = self.pending.unwrap_or(true);
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            ticket.finish(false);
            return;
        };
        runtime.spawn(async move {
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(15),
                source.record_admission_failure(cancelled),
            )
            .await;
            ticket.finish(matches!(result, Ok(Ok(()))));
        });
    }
}

/// Produced only after the native input's capsule verification and exact owned
/// readback. No public constructor or caller-supplied raw metadata entrance exists.
#[cfg(feature = "llm-router")]
pub(crate) struct VerifiedEmbeddedInputMetadata {
    sequence: i64,
    event_id: String,
    workspace: String,
    owner: String,
    session: String,
    run: String,
    generation: u64,
    revision: String,
    raw: Option<Arc<str>>,
}
#[cfg(feature = "llm-router")]
impl VerifiedEmbeddedInputMetadata {
    fn from_verified(event: &crate::memory::MessageEvent) -> anyhow::Result<Self> {
        anyhow::ensure!(
            event.raw_payload_json.as_ref().is_none_or(|raw| raw.len() <= 32 * 1024),
            "embedded source metadata exceeds Router bound"
        );
        if let Some(raw) = &event.raw_payload_json {
            let wrapper: serde_json::Value = serde_json::from_str(raw)?;
            anyhow::ensure!(
                wrapper.as_object().is_some_and(|fields| fields.len() == 2)
                    && wrapper
                        .get("prx_ingress_capsule")
                        .is_some_and(serde_json::Value::is_object)
                    && wrapper
                        .get("original_payload_json")
                        .is_some_and(serde_json::Value::is_null),
                "embedded source metadata is not the verified native capsule wrapper"
            );
        }
        for value in [
            Some(event.event_id.as_str()),
            Some(event.workspace_id.as_str()),
            event.owner_id.as_deref(),
            event.session_key.as_deref(),
            event.run_id.as_deref(),
            event.config_source_revision.as_deref(),
        ] {
            anyhow::ensure!(
                value.is_some_and(|value| !value.is_empty() && value.len() <= 512),
                "embedded input metadata identity exceeds Router bound"
            );
        }
        anyhow::ensure!(event.id > 0, "embedded input sequence is not durable");
        Ok(Self {
            sequence: event.id,
            event_id: event.event_id.clone(),
            workspace: event.workspace_id.clone(),
            owner: event.owner_id.clone().context("embedded input owner missing")?,
            session: event.session_key.clone().context("embedded input session missing")?,
            run: event.run_id.clone().context("embedded input run missing")?,
            generation: event
                .config_generation_id
                .context("embedded input generation missing")?,
            revision: event
                .config_source_revision
                .clone()
                .context("embedded input revision missing")?,
            raw: event.raw_payload_json.as_deref().map(Arc::from),
        })
    }

    pub(crate) const fn sequence(&self) -> i64 {
        self.sequence
    }

    pub(crate) fn verified_raw_for<'a>(&'a self, source: &dyn TurnSourceProof) -> anyhow::Result<Option<&'a str>> {
        anyhow::ensure!(
            source.source_event_id() == Some(self.event_id.as_str())
                && source.workspace_id() == self.workspace
                && source.owner_id() == self.owner
                && source.session_key() == self.session
                && source.run_id() == self.run
                && source.config_generation_id() == self.generation
                && source.config_source_revision() == self.revision,
            "Router source metadata witness differs from its source"
        );
        Ok(self.raw.as_deref())
    }
}

pub(crate) struct EmbeddedAgentSource {
    binding: Arc<EmbeddedAgentBinding>,
    selected_choice: Option<SelectedEmbeddedModel>,
    envelope: RuntimeEnvelope,
    owner: String,
    input: String,
    scope: crate::memory::principal::MemoryWriteContext,
    fabric: MemoryFabric,
    #[cfg(feature = "llm-router")]
    issued_input: Option<Arc<VerifiedEmbeddedInputMetadata>>,
}
impl EmbeddedAgentSource {
    /// Resolve only the requested spelling returned by the source-bound PMM
    /// auxiliary scope; the accounting service rechecks the selected binding.
    #[cfg(feature = "llm-router")]
    pub(crate) fn select_auxiliary_model(&self, provider: &str, model: &str) -> anyhow::Result<SelectedEmbeddedModel> {
        let selected =
            super::entry::resolve_agent_model_choice(&self.binding.generation.effective, Some(provider), Some(model))?;
        self.binding.bind_selected_model(selected)
    }

    #[cfg(feature = "llm-router")]
    pub(crate) fn input_metadata(&self) -> anyhow::Result<&VerifiedEmbeddedInputMetadata> {
        self.issued_input
            .as_deref()
            .context("embedded input metadata witness missing")
    }
    #[cfg(feature = "llm-router")]
    pub(crate) fn needs_router_history(&self, router: &crate::router::RouterEngine) -> anyhow::Result<bool> {
        let config = &self.binding.generation.effective;
        router.validate_embedded_binding(config, self.binding.choice.provider_name(), &self.binding.memory)?;
        router.bind_local_source(self)?;
        let task = super::classifier::classify_intent(&config.task_routing, &self.input);
        Ok(router.has_history() && task.intent != super::classifier::TaskIntent::Delegate && task.model_hint.is_none())
    }

    #[cfg(feature = "llm-router")]
    pub(crate) fn prepare_router_selection(
        &self,
        ranked: &crate::router::scorer::RouterResult,
    ) -> anyhow::Result<SelectedEmbeddedModel> {
        let config = &self.binding.generation.effective;
        let choice = if let (Some(provider), Some(model)) =
            (ranked.chosen_provider.as_deref(), ranked.chosen_model.as_deref())
        {
            if let Some(route) = config
                .model_routes
                .iter()
                .find(|route| route.provider == provider && route.model == model)
            {
                super::entry::resolve_agent_model_choice(
                    config,
                    Some(self.binding.choice.provider_name()),
                    Some(&format!("hint:{}", route.hint)),
                )?
            } else {
                let model = if provider == "ollama" && model == "*" {
                    self.binding.choice.resolved_selection().1
                } else {
                    model
                };
                super::entry::resolve_agent_model_choice(config, Some(provider), Some(model))?
            }
        } else {
            self.binding.choice.clone()
        };
        self.binding.bind_selected_model(choice)
    }

    #[cfg(feature = "llm-router")]
    pub(crate) fn with_admitted_router_selection(
        self: &Arc<Self>,
        token: crate::runtime::turn_decision::RouterModelReadmission,
    ) -> anyhow::Result<Arc<Self>> {
        let selected_choice = token.consume_for(self)?;
        Ok(Arc::new(Self {
            binding: Arc::clone(&self.binding),
            selected_choice: Some(selected_choice),
            envelope: self.envelope.clone(),
            owner: self.owner.clone(),
            input: self.input.clone(),
            scope: self.scope.clone(),
            fabric: self.fabric.clone(),
            issued_input: self.issued_input.clone(),
        }))
    }

    #[cfg(feature = "llm-router")]
    pub(super) fn with_local_router(
        self: &Arc<Self>,
        router: &crate::router::RouterEngine,
    ) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(
            self.source_event_id().is_some(),
            "embedded Router requires durable original input"
        );
        let config = &self.binding.generation.effective;
        router.validate_embedded_binding(config, self.binding.choice.provider_name(), &self.binding.memory)?;
        router.bind_local_source(self.as_ref())?;
        let task = super::classifier::classify_intent(&config.task_routing, &self.input);
        let selected_choice = if task.intent == super::classifier::TaskIntent::Delegate || task.model_hint.is_some() {
            // Explicit task routing retains precedence over adaptive ranking.
            self.binding.select_model(&self.input)?
        } else {
            let ranked = router.select_model_local(&self.input, task.intent, self.as_ref())?;
            if let (Some(provider), Some(model)) = (ranked.chosen_provider.as_deref(), ranked.chosen_model.as_deref()) {
                let choice = if let Some(route) = config
                    .model_routes
                    .iter()
                    .find(|route| route.provider == provider && route.model == model)
                {
                    super::entry::resolve_agent_model_choice(
                        config,
                        Some(self.binding.choice.provider_name()),
                        Some(&format!("hint:{}", route.hint)),
                    )?
                } else {
                    let model = if provider == "ollama" && model == "*" {
                        self.binding.choice.resolved_selection().1
                    } else {
                        model
                    };
                    super::entry::resolve_agent_model_choice(config, Some(provider), Some(model))?
                };
                Some(self.binding.bind_selected_model(choice)?)
            } else {
                self.binding.select_model(&self.input)?
            }
        };
        Ok(Arc::new(Self {
            binding: Arc::clone(&self.binding),
            selected_choice,
            envelope: self.envelope.clone(),
            owner: self.owner.clone(),
            input: self.input.clone(),
            scope: self.scope.clone(),
            fabric: self.fabric.clone(),
            issued_input: self.issued_input.clone(),
        }))
    }

    #[cfg(feature = "llm-router")]
    pub(super) async fn record_automix_budget_refusal(&self, premium: &str) -> anyhow::Result<()> {
        let scope = self.envelope.message_scope();
        let summary = "premium attempt not dispatched: local admission not confirmed";
        let payload = serde_json::json!({
            "source_event_id": self.source_event_id(), "run_id": self.run_id(),
            "reason": "admission_not_confirmed", "attempt_dispatched": false,
            "premium_model": crate::providers::redact_endpoint_userinfo(premium),
        });
        let key = format!("turn:{}:automix-budget-refused", self.run_id());
        let event = self
            .fabric
            .record_runtime_event_idempotent(
                scope.clone(),
                "router.automix.refused",
                summary,
                Some(serde_json::to_string(&payload)?),
                key.clone(),
            )
            .await?;
        if event.id <= 0 && !self.binding.generation.effective.causal_tree.enabled {
            return Ok(());
        }
        anyhow::ensure!(
            event.id > 0
                && !event.event_id.is_empty()
                && event.workspace_id == self.workspace_id()
                && event.owner_id == scope.owner_id
                && event.source == scope.source
                && event.channel == scope.channel
                && event.sender == scope.sender
                && event.recipient == scope.recipient
                && event.session_key == scope.session_key
                && event.run_id == scope.run_id
                && event.parent_run_id == scope.parent_run_id
                && event.parent_session_key == scope.parent_session_key
                && event.agent_id == scope.agent_id
                && event.persona_id == scope.persona_id
                && event.attempt_id == scope.attempt_id
                && event.lease_epoch == scope.lease_epoch
                && event.subject == scope.subject
                && event.causation_event_id == scope.causation_event_id
                && event.goal_id == scope.goal_id
                && event.correlation_id == scope.correlation_id
                && event.config_generation_id == scope.config_generation_id
                && event.config_source_revision == scope.config_source_revision
                && event.visibility == scope.visibility
                && event.role == "event"
                && event.event_type == "router.automix.refused"
                && event.content == format!("router.automix.refused {summary}")
                && event.idempotency_key.as_deref() == Some(key.as_str())
                && event
                    .raw_payload_json
                    .as_deref()
                    .map(serde_json::from_str::<serde_json::Value>)
                    .transpose()?
                    .as_ref()
                    == Some(&payload),
            "automix refusal receipt conflicts with its source"
        );
        let stored = self
            .binding
            .memory
            .find_owned_message_event(self.workspace_id(), self.owner_id(), &event.event_id)
            .await?
            .context("automix refusal receipt is not durable")?;
        anyhow::ensure!(
            serde_json::to_value(&stored)? == serde_json::to_value(&event)?,
            "automix refusal readback differs from its receipt"
        );
        Ok(())
    }

    async fn record_admission_failure(&self, cancelled: bool) -> anyhow::Result<()> {
        let recording = self.binding.generation.effective.memory.event_recording_config();
        if !recording.enabled || !recording.record_user_messages {
            return Ok(());
        }
        let event_id = self.source_event_id().context("embedded admission input is missing")?;
        let input = self
            .binding
            .memory
            .find_owned_message_event(self.workspace_id(), self.owner_id(), event_id)
            .await?
            .context("embedded admission input is not durable")?;
        let scope = self.envelope.message_scope();
        anyhow::ensure!(
            input.source == scope.source
                && input.channel == scope.channel
                && input.sender == scope.sender
                && input.visibility == scope.visibility
                && input.goal_id == scope.goal_id
                && input.correlation_id == scope.correlation_id,
            "embedded admission input attribution differs from its source"
        );
        anyhow::ensure!(
            input.id > 0
                && input.event_id == event_id
                && input.workspace_id == self.workspace_id()
                && input.owner_id.as_deref() == Some(self.owner_id())
                && input.session_key.as_deref() == Some(self.session_key())
                && input.run_id.as_deref() == Some(self.run_id())
                && input.config_generation_id == Some(self.config_generation_id())
                && input.config_source_revision.as_deref() == Some(self.config_source_revision())
                && input.role == "user"
                && input.event_type == "message.created"
                && input.recipient == self.envelope.recipient
                && input.parent_run_id.is_none()
                && input.parent_session_key.is_none()
                && input.agent_id.is_none()
                && input.persona_id.is_none()
                && input.subject.is_none()
                && input.attempt_id.is_none()
                && input.lease_epoch.is_none()
                && input.causation_event_id.is_none()
                && crate::runtime::turn_source::recorded_input_matches(self.surface(), &input, &self.input),
            "embedded admission input differs from its source"
        );
        let summary = "embedded input admission did not complete";
        let payload = serde_json::json!({
            "run_id": self.run_id(), "source_event_id": event_id,
            "status": if cancelled { "cancelled" } else { "failed" }, "summary": summary,
        });
        let key = format!("turn:{}:admission-failed", self.run_id());
        let event = self
            .fabric
            .record_runtime_event_idempotent(
                scope.clone(),
                "turn.admission.failed",
                summary,
                Some(serde_json::to_string(&payload)?),
                key.clone(),
            )
            .await?;
        anyhow::ensure!(
            event.id > 0
                && !event.event_id.is_empty()
                && event.workspace_id == self.workspace_id()
                && event.owner_id == scope.owner_id
                && event.source == scope.source
                && event.channel == scope.channel
                && event.sender == scope.sender
                && event.recipient == scope.recipient
                && event.session_key == scope.session_key
                && event.run_id == scope.run_id
                && event.parent_run_id == scope.parent_run_id
                && event.parent_session_key == scope.parent_session_key
                && event.agent_id == scope.agent_id
                && event.persona_id == scope.persona_id
                && event.attempt_id == scope.attempt_id
                && event.lease_epoch == scope.lease_epoch
                && event.subject == scope.subject
                && event.causation_event_id == scope.causation_event_id
                && event.goal_id == scope.goal_id
                && event.correlation_id == scope.correlation_id
                && event.config_generation_id == scope.config_generation_id
                && event.config_source_revision == scope.config_source_revision
                && event.visibility == scope.visibility
                && event.role == "event"
                && event.event_type == "turn.admission.failed"
                && event.content == format!("turn.admission.failed {summary}")
                && event.idempotency_key.as_deref() == Some(key.as_str())
                && event
                    .raw_payload_json
                    .as_deref()
                    .map(serde_json::from_str::<serde_json::Value>)
                    .transpose()?
                    .as_ref()
                    == Some(&payload),
            "embedded admission failure retry conflicts with its source or status"
        );
        let stored = self
            .binding
            .memory
            .find_owned_message_event(self.workspace_id(), self.owner_id(), &event.event_id)
            .await?
            .context("embedded admission failure receipt is not durable")?;
        anyhow::ensure!(
            serde_json::to_value(&stored)? == serde_json::to_value(&event)?,
            "embedded admission failure readback differs from its receipt"
        );
        Ok(())
    }

    pub(super) fn configured_delegation(&self) -> Option<super::classifier::ClassifyResult> {
        let config = &self.binding.generation.effective.task_routing;
        if config.rules.is_empty() && config.default_intent != crate::config::TaskRoutingIntentConfig::Delegate {
            return None;
        }
        let task = super::classifier::classify_intent(config, &self.input);
        (task.intent == super::classifier::TaskIntent::Delegate).then_some(task)
    }

    fn choice(&self) -> &super::entry::AgentModelChoice {
        self.selected_choice
            .as_ref()
            .map_or(&self.binding.choice, |selected| &selected.choice)
    }
    pub(super) fn provider(&self) -> &Arc<dyn crate::providers::Provider> {
        self.selected_choice
            .as_ref()
            .map_or(&self.binding.provider, |selected| &selected.provider)
    }
    pub(crate) fn wire_selection(&self) -> (&str, &str) {
        self.choice().resolved_selection()
    }
    pub(crate) const fn envelope(&self) -> &RuntimeEnvelope {
        &self.envelope
    }
    pub(super) const fn fabric(&self) -> &MemoryFabric {
        &self.fabric
    }

    /// Best-effort promotion is invoked only after decision admission. The
    /// stored content and attribution come exclusively from this source.
    pub(super) async fn autosave_user_input(&self) -> anyhow::Result<()> {
        if !self
            .binding
            .generation
            .effective
            .memory
            .should_auto_promote_user_message(&self.input)
        {
            return Ok(());
        }
        // This is a stricter write requirement, not new caller authority.
        // Backends unable to persist an owner must refuse instead of using
        // the trait's unscoped compatibility fallback.
        let mut context = self.scope.clone();
        context.owner_id = Some(self.owner.clone());
        context.require_owner_match = true;
        self.binding
            .memory
            .store_with_context_and_metadata(
                &format!("user_msg:{}", self.run_id()),
                &self.input,
                crate::memory::MemoryCategory::Conversation,
                Some(self.session_key()),
                Some(&context),
                crate::memory::MemoryStoreMetadata {
                    workspace_id: Some(self.workspace_id().to_owned()),
                    owner_id: Some(self.owner.clone()),
                    source_event_id: self.source_event_id().map(str::to_owned),
                    source: Some("semantic_promotion".to_owned()),
                    channel: Some("agent_api".to_owned()),
                    ..Default::default()
                },
            )
            .await
    }
}
impl TurnSourceProof for EmbeddedAgentSource {
    fn owner_isolated(&self) -> bool {
        self.envelope.owner_isolated
    }
    fn source_memory_principal(&self) -> crate::memory::MemoryPrincipal {
        self.envelope.memory_principal()
    }

    fn workspace_id(&self) -> &str {
        &self.envelope.workspace_id
    }
    fn owner_id(&self) -> &str {
        &self.owner
    }
    fn session_key(&self) -> &str {
        &self.envelope.session_key
    }
    fn run_id(&self) -> &str {
        self.envelope.run_id.as_deref().unwrap_or_default()
    }
    fn routing_input(&self) -> &str {
        &self.input
    }
    fn provider_name(&self) -> &str {
        self.choice().provider_name()
    }
    fn model_name(&self) -> &str {
        self.choice().model_name()
    }
    fn config_source_revision(&self) -> &str {
        &self.binding.generation.source_revision.fingerprint_sha256
    }
    fn source_event_id(&self) -> Option<&str> {
        self.envelope.source_message_event_id.as_deref()
    }
    fn memory_scope(&self) -> &crate::memory::principal::MemoryWriteContext {
        &self.scope
    }
    fn model_binding(&self) -> &super::entry::AgentModelBinding {
        self.choice().binding()
    }
    fn config_generation_id(&self) -> u64 {
        self.binding.generation.id.0
    }
    fn surface(&self) -> crate::causal_tree::state::TurnSurfaceKind {
        crate::causal_tree::state::TurnSurfaceKind::AgentLoop
    }
    fn policy_channel(&self) -> &str {
        "agent_api"
    }
    fn policy_sender(&self) -> &str {
        "embedded-caller"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct LocalProvider;
    #[async_trait::async_trait]
    impl crate::providers::Provider for LocalProvider {
        async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
            anyhow::bail!("source admission must not call the provider")
        }
    }
    fn autosave_fixture(
        workspace: &std::path::Path,
        auto_save: bool,
        promote: bool,
        min_chars: usize,
    ) -> (Arc<EmbeddedAgentBinding>, Arc<dyn Memory>) {
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.to_owned();
        config.config_path = workspace.join("config.toml");
        config.causal_tree.enabled = false;
        config.memory.auto_save = auto_save;
        config.memory.semantic.auto_promote_user_messages = promote;
        config.memory.semantic.min_chars = min_chars;
        let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(workspace).unwrap());
        let binding = EmbeddedAgentBinding::capture(
            crate::config::new_shared(config).pin(),
            Arc::new(LocalProvider),
            ("ollama", "prx-local-model"),
            Arc::clone(&memory),
        )
        .unwrap();
        (binding, memory)
    }

    #[tokio::test]
    async fn embedded_autosave_respects_configuration_and_keeps_original_source() {
        let input = "The project uses a weekly release schedule with a separate review window.";
        for (auto_save, promote, min_chars, expected) in [
            (true, true, 1, true),
            (false, true, 1, false),
            (true, false, 1, false),
            (true, true, 1000, false),
        ] {
            let workspace = tempfile::tempdir().unwrap();
            let (binding, memory) = autosave_fixture(workspace.path(), auto_save, promote, min_chars);
            let source = binding.record(input).await.unwrap();
            source.autosave_user_input().await.unwrap();
            let key = format!("user_msg:{}", source.run_id());
            let entry = memory.get(&key).await.unwrap();
            assert_eq!(entry.is_some(), expected);
            if let Some(entry) = entry {
                assert_eq!(entry.content, input);
                assert_eq!(entry.session_id.as_deref(), Some(source.session_key()));
                let conn = rusqlite::Connection::open(workspace.path().join("memory/brain.db")).unwrap();
                let metadata: (String, String, String, String, String) = conn.query_row(
                    "SELECT workspace_id, owner_id, source_event_id, channel, visibility FROM memories WHERE key = ?1",
                    [&key], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
                ).unwrap();
                assert_eq!(metadata.0, source.workspace_id());
                assert_eq!(metadata.1, source.owner_id());
                assert_eq!(metadata.2, source.source_event_id().unwrap());
                assert_eq!(metadata.3, "agent_api");
                assert!(matches!(metadata.4.as_str(), "private" | "owner"));
            }
            assert!(memory.get("assistant_resp").await.unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn embedded_autosave_refuses_unscoped_backend_fallback() {
        let workspace = tempfile::tempdir().unwrap();
        let (configured, _) = autosave_fixture(workspace.path(), true, true, 1);
        let binding = EmbeddedAgentBinding::capture(
            Arc::clone(&configured.generation),
            Arc::clone(&configured.provider),
            ("ollama", "prx-local-model"),
            Arc::new(crate::memory::NoneMemory::new()),
        )
        .unwrap();
        let source = binding
            .record("The project uses a weekly release schedule.")
            .await
            .unwrap();
        let error = source.autosave_user_input().await.unwrap_err();
        assert!(matches!(
            error.downcast_ref::<crate::memory::owner_predicate::OwnerScopedStoreRefusal>(),
            Some(crate::memory::owner_predicate::OwnerScopedStoreRefusal::UnscopedBackend { .. })
        ));
    }

    #[tokio::test]
    async fn embedded_autosave_refuses_to_overwrite_another_owner() {
        let workspace = tempfile::tempdir().unwrap();
        let (binding, memory) = autosave_fixture(workspace.path(), true, true, 1);
        let source = binding
            .record("The project uses a weekly release schedule.")
            .await
            .unwrap();
        let key = format!("user_msg:{}", source.run_id());
        memory
            .store_with_metadata(
                &key,
                "existing project note",
                crate::memory::MemoryCategory::Conversation,
                None,
                crate::memory::MemoryStoreMetadata {
                    owner_id: Some("prx_other_owner".to_owned()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(source.autosave_user_input().await.is_err());
        assert_eq!(
            memory.get(&key).await.unwrap().unwrap().content,
            "existing project note"
        );
    }

    fn routing_binding(workspace: &std::path::Path, mut config: crate::config::Config) -> Arc<EmbeddedAgentBinding> {
        config.workspace_dir = workspace.to_owned();
        config.config_path = workspace.join("config.toml");
        config.causal_tree.enabled = false;
        EmbeddedAgentBinding::capture(
            crate::config::new_shared(config).pin(),
            Arc::new(LocalProvider),
            ("ollama", "prx-local-model"),
            Arc::new(crate::memory::SqliteMemory::new(workspace).unwrap()),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn embedded_model_selection_resolves_hints_without_sticking_to_previous_turn() {
        let workspace = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.model_routes.push(crate::config::ModelRouteConfig {
            settings: Default::default(),
            hint: "fast".to_owned(),
            provider: "ollama".to_owned(),
            model: "prx-fast-model".to_owned(),
            api_key: None,
            credential_ref: None,
        });
        config
            .query_classification
            .rules
            .push(crate::config::schema::ClassificationRule {
                hint: "fast".to_owned(),
                keywords: vec!["quick".to_owned()],
                ..Default::default()
            });
        let binding = routing_binding(workspace.path(), config);
        let selected = binding.record("quick response").await.unwrap();
        let ordinary = binding.record("ordinary response").await.unwrap();
        assert_eq!(selected.model_name(), "prx-fast-model");
        assert_eq!(ordinary.model_name(), "prx-local-model");
        assert_eq!(selected.provider_name(), "ollama");
        assert_eq!(selected.session_key(), ordinary.session_key());
        assert_ne!(selected.source_event_id(), ordinary.source_event_id());
    }

    #[tokio::test]
    async fn embedded_initial_hint_is_bound_to_a_concrete_model() {
        let workspace = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_owned();
        config.config_path = workspace.path().join("config.toml");
        config.causal_tree.enabled = false;
        config.model_routes.push(crate::config::ModelRouteConfig {
            settings: Default::default(),
            hint: "fast".to_owned(),
            provider: "ollama".to_owned(),
            model: "prx-fast-model".to_owned(),
            api_key: None,
            credential_ref: None,
        });
        let generation = crate::config::new_shared(config).pin();
        let provider: Arc<dyn crate::providers::Provider> = Arc::new(LocalProvider);
        let binding = EmbeddedAgentBinding::capture(
            Arc::clone(&generation),
            Arc::clone(&provider),
            ("ollama", "hint:fast"),
            Arc::new(crate::memory::SqliteMemory::new(workspace.path()).unwrap()),
        )
        .unwrap();
        binding.verify(&generation, &provider, "hint:fast").unwrap();
        assert_eq!(
            binding.record("ordinary input").await.unwrap().model_name(),
            "prx-fast-model"
        );
    }

    #[tokio::test]
    async fn embedded_initial_route_keeps_its_own_credential_slot() {
        use crate::providers::credentials::{CredentialRef, LegacySlot};
        let workspace = tempfile::tempdir().unwrap();
        let endpoint = "custom:http://127.0.0.1:19999/v1";
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_owned();
        config.config_path = workspace.path().join("config.toml");
        config.causal_tree.enabled = false;
        config.model_routes.push(crate::config::ModelRouteConfig {
            settings: Default::default(),
            hint: "private_route".to_owned(),
            provider: endpoint.to_owned(),
            model: "prx-route-model".to_owned(),
            api_key: Some("prx-test-route-key".to_owned()),
            credential_ref: None,
        });
        assert!(config.api_key.is_none());
        let generation = crate::config::new_shared(config).pin();
        let provider: Arc<dyn crate::providers::Provider> = Arc::new(LocalProvider);
        let binding = EmbeddedAgentBinding::capture(
            Arc::clone(&generation),
            Arc::clone(&provider),
            (endpoint, "hint:private_route"),
            Arc::new(crate::memory::SqliteMemory::new(workspace.path()).unwrap()),
        )
        .unwrap();
        binding.verify(&generation, &provider, "hint:private_route").unwrap();
        let source = binding.record("original input").await.unwrap();
        assert_eq!(source.wire_selection(), (endpoint, "prx-route-model"));
        assert_eq!(source.model_name(), "hint:private_route");
        assert_eq!(
            source.model_binding().credential_reference(),
            &CredentialRef::LegacyInline(LegacySlot::ModelRoute("private_route".to_owned()))
        );
        assert!(Arc::ptr_eq(source.provider(), &provider));
    }

    #[tokio::test]
    async fn embedded_route_provider_label_must_match_resolved_destination() {
        let workspace = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_owned();
        config.config_path = workspace.path().join("config.toml");
        config.causal_tree.enabled = false;
        config.model_routes.push(crate::config::ModelRouteConfig {
            settings: Default::default(),
            hint: "other".to_owned(),
            provider: "custom:http://127.0.0.1:19999/v1".to_owned(),
            model: "prx-other-model".to_owned(),
            api_key: None,
            credential_ref: None,
        });
        let result = EmbeddedAgentBinding::capture_with_providers(
            crate::config::new_shared(config).pin(),
            Arc::new(LocalProvider),
            ("ollama", "prx-local-model"),
            Arc::new(crate::memory::SqliteMemory::new(workspace.path()).unwrap()),
            vec![EmbeddedProviderInput {
                provider_name: "ollama".to_owned(),
                model_name: "hint:other".to_owned(),
                provider: Arc::new(LocalProvider),
            }],
        );
        assert!(matches!(result, Err(error) if error.to_string().contains("differs from its declared connection")));
    }

    #[tokio::test]
    async fn embedded_task_model_takes_precedence_over_query_classification() {
        let workspace = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config
            .query_classification
            .rules
            .push(crate::config::schema::ClassificationRule {
                hint: "prx-query-model".to_owned(),
                keywords: vec!["review".to_owned()],
                ..Default::default()
            });
        config.task_routing.rules.push(crate::config::TaskRoutingRule {
            keywords: vec!["review".to_owned()],
            model_hint: Some("prx-task-model".to_owned()),
            ..Default::default()
        });
        let binding = routing_binding(workspace.path(), config);
        assert_eq!(
            binding.record("review project").await.unwrap().model_name(),
            "prx-task-model"
        );
    }

    #[tokio::test]
    async fn embedded_delegation_keeps_parent_model_and_original_source_input() {
        let workspace = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.task_routing.rules.push(crate::config::TaskRoutingRule {
            keywords: vec!["review".to_owned()],
            intent: crate::config::TaskRoutingIntentConfig::Delegate,
            sub_agent_model: Some("prx-child-model".to_owned()),
            ..Default::default()
        });
        config
            .query_classification
            .rules
            .push(crate::config::schema::ClassificationRule {
                hint: "prx-query-model".to_owned(),
                keywords: vec!["review".to_owned()],
                ..Default::default()
            });
        let binding = routing_binding(workspace.path(), config);
        let source = binding.record("review project").await.unwrap();
        assert_eq!(source.model_name(), "prx-local-model");
        assert_eq!(source.wire_selection().1, "prx-local-model");
        assert_eq!(source.routing_input(), "review project");
        assert_eq!(
            source.configured_delegation().unwrap().model_hint.as_deref(),
            Some("prx-child-model")
        );
        let ordinary = binding.record("hello project").await.unwrap();
        assert!(ordinary.configured_delegation().is_none());
        assert_eq!(ordinary.model_name(), "prx-local-model");
    }

    #[tokio::test]
    async fn embedded_cross_provider_selection_is_refused_before_input_recording() {
        let workspace = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.model_routes.push(crate::config::ModelRouteConfig {
            settings: Default::default(),
            hint: "other".to_owned(),
            provider: "custom:http://127.0.0.1:19999/v1".to_owned(),
            model: "prx-other-model".to_owned(),
            api_key: None,
            credential_ref: None,
        });
        config
            .query_classification
            .rules
            .push(crate::config::schema::ClassificationRule {
                hint: "other".to_owned(),
                keywords: vec!["switch".to_owned()],
                ..Default::default()
            });
        let binding = routing_binding(workspace.path(), config);
        assert!(matches!(binding.record("switch model").await,
            Err(error) if error.to_string().contains("different provider connection")));
        let conn = rusqlite::Connection::open(workspace.path().join("memory/brain.db")).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM message_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn embedded_source_binds_real_instance_and_records_fresh_non_cli_runs() {
        let workspace = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_owned();
        config.config_path = workspace.path().join("config.toml");
        config.causal_tree.enabled = false;
        let generation = crate::config::new_shared(config.clone()).pin();
        let provider: Arc<dyn crate::providers::Provider> = Arc::new(LocalProvider);
        let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).unwrap());
        let binding = EmbeddedAgentBinding::capture(
            generation.clone(),
            provider.clone(),
            ("ollama", "prx-local-model"),
            memory,
        )
        .unwrap();
        binding.verify(&generation, &provider, "prx-local-model").unwrap();
        let replacement: Arc<dyn crate::providers::Provider> = Arc::new(LocalProvider);
        assert!(binding.verify(&generation, &replacement, "prx-local-model").is_err());
        assert!(
            binding
                .verify(&crate::config::new_shared(config).pin(), &provider, "prx-local-model")
                .is_err()
        );
        assert!(binding.verify(&generation, &provider, "other-model").is_err());
        let first = binding.record("original input").await.unwrap();
        let second = binding.record("next input").await.unwrap();
        assert_eq!(first.session_key(), second.session_key());
        assert_ne!(first.run_id(), second.run_id());
        assert_ne!(first.source_event_id(), second.source_event_id());
        assert_eq!(first.policy_channel(), "agent_api");
        assert_eq!(first.policy_sender(), "embedded-caller");
        let event = first
            .fabric()
            .memory()
            .find_owned_message_event(first.workspace_id(), first.owner_id(), first.source_event_id().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert!(crate::runtime::turn_source::recorded_input_matches(
            first.surface(),
            &event,
            "original input"
        ));
        assert!(!crate::runtime::turn_source::recorded_input_matches(
            first.surface(),
            &event,
            "substituted input"
        ));
        let mut forged = event;
        forged.sender = Some("local-user".to_owned());
        assert!(!crate::runtime::turn_source::recorded_input_matches(
            first.surface(),
            &forged,
            "original input"
        ));
    }
}
