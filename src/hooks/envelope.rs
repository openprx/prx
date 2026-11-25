//! The envelope every hook event is delivered in.

use std::path::Path;

use serde_json::{Value, json};

use super::event::HookEvent;
use super::identity::HookIdentity;

/// The envelope version. Raised only for a breaking change.
pub const ENVELOPE_VERSION: u32 = 1;

/// Wrap the event's own fields (`data`) with the identity of the turn or
/// session they belong to. Fields that are not known stay `null`, so a script
/// can rely on the keys; `parent` appears only for a child session and
/// `schedule` only for a scheduled run.
pub(super) fn build(event: HookEvent, workspace: &Path, identity: Option<&HookIdentity>, data: Value) -> Value {
    let text = |value: Option<&String>| value.map_or(Value::Null, |value| json!(value));
    let mut envelope = json!({
        "v": ENVELOPE_VERSION,
        "event": event.as_str(),
        "id": uuid::Uuid::new_v4().to_string(),
        "ts": chrono::Utc::now().to_rfc3339(),
        "workspace": workspace.display().to_string(),
        "owner": text(identity.and_then(|identity| identity.owner.as_ref())),
        "session": text(identity.and_then(|identity| identity.session.as_ref())),
        "run": text(identity.and_then(|identity| identity.run.as_ref())),
        "turn": text(identity.and_then(|identity| identity.turn.as_ref())),
        "round": identity.and_then(HookIdentity::round).map_or(Value::Null, |round| json!(round)),
        "agent": text(identity.and_then(|identity| identity.agent.as_ref())),
        "origin": identity.and_then(|identity| identity.origin).map_or(Value::Null, |origin| json!(origin)),
    });
    if let (Some(identity), Some(object)) = (identity, envelope.as_object_mut()) {
        if identity.parent_session.is_some() || identity.parent_run.is_some() {
            object.insert(
                "parent".to_string(),
                json!({"session": identity.parent_session, "run": identity.parent_run}),
            );
        }
        if let Some(schedule) = &identity.schedule {
            object.insert("schedule".to_string(), json!({"kind": schedule.kind, "id": schedule.id}));
        }
    }
    if let Some(object) = envelope.as_object_mut() {
        object.insert("data".to_string(), data);
    }
    envelope
}

/// The identity environment variables of a delivered envelope: the name and
/// value of each one that is known.
pub(super) fn identity_variables(envelope: &Value) -> Vec<(&'static str, String)> {
    [
        ("PRX_HOOK_SESSION", "session"),
        ("PRX_HOOK_RUN", "run"),
        ("PRX_HOOK_ORIGIN", "origin"),
    ]
    .into_iter()
    .filter_map(|(variable, key)| {
        let value = envelope.get(key)?.as_str()?;
        (!value.is_empty() && !value.contains('\0')).then(|| (variable, value.to_string()))
    })
    .collect()
}
