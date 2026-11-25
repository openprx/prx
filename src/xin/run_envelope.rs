//! Builds the runtime envelope a Xin task or goal step runs under.
use crate::config::Config;
use crate::runtime::envelope::RuntimeEnvelope;
use crate::xin::runner::{CONFIG_GENERATION, XinRuntimeAuthority};
use crate::xin::store;
use crate::xin::types::XinTask;
use chrono::Utc;
use std::sync::Arc;

pub(super) fn xin_task_runtime_envelope(
    config: &Config,
    task: &XinTask,
    authority: XinRuntimeAuthority,
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
    let mut envelope = RuntimeEnvelope::xin(
        config.workspace_dir.to_string_lossy().to_string(),
        task.id.clone(),
        lease_epoch,
    )
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
