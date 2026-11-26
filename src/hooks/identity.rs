//! Who a hook event belongs to: the identity every hook envelope carries.
//!
//! The turn engine opens an identity scope around each turn, so every event
//! emitted from that turn's task is stamped the same way on every surface
//! (terminal, gateway, channel, scheduled work, child sessions) without each
//! emit site threading the identity through. Events emitted outside a turn
//! (a session opened, a scheduled run claimed) name their identity with
//! [`super::HookManager::emit_for`].

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use super::manager::HookManager;
use crate::runtime::envelope::{RuntimeEnvelope, RuntimeSource};

/// A scheduled run an event belongs to.
#[derive(Debug, Clone)]
pub struct HookSchedule {
    /// `cron` or `xin`.
    pub kind: &'static str,
    /// The job or task id, when the envelope carries one.
    pub id: Option<String>,
}

/// The identity fields of a hook envelope.
#[derive(Clone, Default)]
pub struct HookIdentity {
    pub owner: Option<String>,
    pub session: Option<String>,
    pub run: Option<String>,
    pub turn: Option<String>,
    pub agent: Option<String>,
    /// `terminal`, `gateway`, `channel`, `schedule` or `child`.
    pub origin: Option<&'static str>,
    pub parent_session: Option<String>,
    pub parent_run: Option<String>,
    pub schedule: Option<HookSchedule>,
    /// The model round the turn is in (0: none yet). Shared by every clone,
    /// so the engine's update is seen by events emitted from the same turn.
    round: Arc<AtomicU32>,
    /// The manager that delivers this identity's events, for code that has
    /// no manager of its own (see [`notify`]).
    manager: Option<HookManager>,
}

tokio::task_local! {
    static CURRENT: HookIdentity;
}

impl HookIdentity {
    /// The origin name of a runtime source.
    #[must_use]
    pub const fn origin_of(source: RuntimeSource) -> &'static str {
        match source {
            RuntimeSource::Chat | RuntimeSource::Agent => "terminal",
            RuntimeSource::Gateway | RuntimeSource::Console => "gateway",
            RuntimeSource::Channel => "channel",
            RuntimeSource::Cron | RuntimeSource::Xin => "schedule",
            RuntimeSource::SessionWorker | RuntimeSource::SessionsSpawn | RuntimeSource::Delegate => "child",
        }
    }

    /// The identity of one turn run under `envelope`. `turn` is the id the
    /// engine minted for this turn; a run without its own run id is named by
    /// its turn id.
    #[must_use]
    pub fn from_envelope(envelope: &RuntimeEnvelope, turn: String) -> Self {
        let schedule = match envelope.source {
            RuntimeSource::Cron => Some("cron"),
            RuntimeSource::Xin => Some("xin"),
            _ => None,
        }
        .map(|kind| HookSchedule {
            kind,
            id: envelope.task_id.clone(),
        });
        Self {
            owner: Some(envelope.resolved_owner_id()),
            session: Some(envelope.session_key.clone()),
            run: Some(envelope.run_id.clone().unwrap_or_else(|| turn.clone())),
            turn: Some(turn),
            agent: envelope.agent_id.clone(),
            origin: Some(Self::origin_of(envelope.source)),
            parent_session: envelope.parent_session_key.clone(),
            parent_run: envelope.parent_run_id.clone(),
            schedule,
            round: Arc::new(AtomicU32::new(0)),
            manager: None,
        }
    }

    /// This identity with `manager` as the one that delivers its events.
    #[must_use]
    pub fn with_manager(mut self, manager: HookManager) -> Self {
        self.manager = Some(manager);
        self
    }

    /// The identity of one run of a scheduled job or task.
    #[must_use]
    pub fn for_schedule(kind: &'static str, id: &str, run: String) -> Self {
        Self {
            run: Some(run),
            origin: Some("schedule"),
            schedule: Some(HookSchedule {
                kind,
                id: Some(id.to_string()),
            }),
            ..Self::default()
        }
    }

    /// The identity of a session named by its key alone.
    #[must_use]
    pub fn for_session_key(session: String, origin: &'static str) -> Self {
        Self {
            session: Some(session),
            origin: Some(origin),
            ..Self::default()
        }
    }

    /// The identity of a turn that runs without a runtime envelope: it is
    /// named by its turn id alone.
    #[must_use]
    pub fn bare_turn(turn: String) -> Self {
        Self {
            run: Some(turn.clone()),
            turn: Some(turn),
            ..Self::default()
        }
    }

    /// The identity of an event that belongs to a session, not to a turn.
    #[must_use]
    pub fn for_session(envelope: &RuntimeEnvelope) -> Self {
        Self {
            owner: Some(envelope.resolved_owner_id()),
            session: Some(envelope.session_key.clone()),
            run: envelope.run_id.clone(),
            origin: Some(Self::origin_of(envelope.source)),
            agent: envelope.agent_id.clone(),
            parent_session: envelope.parent_session_key.clone(),
            parent_run: envelope.parent_run_id.clone(),
            ..Self::default()
        }
    }

    /// The current model round, when a turn is in one.
    pub fn round(&self) -> Option<u32> {
        Some(self.round.load(Ordering::Relaxed)).filter(|round| *round > 0)
    }
}

/// Run `future` with `identity` as the identity of every hook event emitted
/// from its task.
pub async fn scope<F: Future>(identity: HookIdentity, future: F) -> F::Output {
    CURRENT.scope(identity, future).await
}

/// The identity of the turn the calling task runs, if any.
#[must_use]
pub fn current() -> Option<HookIdentity> {
    CURRENT.try_with(Clone::clone).ok()
}

/// Record the model round the current turn entered.
pub fn set_round(round: usize) {
    // A failed access means no turn scope is open: nothing to update.
    let _ = CURRENT.try_with(|identity| {
        identity
            .round
            .store(u32::try_from(round).unwrap_or(u32::MAX), Ordering::Relaxed);
    });
}

/// Queue `event` through the manager of the turn the calling task runs, with
/// that turn's identity. Does nothing outside a turn scope. The event is
/// queued and never awaited.
pub fn notify(event: super::event::HookEvent, data: serde_json::Value) {
    if let Some(identity) = current()
        && let Some(manager) = identity.manager.as_ref()
    {
        // `emit_as` queues synchronously; the returned future is already ready.
        drop(manager.emit_as(event, data, Some(&identity)));
    }
}

/// The manager of the turn the calling task runs, if any: a clone of the one
/// the turn was scoped with (see [`HookIdentity::with_manager`]).
#[must_use]
pub fn current_manager() -> Option<HookManager> {
    CURRENT.try_with(|identity| identity.manager.clone()).ok().flatten()
}

/// [`scope`] when there is an identity to install, else `future` as it is.
pub async fn scope_optional<F: Future>(identity: Option<HookIdentity>, future: F) -> F::Output {
    match identity {
        Some(identity) => scope(identity, future).await,
        None => future.await,
    }
}
