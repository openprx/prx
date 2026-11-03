//! Read-only provenance for already committed v1 served-auxiliary replies.
//! This observation never grants provider or tool execution authority.
use crate::memory::MessageEvent;
use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ServedAuxiliaryProjection {
    event_id: String,
    provider_outcome: crate::llm::route_decision::ProviderExecutionOutcome,
    reply_sha256: String,
}

pub(super) fn validate_event_identity(event: &MessageEvent, terminal_id: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        event.event_type == "provider.auxiliary.settled"
            && event.role == "event"
            && event.idempotency_key.as_deref() == Some(format!("turn:{terminal_id}:automix_premium").as_str()),
        "served auxiliary event identity differs"
    );
    Ok(())
}

pub(super) fn projection_from_event(event: &MessageEvent) -> anyhow::Result<ServedAuxiliaryProjection> {
    let raw = event.raw_payload_json.as_deref().context("auxiliary payload missing")?;
    anyhow::ensure!(raw.len() <= 256 * 1024, "auxiliary payload exceeds bound");
    let value: serde_json::Value = serde_json::from_str(raw)?;
    let reply_sha256 = value
        .get("served_reply_sha256")
        .and_then(serde_json::Value::as_str)
        .context("served auxiliary response digest missing")?
        .to_owned();
    anyhow::ensure!(
        reply_sha256.len() == 64 && reply_sha256.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "served auxiliary response digest invalid"
    );
    Ok(ServedAuxiliaryProjection {
        event_id: event.event_id.clone(),
        provider_outcome: serde_json::from_value(
            value
                .get("provider_outcome")
                .context("auxiliary outcome missing")?
                .clone(),
        )?,
        reply_sha256,
    })
}
