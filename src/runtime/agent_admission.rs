//! Shared admission for named-agent root turns. Children hold no root slot.
use crate::config::Config;
use parking_lot::Mutex;
use std::{collections::BTreeMap, sync::LazyLock};
use tokio::sync::Notify;

type Key = (String, String, String);
static TURNS: LazyLock<Mutex<BTreeMap<Key, u32>>> = LazyLock::new(Default::default);
static RELEASED: Notify = Notify::const_new();

#[derive(Debug, Clone, Copy, thiserror::Error)]
pub(crate) enum AdmissionError {
    #[error("agent_busy")]
    Busy,
    #[error("agent_budget_exhausted")]
    Budget,
    #[error("storage_unavailable")]
    Storage,
}

#[derive(Debug)]
pub(crate) struct AgentTurnPermit(Key);
impl Drop for AgentTurnPermit {
    fn drop(&mut self) {
        let mut turns = TURNS.lock();
        if let Some(count) = turns.get_mut(&self.0) {
            *count = count.saturating_sub(1);
            if *count == 0 { turns.remove(&self.0); }
        }
        drop(turns);
        RELEASED.notify_waiters();
    }
}

pub(crate) fn admit(config: &Config, owner: &str, name: &str) -> Result<Option<AgentTurnPermit>, AdmissionError> {
    let Some(agent) = config.agents.get(name) else { return Ok(None); };
    let key = (config.workspace_dir.to_string_lossy().into_owned(), owner.to_owned(), name.to_owned());
    let mut turns = TURNS.lock();
    let count = turns.get(&key).copied().unwrap_or(0);
    if agent.limits.max_concurrent_turns.is_some_and(|limit| count >= limit) { return Err(AdmissionError::Busy); }
    if let Some(cents) = agent.limits.max_cost_per_day_cents {
        let root = crate::cost::types::RootAgent { owner_id: owner.to_owned(), name: name.to_owned() };
        let spent = crate::cost::tracker::CostTracker::for_workspace(config.cost.clone(), &config.workspace_dir)
            .and_then(|tracker| tracker.root_agent_spend_today(&root)).map_err(|_| AdmissionError::Storage)?;
        if spent >= u128::from(cents) * 10_000 { return Err(AdmissionError::Budget); }
    }
    let next = count.checked_add(1).ok_or(AdmissionError::Busy)?;
    turns.insert(key.clone(), next);
    Ok(Some(AgentTurnPermit(key)))
}

pub(crate) async fn wait(config: &Config, owner: &str, name: &str) -> Result<Option<AgentTurnPermit>, AdmissionError> {
    let waiting = async {
        loop {
            // Register before checking the count so a release cannot be lost.
            let released = RELEASED.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            match admit(config, owner, name) {
                Err(AdmissionError::Busy) => released.await,
                result => return result,
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(600), waiting).await.unwrap_or(Err(AdmissionError::Busy))
}
