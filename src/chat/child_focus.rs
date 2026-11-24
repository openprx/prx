//! Child input queues and commands, independent of the main conversation.
use super::binding::{ChatOperatorSpawn, ChatProviderBinding};
use crate::channels::traits::ChannelMessage;
use crate::memory::Memory;
use crate::memory::child_sessions::{ChildSessionRecord, ChildTurn};
use crate::tools::sessions_spawn::{SubAgentRun, SubAgentStatus};
use anyhow::Context;
use std::{collections::VecDeque, sync::Arc};

#[derive(Default)]
pub(super) struct ChildFocus {
    parent: Option<String>,
    message_queue: VecDeque<(String, crate::messaging::runtime::MessageInput)>,
    queue: VecDeque<(String, ChannelMessage)>,
    mailboxes: std::collections::HashMap<String, crate::messaging::runtime::SessionInbox>,
}

pub(super) struct ChildFocusContext<'a> {
    pub memory: &'a Arc<dyn Memory>,
    pub generation: &'a Arc<crate::config::ConfigGeneration>,
    pub parent: &'a str,
    pub security: &'a Arc<crate::security::SecurityPolicy>,
    pub service: &'a Arc<crate::runtime::turn_publication::TurnRuntimeService>,
    pub tools: &'a [Box<dyn crate::tools::Tool>],
    pub sessions: &'a super::sessions::ChatSessionsHandle,
    pub approvals: &'a super::sessions::PendingApprovals,
}

