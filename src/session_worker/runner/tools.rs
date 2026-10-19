use crate::channels::traits::{Channel, ChannelMessage, SendMessage};
use crate::session_worker::protocol::WorkerManifest;
use crate::tools::Tool;
use anyhow::Result;
use std::sync::Arc;

pub(super) const WORKER_ORCHESTRATION_TOOL_NAMES: &[&str] = &[
    "sessions_list",
    "sessions_send",
    "subagents",
    "sessions_history",
    "session_status",
    "sessions_spawn",
    "message_send",
    "image",
    "config_reload",
    "gateway",
];

/// Process workers do not own an inbound channel connection. Session result
/// announcements remain observable through the parent run registry, so nested
/// workers use a silent channel instead of writing into the stdout IPC stream.
pub(super) struct WorkerChannel;

#[async_trait::async_trait]
impl Channel for WorkerChannel {
    fn name(&self) -> &str {
        "session-worker"
    }

    async fn send(&self, _message: &SendMessage) -> anyhow::Result<()> {
        Ok(())
    }

    async fn listen(&self, _tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        anyhow::bail!("session-worker channel does not accept inbound messages")
    }
}

pub(super) async fn select_tools_for_worker(
    source: Vec<Box<dyn Tool>>,
    allowed_tools: &[String],
    channel_excluded_tools: &[String],
) -> Result<Arc<Vec<Box<dyn Tool>>>> {
    let normalized = allowed_tools
        .iter()
        .map(|name| name.trim())
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    if normalized.contains(&"*") && normalized.as_slice() != ["*"] {
        anyhow::bail!("Worker allowed_tools must use '*' exclusively");
    }

    let source = Arc::new(source);
    crate::tools::sessions_spawn::refresh_tools_for_explicit_allowlist(&source, allowed_tools).await?;
    Ok(crate::tools::sessions_spawn::resolve_tools_for_agent(
        source,
        "session-worker",
        crate::tools::sessions_spawn::MemoryScope::Shared,
        (!normalized.is_empty()).then_some(allowed_tools),
        channel_excluded_tools,
    ))
}

/// Bind a worker manifest to the tool filter.
///
/// This is the only place the manifest's two tool lists reach
/// [`select_tools_for_worker`]. It takes the whole [`WorkerManifest`] rather
/// than two `&[String]` arguments read out at the call site so that the wiring
/// itself is reachable from a test: a caller cannot hand the filter a list that
/// did not come from the sealed manifest, and
/// `a_worker_manifest_binds_its_channel_exclusions_to_the_tool_filter` exercises
/// this binding instead of the filter alone.
pub(super) async fn select_tools_for_manifest(
    source: Vec<Box<dyn Tool>>,
    manifest: &WorkerManifest,
) -> Result<Arc<Vec<Box<dyn Tool>>>> {
    select_tools_for_worker(source, &manifest.allowed_tools, &manifest.channel_excluded_tools).await
}

pub(super) fn verify_worker_orchestration_tool_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Result<()> {
    let registered = names.into_iter().collect::<std::collections::HashSet<_>>();
    let missing = WORKER_ORCHESTRATION_TOOL_NAMES
        .iter()
        .copied()
        .filter(|name| !registered.contains(name))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        anyhow::bail!(
            "Session worker is missing required orchestration tools: {}",
            missing.join(", ")
        );
    }
    Ok(())
}
