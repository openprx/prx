//! The main chat consumes mailbox data as model input, never as an operator command.
use crate::channels::traits::ChannelMessage;
use crate::messaging::runtime::{MessageInput, MessagingRuntime, SessionInbox};
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
#[error("terminal_session_erased: cached history discarded; starting a new chat")]
pub struct SessionErased;

pub(super) async fn pin_epoch(config: &crate::config::Config, session: &str) -> anyhow::Result<()> {
    if config.memory.backend != "sqlite" {
        return Ok(());
    }
    let config = config.clone();
    let session = session.to_owned();
    crate::runtime::blocking::spawn_blocking(move || {
        let runtime = MessagingRuntime::for_config(&config)?;
        let owner = super::chat_runtime_envelope(&runtime.workspace, &session).resolved_owner_id();
        runtime.store.pin_terminal_epoch(&runtime.workspace, &owner, &session)
    })
    .await?
}

pub(super) struct ChatInbox {
    pub(super) runtime: Arc<MessagingRuntime>,
    inbox: SessionInbox,
    session: String,
    wake: bool,
    pending: std::collections::HashMap<String, MessageInput>,
}
impl ChatInbox {
    pub(super) fn open(config: &crate::config::Config, session: &str) -> anyhow::Result<Option<Self>> {
        if config.memory.backend != "sqlite" {
            return Ok(None);
        }
        let runtime = MessagingRuntime::for_config(config)?;
        let owner = super::chat_runtime_envelope(&config.workspace_dir.to_string_lossy(), session).resolved_owner_id();
        let inbox = runtime.register(runtime.address(&owner, session, "main"))?;
        Ok(Some(Self {
            runtime,
            inbox,
            session: session.into(),
            wake: true,
            pending: Default::default(),
        }))
    }
    /// Other local processes notify through durable storage while the owning
    /// gateway is live. Without it, a human turn is the next polling boundary.
    pub(super) async fn wake_from_daemon(&mut self, config: &crate::config::Config) {
        let Ok(path) = crate::runtime::host_bootstrap::runtime_path(config) else {
            return;
        };
        let port = crate::runtime::blocking::spawn_blocking(move || {
            use std::io::Read as _;
            const MAX_RUNTIME_BYTES: u64 = 4096;
            let mut bytes = Vec::new();
            std::fs::File::open(path)
                .ok()?
                .take(MAX_RUNTIME_BYTES + 1)
                .read_to_end(&mut bytes)
                .ok()?;
            if bytes.len() > MAX_RUNTIME_BYTES as usize {
                return None;
            }
            let record: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
            let url = reqwest::Url::parse(record.get("api_base")?.as_str()?).ok()?;
            if url.scheme() != "http" || url.host_str() != Some("127.0.0.1") {
                return None;
            }
            url.port()
        })
        .await
        .ok()
        .flatten();
        if let Some(port) = port
            && matches!(
                tokio::time::timeout(
                    std::time::Duration::from_millis(200),
                    tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
                )
                .await,
                Ok(Ok(_))
            )
        {
            self.wake();
        }
    }

    pub(super) const fn wake(&mut self) {
        self.wake = true;
    }
    pub(super) fn poll(&mut self) -> anyhow::Result<Vec<MessageInput>> {
        if !std::mem::take(&mut self.wake) && !self.inbox.take_notification() {
            return Ok(Vec::new());
        }
        let inputs = self.inbox.poll()?;
        for input in &inputs {
            if input.auto_start()
                && let Some(event) = input.receipt.adopted_message_event_id
            {
                self.pending.insert(event.to_string(), input.clone());
            }
        }
        Ok(inputs)
    }
    pub(super) fn fenced(&self) -> bool {
        !self.inbox.live()
    }
    pub(super) fn drain(&mut self) {
        self.pending.clear();
    }
    pub(super) fn take_input(&mut self, message: &ChannelMessage) -> anyhow::Result<Option<MessageInput>> {
        let Some(event) = cause(message) else {
            return Ok(None);
        };
        let input = self
            .pending
            .remove(event)
            .ok_or_else(|| anyhow::anyhow!("mailbox input is no longer queued"))?;
        input.verify(&self.runtime)?;
        anyhow::ensure!(input.text == message.content, "mailbox input content differs");
        Ok(Some(input))
    }
    pub(super) fn session(&self) -> &str {
        &self.session
    }
}