impl ChildFocusContext<'_> {
    async fn start_message(
        &self,
        record: ChildSessionRecord,
        input: crate::messaging::runtime::MessageInput,
    ) -> anyhow::Result<String> {
        crate::runtime::mailbox_child::MailboxChildSource::admit(self.service, record, input, None)
            .await?
            .continue_turn(self.tools)
            .await
    }
    pub(super) async fn view(
        &self,
        target: &str,
        seq: u64,
        lines: Vec<String>,
        scroll_offset: usize,
    ) -> anyhow::Result<super::sessions::ActiveSessionView> {
        let record = self.record(target)?;
        let store = self.memory.child_session_store().context("child storage unavailable")?;
        let mut turns = store.history(&record)?;
        if let Some(run) = self.sessions.child_run(&record.session_key).await {
            let live = run.history.read().await;
            if live.len() > turns.len() {
                turns = live
                    .iter()
                    .filter(|turn| turn.role != "system")
                    .map(|turn| ChildTurn {
                        role: turn.role.clone(),
                        content: turn.content.clone(),
                        timestamp: turn.timestamp,
                    })
                    .collect();
            }
        }
        Ok(super::sessions::ActiveSessionView {
            seq,
            kind: "agent".into(),
            title: format!("{} / {} · {:?}", record.provider, record.model, record.level),
            turns,
            lines: if self.busy(&record).await { lines } else { Vec::new() },
            truncated: false,
            scroll_offset,
        })
    }
    pub(super) async fn restore(&self) -> anyhow::Result<()> {
        let workspace = self.generation.effective.workspace_dir.to_string_lossy();
        let owner = super::chat_runtime_envelope(&workspace, self.parent).resolved_owner_id();
        let Some(store) = self.memory.child_session_store() else {
            return Ok(());
        };
        for record in store
            .sessions(&workspace, &owner)?
            .into_iter()
            .filter(|record| record.parent_session == self.parent)
        {
            let history = store.history(&record)?;
            let id = record.runs.last().cloned().context("child session has no run")?;
            let output = history
                .iter()
                .rev()
                .find(|turn| turn.role == "assistant")
                .map_or_else(String::new, |turn| turn.content.clone());
            self.sessions
                .restore_child(SubAgentRun {
                    id,
                    task: history
                        .first()
                        .map_or_else(|| "Child session".into(), |turn| turn.content.clone()),
                    owner_id: Some(owner.clone()),
                    topic_id: None,
                    source_message_event_id: None,
                    started_at: record.created_at,
                    finished_at: record.ended_at.or_else(|| Some(chrono::Utc::now())),
                    status: SubAgentStatus::Completed(output),
                    recipient: None,
                    channel_name: Some("terminal".into()),
                    abort_handle: None,
                    process_control: None,
                    task_control: None,
                    history: Arc::new(tokio::sync::RwLock::new(
                        history
                            .into_iter()
                            .map(|turn| crate::tools::sessions_spawn::HistoryEntry {
                                role: turn.role,
                                content: turn.content,
                                timestamp: turn.timestamp,
                            })
                            .collect(),
                    )),
                    steer_tx: None,
                    parent_run_id: Some(record.parent_run),
                    session_scope_key: record.session_key,
                    spawn_depth: 0,
                    delegation: None,
                    token_usage_records: Vec::new(),
                    progress: crate::agent::idle::child_beat(),
                    batch_id: None,
                    agent_partition: None,
                })
                .await;
        }
        Ok(())
    }
    fn record(&self, target: &str) -> anyhow::Result<ChildSessionRecord> {
        let workspace = self.generation.effective.workspace_dir.to_string_lossy();
        let owner = super::chat_runtime_envelope(&workspace, self.parent).resolved_owner_id();
        self.memory
            .child_session_store()
            .context("child session storage unavailable")?
            .sessions(&workspace, &owner)?
            .into_iter()
            .find(|record| {
                record.parent_session == self.parent
                    && (record.session_key == target || record.runs.iter().any(|run| run == target))
            })
            .context("child_session_unknown")
    }

    async fn busy(&self, record: &ChildSessionRecord) -> bool {
        self.sessions.child_run(&record.session_key).await.is_some_and(|run| {
            matches!(
                run.status,
                SubAgentStatus::Running | SubAgentStatus::AwaitingInput { .. }
            )
        })
    }

    fn provider(&self, record: &ChildSessionRecord) -> anyhow::Result<Arc<dyn crate::providers::Provider>> {
        let config = &self.generation.effective;
        let (_, provider) = super::bootstrap::chat_provider_chain(
            config,
            &record.provider,
            crate::providers::selection::default_provider(config),
            &record.model,
            &crate::providers::provider_runtime_options_from_config(config),
        )?;
        Ok(Arc::from(provider))
    }

    async fn start(&self, record: ChildSessionRecord, message: ChannelMessage) -> anyhow::Result<String> {
        let task = message.content.strip_prefix("/now ").unwrap_or(&message.content);
        let provider = self.provider(&record)?;
        let binding = ChatProviderBinding::capture(self.generation, &provider, &record.provider, &record.model)?
            .context("child provider binding missing")?;
        let command = ChatOperatorSpawn::capture_child(&message, task, record, binding)?;
        let (mut args, source) = command
            .admit(
                Arc::clone(self.memory),
                self.parent,
                Arc::clone(self.security),
                Arc::clone(self.service),
            )
            .await?;
        let grant = serde_json::to_value(crate::security::policy::ApprovalGrant::for_resource_operation(
            "sessions_spawn",
            "sessions_spawn:spawn",
            "chat-operator",
            None,
        ))?;
        args.as_object_mut()
            .context("child arguments must be an object")?
            .insert(crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG.into(), grant);
        self.execute_source(source, args).await
    }
    async fn execute_source(
        &self,
        source: crate::runtime::operator_spawn::ChildLaunchSource,
        args: serde_json::Value,
    ) -> anyhow::Result<String> {
        let tool = self
            .tools
            .iter()
            .find(|tool| tool.supports_name("sessions_spawn"))
            .context("child execution unavailable")?;
        let result = source.scope(tool.execute_named("sessions_spawn", args)).await?;
        anyhow::ensure!(
            result.success,
            "{}",
            result.error.as_deref().unwrap_or("child continuation failed")
        );
        Ok(result.output)
    }
}

