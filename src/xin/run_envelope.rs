//! Builds the scheduled origin and runtime envelope a Xin task or goal step runs under.
use crate::agent::scheduled_source::ScheduledAgentOrigin;
use crate::config::Config;
use crate::runtime::envelope::RuntimeEnvelope;
use crate::xin::runner::{CONFIG_GENERATION, XinRuntimeAuthority};
use crate::xin::store;
use crate::xin::types::{XinGoal, XinStep, XinTask};
use chrono::Utc;
use std::sync::Arc;

/// The scheduled origin of a Xin run, with its task card. A goal step carries its
/// goal, so its session is the goal's shared session; a plain task does not.
pub(super) fn scheduled_origin(
    config: &Config,
    task: &XinTask,
    step: Option<(&XinStep, &XinGoal)>,
) -> ScheduledAgentOrigin {
    let (origin, card) = match step {
        Some((step, goal)) => (
            ScheduledAgentOrigin::from_xin_goal_step(task, &step.goal_id),
            crate::agent::scheduled_card::for_xin_step(config, step, goal),
        ),
        None => (
            ScheduledAgentOrigin::from_xin(task),
            crate::agent::scheduled_card::for_xin_task(task),
        ),
    };
    origin.with_card(card)
}

/// `goal_id` is the goal of the step `task` is, or `None` for a plain task.
pub(super) fn xin_task_runtime_envelope(
    config: &Config,
    task: &XinTask,
    authority: XinRuntimeAuthority,
    goal_id: Option<&str>,
) -> RuntimeEnvelope {
    let lease_epoch = authority.epoch();
    let guard_config = config.clone();
    let guard_task_id = task.id.clone();
    let authority_guard =
        crate::memory::RuntimeAuthorityGuard::new(format!("xin:{}:{lease_epoch}", task.id), move || match &authority {
            XinRuntimeAuthority::Legacy(lease) => {
                store::task_lease_is_current(&guard_config, &guard_task_id, lease, Utc::now())
            }
            XinRuntimeAuthority::Step(lease) => {
                store::step_lease_is_current(&guard_config, &guard_task_id, lease, Utc::now())
            }
        });
    let workspace = config.workspace_dir.to_string_lossy().to_string();
    let base = match goal_id {
        Some(goal_id) => RuntimeEnvelope::xin_goal_step(workspace, goal_id, task.id.clone(), lease_epoch),
        None => RuntimeEnvelope::xin(workspace, task.id.clone(), lease_epoch),
    };
    let mut envelope = base
        .with_authority_guard(authority_guard)
        .with_recipient(format!("xin:goal:task:{}", task.id))
        .with_task_id(task.id.clone());
    if let Some(owner_id) = task.owner_id.as_deref() {
        envelope = envelope.with_owner_id(owner_id);
    }
    if let Some(topic_id) = task.topic_id.as_deref() {
        envelope = envelope.with_topic_id(topic_id);
    }
    if let Some(source_message_event_id) = task.source_message_event_id.as_deref() {
        envelope = envelope.with_source_message_event_id(source_message_event_id);
    }
    if let Ok(generation) = CONFIG_GENERATION.try_with(Arc::clone) {
        envelope = envelope.with_config_generation(&generation);
    }
    if task.origin.is_owner_isolated() {
        envelope = envelope.with_owner_isolation();
    }
    envelope
}
