//! Owner-scoped memory fixture for the two memory-enrichment integration cases.
//! Turns open prediction-free (R9): the embedded turn recalls this fact by its
//! own scoped recall, never through an engine-selected retrieval.
//!
//! The embedded Agent recalls from its own `agent_api` session, which it mints
//! itself. A warm-up turn makes the Agent record a real input event in that
//! session; [`Driver::seed_agent_session`] reads the session from that durable
//! event and stores the fact there, so the following turns recall it.
use anyhow::{Context as _, Result, ensure};
use openprx::memory::{Memory, MemoryCategory, MemoryStoreMetadata, SqliteMemory};
use std::sync::Arc;

/// The input every case sends first, before the fact exists.
pub const WARM_UP_INPUT: &str = "warm-up";

pub struct Driver {
    memory: Arc<SqliteMemory>,
    workspace_id: String,
    fact: String,
}

impl Driver {
    pub fn start(memory: Arc<SqliteMemory>, workspace: &std::path::Path, fact: &str) -> Self {
        Self {
            memory,
            workspace_id: workspace.to_string_lossy().into_owned(),
            fact: fact.to_owned(),
        }
    }

    /// Stores the fact in the session of the Agent's warm-up input, under the
    /// owner and write scope the embedded turn reads with.
    pub async fn seed_agent_session(&self) -> Result<()> {
        // The embedded caller's owner; the session is the Agent's own.
        let caller = openprx::runtime::envelope::RuntimeEnvelope::agent(&self.workspace_id, "prx-memory-seed")
            .with_channel("agent_api")
            .with_sender("embedded-caller");
        let owner = caller.resolved_owner_id();
        ensure!(!owner.is_empty(), "fixture owner projection");
        let inputs = self
            .memory
            .list_owned_message_events_by_type(&self.workspace_id, &owner, "message.created", 0, 16)
            .await?;
        let warm_up = inputs
            .iter()
            .find(|event| event.role == "user" && event.content == WARM_UP_INPUT)
            .context("the warm-up turn recorded no input event")?;
        let session = warm_up
            .session_key
            .clone()
            .context("the warm-up input carries no session")?;
        let run = warm_up.run_id.clone().context("the warm-up input carries no run")?;
        let mut envelope = openprx::runtime::envelope::RuntimeEnvelope::agent(&self.workspace_id, &run)
            .with_channel("agent_api")
            .with_sender("embedded-caller")
            .with_recipient(session.clone());
        envelope.session_key.clone_from(&session);
        ensure!(
            envelope.resolved_owner_id() == owner,
            "the session belongs to the caller"
        );
        let mut scope = envelope.memory_write_context("private");
        scope.owner_id = Some(owner.clone());
        scope.require_owner_match = true;
        self.memory
            .store_with_context_and_metadata(
                "prx_selected_fact",
                &self.fact,
                MemoryCategory::Core,
                Some(&session),
                Some(&scope),
                MemoryStoreMetadata {
                    workspace_id: Some(self.workspace_id.clone()),
                    owner_id: Some(owner),
                    channel: Some("agent_api".to_owned()),
                    ..Default::default()
                },
            )
            .await?;
        Ok(())
    }
}

pub struct AgentFixture(pub openprx::agent::agent::Agent);
impl AgentFixture {
    pub fn turn<'a>(
        &'a mut self,
        message: &'a str,
    ) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<String>> + 'a>> {
        Box::pin(self.0.turn(message))
    }
}
impl std::ops::Deref for AgentFixture {
    type Target = openprx::agent::agent::Agent;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