impl ChildFocus {
    pub(super) async fn submit(
        &mut self,
        target: &str,
        message: ChannelMessage,
        ctx: &ChildFocusContext<'_>,
    ) -> anyhow::Result<Option<String>> {
        let command = message.content.split_whitespace().next().unwrap_or_default();
        let scoped = matches!(
            command,
            "/clear"
                | "/compact"
                | "/model"
                | "/tools"
                | "/cost"
                | "/export"
                | "/copy"
                | "/transcript"
                | "/rewind"
                | "/approve"
                | "/deny"
                | "/help"
                | "/level"
        );
        if command.starts_with('/') && !scoped && command != "/now" {
            return Ok(None);
        }
        anyhow::ensure!(
            super::is_local_operator_input(&message) && !message.sender_is_bot,
            "child focus requires local operator input"
        );
        let mut record = ctx.record(target)?;
        if !scoped {
            if ctx.busy(&record).await {
                if command == "/now" {
                    self.queue.push_front((record.session_key, message));
                } else {
                    self.queue.push_back((record.session_key, message));
                }
                return Ok(Some("Queued for the child session.".into()));
            }
            return ctx.start(record, message).await.map(Some);
        }
        if matches!(command, "/approve" | "/deny") {
            let run = ctx
                .sessions
                .child_run(&record.session_key)
                .await
                .context("child is not running")?;
            let decision = if command == "/approve" {
                crate::agent::turn_contracts::ApprovalDecision::Grant
            } else {
                crate::agent::turn_contracts::ApprovalDecision::Deny
            };
            return Ok(Some(
                if ctx.approvals.resolve(&run.id, decision) {
                    "Child approval resolved."
                } else {
                    "No pending child approval."
                }
                .into(),
            ));
        }
        if command == "/level" {
            return Ok(Some(format!(
                "Child level: {:?} (inherited from its parent).",
                record.level
            )));
        }
        let store = ctx.memory.child_session_store().context("child storage unavailable")?;
        let mut turns = store.history(&record)?;
        if matches!(command, "/clear" | "/compact" | "/rewind") || message.content.starts_with("/model ") {
            anyhow::ensure!(
                !ctx.busy(&record).await,
                "child_session_busy: wait for the current turn before changing context"
            );
            match command {
                "/clear" => turns.clear(),
                "/model" => {
                    let model = message.content.strip_prefix("/model ").unwrap_or_default().trim();
                    super::bootstrap::chat_model_switch(&ctx.generation.effective, &record.provider, model)?;
                    record.model = model.to_owned();
                }
                "/rewind" => {
                    let index = message
                        .content
                        .strip_prefix("/rewind")
                        .unwrap_or_default()
                        .trim()
                        .parse::<usize>()?;
                    let boundary = turns
                        .iter()
                        .enumerate()
                        .filter(|(_, turn)| turn.role == "user")
                        .nth(index.saturating_sub(1))
                        .map(|(index, _)| index)
                        .context("unknown child turn")?;
                    turns.truncate(boundary);
                }
                "/compact" => {
                    let provider = ctx.provider(&record)?;
                    let runtime = crate::messaging::runtime::MessagingRuntime::for_config(&ctx.generation.effective)?;
                    let address = runtime.address(&record.owner, &record.session_key, &record.agent);
                    let scope = runtime.resolve_scope(&address)?;
                    let mut envelope = crate::runtime::envelope::RuntimeEnvelope::sessions_spawn(
                        &record.workspace,
                        &record.session_key,
                        uuid::Uuid::new_v4().to_string(),
                    );
                    envelope.owner_id = Some(record.owner.clone());
                    envelope.owner_isolated = true;
                    // Unnamed child transcript rows carry no presentation
                    // agent id. The verified partition already confines reads.
                    envelope.agent_id = (record.agent != "main").then(|| record.agent.clone());
                    envelope.agent_partition = Some(runtime.store.partition(&scope)?);
                    let audit = crate::agent::document_ingest::DocumentIngestRuntime::from_envelope(
                        Arc::clone(ctx.memory),
                        &envelope,
                    )
                    .bound_to_newest_user_input()
                    .await?
                    .context("child compaction requires a durable transcript")?;
                    let mut history: Vec<_> = turns
                        .iter()
                        .map(|turn| {
                            crate::agent::turn_contracts::stored_turn_message(turn.role.clone(), turn.content.clone())
                        })
                        .collect();
                    let source = crate::agent::turn_history::CanonicalHistory::from_acknowledged(
                        None,
                        turns.iter().map(|turn| (turn.role.as_str(), turn.content.as_str())),
                    );
                    let mut compaction = ctx.generation.effective.agent.compaction.clone();
                    let mut built = None;
                    // Steering segments can have distinct run provenance. Keep
                    // more recent turns rather than invent an exact handoff for
                    // an ambiguous cold window. Every attempted patch still
                    // goes through the normal transcript provenance checks.
                    const MAX_CHILD_COMPACTION_BOUNDARIES: usize = 16;
                    for _ in 0..MAX_CHILD_COMPACTION_BOUNDARIES {
                        if let crate::agent::compaction::CompactionBuild::Patch(patch) =
                            crate::agent::compaction::build_configurable_compaction_patch_with_source_history(
                                &history,
                                &source,
                                provider.as_ref(),
                                &record.model,
                                &compaction,
                                Some(&audit),
                                crate::agent::compaction::MANUAL_COMPACT_TRIGGER,
                            )
                            .await?
                        {
                            built = Some(patch);
                            break;
                        }
                        if compaction.mode != crate::config::AgentCompactionMode::Switch
                            || compaction.keep_recent_messages >= history.len()
                        {
                            break;
                        }
                        compaction.keep_recent_messages = compaction.keep_recent_messages.saturating_add(1);
                    }
                    if let Some(patch) = built {
                        crate::agent::compaction::apply_compaction_patch_exact(&mut history, &patch)?;
                        turns = history
                            .into_iter()
                            .map(|message| ChildTurn {
                                role: message.role,
                                content: message.content,
                                timestamp: chrono::Utc::now(),
                            })
                            .collect();
                    } else {
                        return Ok(Some(
                            "Child compaction unavailable: no older complete window has exact durable transcript provenance. Context retained.".into(),
                        ));
                    }
                }
                _ => {}
            }
            let count = turns.len();
            store.replace_context(&record, &turns)?;
            if let Some(run) = ctx.sessions.child_run(&record.session_key).await {
                *run.history.write().await = turns
                    .iter()
                    .map(|turn| crate::tools::sessions_spawn::HistoryEntry {
                        role: turn.role.clone(),
                        content: turn.content.clone(),
                        timestamp: turn.timestamp,
                    })
                    .collect();
            }
            return Ok(Some(format!("Child session updated: {count} context messages.")));
        }
        let mut session = super::session::ChatSession::new(&record.provider, &record.model);
        session.id = record.session_key.clone();
        session.turns = turns
            .iter()
            .map(|turn| super::session::ChatTurn {
                role: turn.role.clone(),
                content: turn.content.clone(),
                timestamp: turn.timestamp,
                tool_calls: Vec::new(),
                context_handoff: false,
                tool_evidence: None,
            })
            .collect();
        if let Some(run) = ctx.sessions.child_run(&record.session_key).await {
            session.token_usage_records = run.token_usage_records;
        }
        if command == "/transcript" {
            return Ok(Some(
                turns
                    .iter()
                    .map(|turn| format!("{}: {}", turn.role, turn.content))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ));
        }
        if command == "/tools" {
            let configured = ctx.generation.effective.agents.get(&record.agent);
            let names = ctx
                .tools
                .iter()
                .filter(|tool| !matches!(tool.name(), "sessions_spawn" | "delegate"))
                .filter(|tool| {
                    record.level == crate::security::AutonomyLevel::Full
                        || configured.is_none_or(|agent| {
                            agent.allowed_tools.is_empty()
                                || agent
                                    .allowed_tools
                                    .iter()
                                    .any(|name| name == "*" || tool.supports_name(name))
                        })
                })
                .map(|tool| tool.name())
                .collect::<Vec<_>>();
            return Ok(Some(format!("Child session tools: {}", names.join(", "))));
        }
        if command == "/copy" {
            let selected =
                super::copy_select::select_copy_content(&session, &message.content).map_err(anyhow::Error::msg)?;
            super::terminal_proto::copy_to_clipboard(&selected.content)?;
            return Ok(Some(super::copy_select::copy_success_message(&selected)));
        }
        let context = super::commands::CommandContext {
            model_name: &record.model,
            provider_name: &record.provider,
            chat_session: &session,
            tools_registry: ctx.tools,
            mem: ctx.memory.as_ref(),
        };
        match super::commands::dispatch(&message.content, &context).await {
            super::commands::CommandResult::HandledWithOutput(output) => Ok(Some(output)),
            super::commands::CommandResult::Handled => Ok(Some(String::new())),
            _ => anyhow::bail!("unsupported child session command"),
        }
    }

