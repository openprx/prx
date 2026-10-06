//! Reply provenance for an actual separately accounted Automix call.
//! This observation never grants provider or tool execution authority.
use super::{TurnTerminalDraft, TurnTerminalStatus};
use crate::memory::MessageEvent;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ServedAuxiliaryProjection {
    event_id: String,
    provider_outcome: crate::llm::route_decision::ProviderExecutionOutcome,
    reply_sha256: String,
}

pub(crate) struct ServedAuxiliaryLink {
    // No heap allocation exists on the ordinary no-auxiliary path.
    inner: Box<ObservedReply>,
}
struct ObservedReply {
    digest: [u8; 32],
    event: Option<MessageEvent>,
    projection: Option<ServedAuxiliaryProjection>,
}
impl ServedAuxiliaryLink {
    /// Called only after the actual owned call persists and reads back its event.
    pub(crate) fn from_call(
        call: &crate::runtime::auxiliary_call::AuxiliaryProviderCall,
        proposal: &TurnTerminalDraft,
        event: Option<MessageEvent>,
    ) -> anyhow::Result<Option<Self>> {
        let Some(digest) = call.served_reply_sha256() else {
            return Ok(None);
        };
        anyhow::ensure!(
            proposal.status == TurnTerminalStatus::Completed,
            "served auxiliary reply requires a completed terminal"
        );
        let history = proposal
            .history
            .as_ref()
            .context("served auxiliary reply has no projection")?;
        anyhow::ensure!(
            digest == <[u8; 32]>::from(Sha256::digest(history.assistant_content.as_bytes())),
            "served auxiliary reply differs from the actual response"
        );
        let decision = proposal
            .decision
            .as_ref()
            .context("served auxiliary reply has no source authority")?;
        anyhow::ensure!(
            !decision.is_recorded() || event.is_some(),
            "recorded auxiliary reply requires its durable event"
        );
        if let Some(event) = &event {
            anyhow::ensure!(
                event.id > 0
                    && event.event_type == "provider.auxiliary.settled"
                    && event.role == "event"
                    && event.owner_id == proposal.scope.owner_id
                    && event.session_key == proposal.scope.session_key
                    && event.run_id == proposal.scope.run_id
                    && event.source == proposal.scope.source
                    && event.config_generation_id == proposal.scope.config_generation_id
                    && event.config_source_revision == proposal.scope.config_source_revision,
                "served auxiliary event belongs to another turn"
            );
            validate_event(event, &history.assistant_content, &proposal.terminal_id)?;
        }
        let projection = event.as_ref().map(projection_from_event).transpose()?;
        Ok(Some(Self {
            inner: Box::new(ObservedReply {
                digest,
                event,
                projection,
            }),
        }))
    }
    pub(crate) fn projection(&self) -> Option<&ServedAuxiliaryProjection> {
        self.inner.projection.as_ref()
    }
    pub(crate) fn event(&self) -> Option<&MessageEvent> {
        self.inner.event.as_ref()
    }
    pub(crate) fn validate_reply(&self, reply: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.inner.digest == <[u8; 32]>::from(Sha256::digest(reply.as_bytes())),
            "terminal reply differs from verified auxiliary reply"
        );
        Ok(())
    }
}

pub(super) fn validate_event(event: &MessageEvent, reply: &str, terminal_id: &str) -> anyhow::Result<()> {
    validate_event_identity(event, terminal_id)?;
    let raw = event
        .raw_payload_json
        .as_deref()
        .context("auxiliary event payload missing")?;
    anyhow::ensure!(
        raw.len() <= 256 * 1024,
        "auxiliary event payload exceeds verification limit"
    );
    let value: serde_json::Value = serde_json::from_str(raw)?;
    let expected_key = format!("turn:{terminal_id}:automix_premium");
    let digest = hex::encode(Sha256::digest(reply.as_bytes()));
    anyhow::ensure!(
        value.get("version").and_then(serde_json::Value::as_u64) == Some(2)
            && value.get("purpose").and_then(serde_json::Value::as_str) == Some("automix_premium")
            && event.idempotency_key.as_deref() == Some(expected_key.as_str())
            && value.get("served_reply_sha256").and_then(serde_json::Value::as_str) == Some(digest.as_str()),
        "auxiliary served reply binding differs"
    );
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn served_reply_digest_preserves_exact_bytes_and_rejects_changed_projection() {
        let reply = "  PRX response\n";
        let link = ServedAuxiliaryLink {
            inner: Box::new(ObservedReply {
                digest: Sha256::digest(reply.as_bytes()).into(),
                event: None,
                projection: None,
            }),
        };
        assert!(link.validate_reply(reply).is_ok());
        assert!(link.validate_reply(reply.trim()).is_err());
        assert!(link.validate_reply("another response").is_err());
        assert!(
            link.event().is_none(),
            "ephemeral observation cannot claim durable authority"
        );
    }
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
