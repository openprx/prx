//! Owner-scoped memory fixture for the two memory-enrichment integration cases.
//! Turns open prediction-free (R9): the embedded turn recalls this fact by its
//! own scoped recall, never through an engine-selected retrieval.
use anyhow::{Result, ensure};
use openprx::memory::{Memory, MemoryCategory, MemoryStoreMetadata, SqliteMemory};
use std::sync::Arc;

/// The fact is stored before the agent's turns, in the owner scope the
/// embedded `agent_api` caller's turns recall from.
pub struct Driver;

impl Driver {
    pub async fn start(memory: Arc<SqliteMemory>, workspace: std::path::PathBuf, fact: &str) -> Result<Self> {
        let workspace_id = workspace.to_string_lossy().into_owned();
        let envelope = openprx::runtime::envelope::RuntimeEnvelope::agent(&workspace_id, "prx-memory-seed")
            .with_channel("agent_api")
            .with_sender("embedded-caller");
        let owner = envelope.resolved_owner_id();
        ensure!(!owner.is_empty(), "fixture owner projection");
        let mut scope = envelope.memory_write_context("private");
        scope.owner_id = Some(owner.clone());
        scope.require_owner_match = true;
        memory
            .store_with_context_and_metadata(
                "prx_selected_fact",
                fact,
                MemoryCategory::Core,
                None,
                Some(&scope),
                MemoryStoreMetadata {
                    workspace_id: Some(workspace_id),
                    owner_id: Some(owner),
                    channel: Some("agent_api".to_owned()),
                    ..Default::default()
                },
            )
            .await?;
        Ok(Self)
    }

    pub async fn run<T>(self, future: impl std::future::Future<Output = T>) -> T {
        future.await
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