    pub(super) async fn pump(&mut self, ctx: &ChildFocusContext<'_>) -> Vec<String> {
        let mut outputs = Vec::new();
        if self.parent.as_deref() != Some(ctx.parent) {
            self.queue.clear();
            self.message_queue.clear();
            self.mailboxes.clear();
            self.parent = Some(ctx.parent.into());
        }
        if ctx.generation.effective.memory.backend == "sqlite" {
            let messages = async {
                let runtime = crate::messaging::runtime::MessagingRuntime::for_config(&ctx.generation.effective)?;
                self.message_queue.retain(|(_, input)| input.verify(&runtime).is_ok());
                let workspace = ctx.generation.effective.workspace_dir.to_string_lossy();
                let owner = super::chat_runtime_envelope(&workspace, ctx.parent).resolved_owner_id();
                let store = ctx.memory.child_session_store().context("child storage unavailable")?;
                for record in store
                    .sessions(&workspace, &owner)?
                    .into_iter()
                    .filter(|record| record.parent_session == ctx.parent)
                {
                    if ctx.busy(&record).await {
                        continue;
                    }
                    if !self.mailboxes.contains_key(&record.session_key) {
                        let inbox = runtime.register(runtime.address(&owner, &record.session_key, &record.agent))?;
                        self.mailboxes.insert(record.session_key.clone(), inbox);
                    }
                    let inputs = self
                        .mailboxes
                        .get_mut(&record.session_key)
                        .context("child inbox disappeared")?
                        .poll()?;
                    for input in inputs {
                        if input.auto_start() {
                            self.message_queue.push_back((record.session_key.clone(), input));
                        } else {
                            outputs.push(format!(
                                "Message recorded for {} (automatic reply chain stopped).",
                                record.session_key
                            ));
                        }
                    }
                }
                Ok::<(), anyhow::Error>(())
            }
            .await;
            if let Err(error) = messages {
                outputs.push(error.to_string());
            }
        }
        let mut message_index = 0;
        while message_index < self.message_queue.len() {
            let Some((target, _)) = self.message_queue.get(message_index) else {
                break;
            };
            let record = match ctx.record(target) {
                Ok(record) => record,
                Err(error) => {
                    self.message_queue.remove(message_index);
                    outputs.push(error.to_string());
                    continue;
                }
            };
            if ctx.busy(&record).await {
                message_index += 1;
                continue;
            }
            if let Some((_, input)) = self.message_queue.remove(message_index) {
                outputs.push(match ctx.start_message(record, input).await {
                    Ok(output) => output,
                    Err(error) => error.to_string(),
                });
            }
        }
        let mut index = 0;
        while index < self.queue.len() {
            let Some((target, _)) = self.queue.get(index) else {
                break;
            };
            let record = match ctx.record(target) {
                Ok(record) => record,
                Err(error) => {
                    self.queue.remove(index);
                    outputs.push(error.to_string());
                    continue;
                }
            };
            if ctx.busy(&record).await {
                index += 1;
                continue;
            }
            if let Some((_, message)) = self.queue.remove(index) {
                outputs.push(match ctx.start(record, message).await {
                    Ok(output) => output,
                    Err(error) => error.to_string(),
                });
            }
        }
        outputs
    }
}