pub(super) fn channel_input(input: &MessageInput) -> anyhow::Result<ChannelMessage> {
    let event = input
        .receipt
        .adopted_message_event_id
        .ok_or_else(|| anyhow::anyhow!("adopted message event missing"))?;
    Ok(ChannelMessage {
        id: format!("mailbox:{event}"),
        sender: "session-message".into(),
        sender_is_bot: true,
        content: input.text.clone(),
        channel: "terminal".into(),
        reply_target: input.message.to.session.clone(),
        timestamp: u64::try_from(chrono::Utc::now().timestamp())?,
        ..ChannelMessage::default()
    })
}

pub(super) fn cause(message: &ChannelMessage) -> Option<&str> {
    (message.sender == "session-message" && message.sender_is_bot)
        .then(|| message.id.strip_prefix("mailbox:"))
        .flatten()
}

pub(super) async fn send_command(
    config: &crate::config::Config,
    memory: &Arc<dyn crate::memory::Memory>,
    parent: &str,
    focused: Option<&str>,
    message: &ChannelMessage,
    sessions: &mut super::sessions::ChatSessionsHandle,
) -> anyhow::Result<String> {
    use crate::messaging::send::MailboxSender;
    anyhow::ensure!(
        super::is_local_operator_input(message) && !message.sender_is_bot,
        "/msg requires local operator input"
    );
    let runtime = MessagingRuntime::for_config(config)?;
    let owner = super::chat_runtime_envelope(&config.workspace_dir.to_string_lossy(), parent).resolved_owner_id();
    let source_session = if let Some(focused) = focused {
        sessions
            .child_run(focused)
            .await
            .map(|run| run.session_scope_key)
            .unwrap_or_else(|| parent.into())
    } else {
        parent.into()
    };
    let mut source = runtime.address(&owner, &source_session, "main");
    if let Some(agent) = runtime
        .store
        .logical_agent(&runtime.workspace, &owner, &source_session)?
    {
        source.agent = agent;
    }
    let level = if source_session == parent {
        config.autonomy.level
    } else {
        let store = memory
            .child_session_store()
            .ok_or_else(|| anyhow::anyhow!("child store unavailable"))?;
        let record = store
            .sessions(&runtime.workspace, &owner)?
            .into_iter()
            .find(|record| record.session_key == source_session && record.parent_session == parent)
            .ok_or_else(|| anyhow::anyhow!("child_session_unknown"))?;
        record.level.min(
            crate::security::agent_level::named_child_level(config, Some(&record.agent), config.autonomy.level)
                .unwrap_or(config.autonomy.level),
        )
    };
    let policy = crate::security::SecurityPolicy {
        autonomy: level,
        ..crate::security::SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir)
    };
    crate::security::SideEffectGate::new(&policy)
        .authorize_resource_operation(
            "sessions_send",
            "sessions_send:message",
            crate::security::policy::ResourceRiskLevel::Low,
            None,
        )
        .map_err(anyhow::Error::msg)?;
    let (target, text) = super::commands::msg::parse(&message.content, &source)?;
    let mut target = serde_json::Value::String(target.session);
    if let Some(seq) = target
        .as_str()
        .and_then(|target| target.strip_prefix('#'))
        .and_then(|seq| seq.parse::<u64>().ok())
    {
        let run = sessions.resolve_run_id(seq).await?;
        let run = sessions
            .child_run(&run)
            .await
            .ok_or_else(|| anyhow::anyhow!("message target is not a child session"))?;
        target = run.session_scope_key.into();
    }
    let target = runtime.alias(&source, &target)?;
    let sender = runtime.bind(source, None)?;
    Ok(crate::messaging::send::send_result(sender.send(target, text, "text/plain".into()).await).to_string())
}
