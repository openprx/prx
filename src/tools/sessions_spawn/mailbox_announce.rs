//! A finished child's completion, delivered to its gateway parent session
//! through the session mailbox.
//!
//! The gateway has no outbound channel of its own, so a result announcement
//! addressed to a Console, rf or webhook session used to be dropped with a log
//! line. Such a session already receives messages from other sessions through
//! the mailbox: the receiver adopts the message as one inbound row and wakes a
//! turn, after the running turn if there is one. The announcement takes the
//! same route, sent from the child's own session address, so the row reads
//! `[message from spawn:<run id>/<agent>]` and is marked like every adopted
//! mailbox message (source `prx_mailbox`, channel `mailbox`), not as text the
//! user typed.

use crate::config::Config;
use crate::context::transcript::Transcript;
use crate::memory::{MemoryFabric, MemoryPrincipal, MessageEventScope};
use crate::messaging::mailbox::MailboxSendState;
use crate::messaging::runtime::MessagingRuntime;
use crate::messaging::send::MailboxSender as _;
use crate::runtime::envelope::RuntimeSource;
use crate::tools::execution::ChildCallContext;
use std::sync::Arc;

/// Result text carried in one announcement; the full result stays retrievable
/// through `join` and `sessions_list`.
const ANNOUNCE_RESULT_MAX_CHARS: usize = 16_000;

/// Document references listed in one announcement; the rest is counted.
const ANNOUNCE_DOCUMENT_MAX_REFS: usize = 20;

/// The documents a finished run stored, read from its own tool rows in the
/// child session (the principal is the child's scope: owner, session and
/// agent partition). A failed read gives none.
pub(super) async fn run_document_refs(fabric: &MemoryFabric, scope: &MessageEventScope, run_id: &str) -> Vec<String> {
    let principal = MemoryPrincipal {
        workspace_id: fabric.workspace_id().to_owned(),
        agent_id: scope.agent_id.clone(),
        persona_id: scope.persona_id.clone(),
        session_key: scope.session_key.clone(),
        channel: scope.channel.clone(),
        sender: scope.sender.clone(),
        owner_id: scope.owner_id.clone(),
        agent_partition: scope.agent_partition.clone(),
        ..MemoryPrincipal::default()
    };
    let memory = fabric.memory();
    Transcript::new(memory.as_ref(), principal)
        .run_document_ids(run_id)
        .await
}

/// Where a run's completion goes: the parent session that launched it.
#[derive(Clone)]
pub(super) struct MailboxAnnouncement {
    runtime: Arc<MessagingRuntime>,
    owner: String,
    parent_session: String,
    parent_agent: String,
    child_session: String,
    child_agent: String,
    source_event: Option<String>,
}

impl MailboxAnnouncement {
    /// The announcement route of a run launched from a gateway session, or
    /// `None` when the parent is not one (terminal chat and channels announce
    /// on their own channel) or the mailbox is unavailable. A run whose
    /// recipient names something other than the parent session keeps that
    /// explicit routing.
    pub(super) fn capture(
        caller: &Option<ChildCallContext>,
        config: &Config,
        recipient: Option<&str>,
        child_session: &str,
        child_agent: Option<&str>,
    ) -> Option<Self> {
        let envelope = &caller.as_ref()?.context().envelope;
        if !matches!(envelope.source, RuntimeSource::Console | RuntimeSource::Gateway) {
            return None;
        }
        if recipient.is_some_and(|recipient| recipient != envelope.session_key) {
            return None;
        }
        let runtime = match MessagingRuntime::for_config(config) {
            Ok(runtime) => runtime,
            Err(error) => {
                tracing::debug!(
                    error = %crate::agent::diagnostics::error_class(&error),
                    "sessions_spawn mailbox announcement unavailable"
                );
                return None;
            }
        };
        Some(Self {
            runtime,
            owner: envelope.resolved_owner_id(),
            parent_session: envelope.session_key.clone(),
            parent_agent: envelope.agent_id.clone().unwrap_or_else(|| "main".to_owned()),
            child_session: child_session.to_owned(),
            child_agent: child_agent.unwrap_or("main").to_owned(),
            source_event: envelope.source_message_event_id.clone(),
        })
    }

