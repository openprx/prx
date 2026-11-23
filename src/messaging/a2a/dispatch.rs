//! Independently scheduled peers under the workspace process lease.
use super::outbound::RemoteTransport;
use crate::messaging::{
    PrxMessage, SessionAddr,
    mailbox::{MailboxError, MailboxScope},
    runtime::MessagingRuntime,
};
use anyhow::Result;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
pub(super) struct Pending {
    pub scope: MailboxScope,
    pub source: SessionAddr,
    pub message: PrxMessage,
    pub attempts: u32,
}
fn ready(runtime: &MessagingRuntime, mut selected: HashSet<String>) -> Result<Vec<Pending>> {
    let mut pending = Vec::new();
    for (scope, source) in runtime.store.outbox_sources()? {
        for outbound in runtime.store.pending_outbox(&scope, &source)? {
            let message = outbound.message();
            if message.to.instance == runtime.instance || !selected.insert(message.to.instance.clone()) {
                continue;
            }
            let message = message.clone();
            let (attempts, created) = runtime.store.outbox_retry(&scope, &message)?;
            if chrono::Utc::now().timestamp().saturating_sub(created) >= 86400 {
                runtime.store.retry_outbox(&scope, &source, &message, None, "expired")?;
            } else {
                pending.push(Pending {
                    scope: scope.clone(),
                    source: source.clone(),
                    message,
                    attempts,
                });
            }
        }
    }
    Ok(pending)
}
pub(super) async fn run(
    transport: Arc<RemoteTransport>,
    runtime: Arc<MessagingRuntime>,
    lease: Arc<std::fs::File>,
) -> Result<()> {
    let mut leader = false;
    let mut peers = HashSet::new();
    let mut tasks = HashMap::new();
    let mut jobs = tokio::task::JoinSet::new();
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            result = jobs.join_next_with_id(), if !jobs.is_empty() => {
                match result {
                    Some(Ok((id, result))) => {
                        if let Some(peer) = tasks.remove(&id) { peers.remove(&peer); }
                        if let Err(error) = result { tracing::error!(error=%crate::agent::diagnostics::error_class(&error), "A2A peer dispatcher failed; retrying durable work"); }
                    }
                    Some(Err(error)) => {
                        if let Some(peer) = tasks.remove(&error.id()) { peers.remove(&peer); }
                        tracing::error!("A2A peer dispatcher exited; restarting from durable work");
                    }
                    None => {}
                }
            }
            _ = interval.tick() => {
                if !leader {
                    let lease = Arc::clone(&lease);
                    match crate::runtime::blocking::spawn_blocking(move || super::leader::claim(&lease)).await? {
                        Ok(true) => leader = true,
                        Ok(false) => continue,
                        Err(error) => { tracing::error!(error=%crate::agent::diagnostics::error_class(&error), "A2A dispatcher lease failed; retrying"); continue; }
                    }
                }
                let store = Arc::clone(&runtime);
                let busy = peers.clone();
                for pending in crate::runtime::blocking::spawn_blocking(move || ready(&store, busy)).await?? {
                    let peer = pending.message.to.instance.clone();
                    if !peers.insert(peer.clone()) { continue; }
                    let transport = Arc::clone(&transport);
                    let runtime = Arc::clone(&runtime);
                    let task = jobs.spawn(async move { transport.deliver(runtime, pending).await });
                    tasks.insert(task.id(), peer);
                }
            }
        }
    }
}
pub(super) fn permanent(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<MailboxError>(),
        Some(MailboxError::Unauthorized | MailboxError::StaleIncarnation)
    )
}
