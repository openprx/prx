//! The one place an autonomy level is decided.
//!
//! Every reader of a turn's, a call's or a scheduled run's level asks these
//! pure functions. A level that is unset or unreadable is never read as
//! `Full`: a stored creator with an unreadable spelling is capped at
//! `Supervised`. A child runs at its parent's level unless it is a named
//! agent, which runs at its own configured level ([`named_child_level`]).
use crate::config::Config;
use crate::security::AutonomyLevel;

/// The `creator_agent` value of work Main created.
pub const CREATOR_MAIN: &str = "main";

/// A named agent's or Main's own turn: the agent's configured level, or
/// Main's when the agent sets none. No `min` with Main: a named agent
/// configured by the owner is a top-level agent with its own level.
#[must_use]
pub fn agent_level(own: Option<AutonomyLevel>, main: AutonomyLevel) -> AutonomyLevel {
    own.unwrap_or(main)
}

/// The level a named-agent child runs at: the agent's configured level, or
/// Main's when it sets none. `None` when `agent` names no configured agent;
/// such a child runs at its parent's level.
#[must_use]
pub fn named_child_level(config: &Config, agent: Option<&str>) -> Option<AutonomyLevel> {
    agent
        .and_then(|name| config.agents.get(name))
        .map(|configured| agent_level(configured.autonomy, config.autonomy.level))
}

/// The level a tool-internal check reads: the call's level when the
/// execution service set one, else the level of the policy the tool holds.
#[must_use]
pub fn call_level(call: Option<AutonomyLevel>, held: AutonomyLevel) -> AutonomyLevel {
    call.unwrap_or(held)
}

/// The stored spelling of a level in a `creator_level` column.
#[must_use]
pub const fn stored_level(level: AutonomyLevel) -> &'static str {
    match level {
        AutonomyLevel::ReadOnly => "readonly",
        AutonomyLevel::Supervised => "supervised",
        AutonomyLevel::Full => "full",
    }
}

fn parse_stored_level(raw: Option<&str>) -> Option<AutonomyLevel> {
    match raw {
        Some("readonly") => Some(AutonomyLevel::ReadOnly),
        Some("supervised") => Some(AutonomyLevel::Supervised),
        Some("full") => Some(AutonomyLevel::Full),
        _ => None,
    }
}

/// The `creator_agent` value of work a named agent's incarnation created.
#[must_use]
pub fn creator_agent_named(name: &str, incarnation: &uuid::Uuid) -> String {
    format!("n:{name}:{incarnation}")
}

/// Who created a scheduled row and at what level (`creator_level`,
/// `creator_agent`). Both are `None` on a row stored without them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CreatorStamp {
    pub level: Option<AutonomyLevel>,
    pub agent: Option<String>,
}

impl CreatorStamp {
    /// Read the two stored columns; an unreadable level reads as `None`.
    #[must_use]
    pub fn from_stored(level: Option<&str>, agent: Option<String>) -> Self {
        Self {
            level: parse_stored_level(level),
            agent,
        }
    }

    /// The `creator_level` column value.
    #[must_use]
    pub fn level_column(&self) -> Option<&'static str> {
        self.level.map(stored_level)
    }
}

/// A scheduled run that cannot be given a level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ScheduledLevelError {
    /// The named agent recorded as the creator is no longer configured, or
    /// is retired.
    #[error("creator_gone: the agent that created this work no longer exists, so nothing ran")]
    CreatorGone,
}

/// Why a scheduled agent run does not start at its level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ScheduledRefusal {
    /// The run's level is read-only.
    #[error("autonomy is read-only")]
    ReadOnly,
    /// The job's origin keeps the local operator's context, whose policy is
    /// Main's, so a run whose level is below Main's cannot start there.
    #[error(
        "below_operator_context: this job runs in the local operator's context with Main's policy, above its own \
         level, so nothing ran"
    )]
    BelowOperatorContext,
}

/// The refusal for a scheduled agent run at `level`, if any.
/// `operator_context` says the run keeps the local operator's context and
/// Main's (`main`) policy (a `LocalOperator` origin).
#[must_use]
pub fn scheduled_agent_refusal(
    level: AutonomyLevel,
    operator_context: bool,
    main: AutonomyLevel,
) -> Option<ScheduledRefusal> {
    if level == AutonomyLevel::ReadOnly {
        Some(ScheduledRefusal::ReadOnly)
    } else if operator_context && level < main {
        Some(ScheduledRefusal::BelowOperatorContext)
    } else {
        None
    }
}

/// The level a scheduled run (cron job, Xin task or goal step) runs at.
///
/// - Work the local operator created keeps Main's live level.
/// - Otherwise the stored creator level, lowered to the creator's current
///   level. A row with no recorded creator runs at Main's level; a creator
///   with an unreadable spelling is capped at `Supervised`. A named creator
///   that is gone fails the run.
/// - A run started from a tool call is lowered to that call's level.
///
/// # Errors
/// [`ScheduledLevelError::CreatorGone`] when the named creator is gone.
pub fn scheduled_level(
    operator: bool,
    creator: &CreatorStamp,
    config: &Config,
    caller: Option<AutonomyLevel>,
) -> Result<AutonomyLevel, ScheduledLevelError> {
    let main = config.autonomy.level;
    let base = if operator {
        main
    } else {
        match (creator.level, creator.agent.as_deref()) {
            (Some(stored), Some(agent)) => match creator_current_level(agent, config) {
                Some(current) => stored.min(current?),
                None => AutonomyLevel::Supervised.min(main),
            },
            _ => main,
        }
    };
    Ok(caller.map_or(base, |caller| base.min(caller)))
}

/// `None` when `agent` is not a creator spelling (the stamp is unreadable).
fn creator_current_level(agent: &str, config: &Config) -> Option<Result<AutonomyLevel, ScheduledLevelError>> {
    if agent == CREATOR_MAIN {
        return Some(Ok(config.autonomy.level));
    }
    let (name, _incarnation) = agent.strip_prefix("n:")?.split_once(':')?;
    Some(
        config
            .agents
            .get(name)
            .filter(|configured| configured.enabled)
            .map(|configured| agent_level(configured.autonomy, config.autonomy.level))
            .ok_or(ScheduledLevelError::CreatorGone),
    )
}