    /// Send the completion to the parent session. `false` leaves the run's
    /// own channel as the fallback; the result stays retrievable either way.
    /// A run of a deleted rf turn's family announces nothing (`true`, so no
    /// fallback either): the delete erases the run's rows, and an
    /// announcement would carry its result into a new parent turn.
    pub(super) async fn deliver(&self, run_id: &str, completed: bool, result_text: &str, documents: &[String]) -> bool {
        if self.family_deleted(run_id).await {
            tracing::info!(run_id = %run_id, "sessions_spawn announcement withheld: the turn that started the run was deleted");
            return true;
        }
        let text = announcement_text(run_id, completed, result_text, &self.child_session, documents);
        let runtime = Arc::clone(&self.runtime);
        let source = runtime.address(&self.owner, &self.child_session, &self.child_agent);
        let target = runtime.address(&self.owner, &self.parent_session, &self.parent_agent);
        let event = self.source_event.clone();
        let bound = crate::runtime::blocking::spawn_blocking(move || runtime.bind(source, event.as_deref())).await;
        let sender = match bound {
            Ok(Ok(sender)) => sender,
            Ok(Err(error)) => {
                tracing::warn!(run_id = %run_id, error = %error, "sessions_spawn mailbox announcement refused");
                return false;
            }
            Err(error) => {
                tracing::warn!(run_id = %run_id, error = %crate::agent::diagnostics::error_class(&error), "sessions_spawn mailbox announcement refused");
                return false;
            }
        };
        match sender.send(target, text, "text/plain".into()).await {
            Ok(published) if published.state != MailboxSendState::Failed => {
                tracing::info!(run_id = %run_id, "sessions_spawn announcement delivered to the parent session mailbox");
                true
            }
            Ok(_) => {
                tracing::warn!(run_id = %run_id, "sessions_spawn mailbox announcement failed to publish");
                false
            }
            Err(error) => {
                tracing::warn!(run_id = %run_id, error = %error, "sessions_spawn mailbox announcement refused");
                false
            }
        }
    }
}

impl MailboxAnnouncement {
    /// Whether the rf turn this run descends from was deleted (message or
    /// reply, settled or pending). A failed read announces as before.
    async fn family_deleted(&self, run_id: &str) -> bool {
        let workspace = std::path::PathBuf::from(&self.runtime.workspace);
        let memory = match crate::runtime::blocking::spawn_blocking(move || {
            crate::memory::SqliteMemory::new(&workspace)
        })
        .await
        {
            Ok(Ok(memory)) => memory,
            Ok(Err(error)) => {
                tracing::debug!(run_id = %run_id, error = %crate::agent::diagnostics::error_class(&error), "sessions_spawn deleted-turn check unavailable");
                return false;
            }
            Err(error) => {
                tracing::debug!(run_id = %run_id, error = %crate::agent::diagnostics::error_class(&error), "sessions_spawn deleted-turn check unavailable");
                return false;
            }
        };
        match memory.rf_run_family_deleted(run_id).await {
            Ok(deleted) => deleted,
            Err(error) => {
                tracing::debug!(run_id = %run_id, error = %crate::agent::diagnostics::error_class(&error), "sessions_spawn deleted-turn check failed");
                false
            }
        }
    }
}

/// The inbound message the parent session reads: which run finished, how, and
/// its result (bounded).
fn announcement_text(
    run_id: &str,
    completed: bool,
    result_text: &str,
    child_session: &str,
    documents: &[String],
) -> String {
    let outcome = if completed { "completed" } else { "FAILED" };
    let head = format!(
        "Sub-agent `{run_id}` has finished ({outcome}). This is an automatic completion notice for a run you already started, not a new request; that run's task is done. Report this result to the user if useful; the request that started the run is already done, do not start it again."
    );
    let detail = result_text.trim();
    let mut references = format!(
        "\n\n[References]\n- transcript {child_session}: read it with sessions_spawn action=history run_id={run_id} transcript=true, or search it with transcript_search session_key={child_session}"
    );
    let mut document_lines: Vec<String> = documents
        .iter()
        .take(ANNOUNCE_DOCUMENT_MAX_REFS)
        .map(|id| format!("\n- document {id}: find its chunks with document_search"))
        .collect();
    let omitted = documents.len().saturating_sub(ANNOUNCE_DOCUMENT_MAX_REFS);
    if omitted > 0 {
        document_lines.push(format!("\n+{omitted} more"));
    }
    references.extend(document_lines);
    if detail.is_empty() {
        return format!("{head}\nResult: (empty){references}");
    }
    format!(
        "{head}\nResult:\n{}{references}",
        crate::util::truncate_with_ellipsis(detail, ANNOUNCE_RESULT_MAX_CHARS)
    )
}
