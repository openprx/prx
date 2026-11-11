//! Runtime-owned causal chain and inherited remote provenance (design §4.3.1).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::types::PrxMessage;

/// Turn input provenance, orthogonal to `DispatchOrigin`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputProvenance {
    LocalOperator,
    LocalModel,
    RemotePeer {
        issuer: String,
        subject: String,
        message_id: Uuid,
    },
}

/// Remote attribution carried with a chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemotePeerAttr {
    /// Verified issuer.
    pub issuer: String,
    /// Verified subject.
    pub subject: String,
    /// Triggering remote message id.
    pub message_id: Uuid,
}

/// Runtime-owned chain derived from receiver-recomputed hop/ancestry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CausalChain {
    /// Message that started this turn.
    pub trigger: Uuid,
    /// Root of the chain.
    pub root: Uuid,
    /// Receiver-recomputed depth.
    pub depth: u8,
    /// Receiver-recomputed ancestry.
    pub ancestry: Vec<Uuid>,
}

/// Sealed into child task input, worker manifest, relaunch recipe, and jobs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InheritedProvenance {
    /// Remote attribution. Never promoted to `LocalOperator`.
    pub remote: RemotePeerAttr,
    /// Inherited chain.
    pub chain: CausalChain,
}

/// In-process task input that can carry a sealed chain (FOUNDATION, dormant).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskInput {
    /// Sealed remote provenance, if this task was created by a chained turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherited_provenance: Option<InheritedProvenance>,
}

impl InheritedProvenance {
    /// Seal this provenance into task input.
    pub fn seal_into_task_input(&self, input: &mut TaskInput) {
        input.inherited_provenance = Some(self.clone());
    }
}

/// Stamp outbound Data with `in_reply_to = chain.trigger`. A conflicting model
/// value is ignored.
pub fn stamp_outbound(chain: &CausalChain, message: &mut PrxMessage) {
    if message.kind != super::types::MessageKind::Data {
        return;
    }
    if message.in_reply_to != Some(chain.trigger) {
        tracing::warn!(
            trigger = %chain.trigger,
            model_in_reply_to = ?message.in_reply_to,
            "stamp_outbound overrode model in_reply_to"
        );
    }
    message.in_reply_to = Some(chain.trigger);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messaging::types::{IdempotencyKey, MessageBody, MessageKind, PrxMessage, RecipientPin, SessionAddr};

    fn chain() -> CausalChain {
        let trigger = Uuid::from_u128(42);
        CausalChain {
            trigger,
            root: trigger,
            depth: 1,
            ancestry: Vec::new(),
        }
    }

    #[test]
    fn stamp_outbound_overrides_model_in_reply_to() {
        let chain = chain();
        let model_parent = Uuid::from_u128(7);
        let mut message = PrxMessage {
            id: Uuid::nil(),
            idempotency: IdempotencyKey::parse("0123456789abcdef").expect("test: key"),
            stream_id: Some(Uuid::nil()),
            seq: 1,
            ts: 0,
            from: SessionAddr::new("i", "o", "s", None),
            to: SessionAddr::new("i", "o", "t", None),
            to_pin: RecipientPin {
                incarnation: Uuid::nil(),
                generation: 1,
                token: "t".into(),
            },
            in_reply_to: Some(model_parent),
            hop_count: 0,
            ancestry: Vec::new(),
            kind: MessageKind::Data,
            body: MessageBody::default(),
            origin: None,
            auth: None,
            exec: None,
            canonical_hash: None,
        };
        stamp_outbound(&chain, &mut message);
        assert_eq!(message.in_reply_to, Some(chain.trigger));
        assert_ne!(message.in_reply_to, Some(model_parent));
        for kind in [MessageKind::Ack, MessageKind::Nack] {
            message.kind = kind;
            message.in_reply_to = Some(model_parent);
            stamp_outbound(&chain, &mut message);
            assert_eq!(message.in_reply_to, Some(model_parent));
        }
    }

    #[tokio::test]
    async fn inherited_provenance_seals_into_task_input() {
        let provenance = InheritedProvenance {
            remote: RemotePeerAttr {
                issuer: "spiffe://prx".into(),
                subject: "spiffe://prx/peer".into(),
                message_id: Uuid::from_u128(42),
            },
            chain: chain(),
        };
        let mut context = crate::tools::sessions_spawn::SpawnExecutionContext::seed_turn_context(
            "prx-run".into(),
            "prx-session".into(),
        );
        provenance.seal_into_task_input(&mut context.task_input);
        crate::tools::sessions_spawn::SPAWN_EXECUTION_CONTEXT
            .scope(context, async {
                let actual =
                    crate::tools::sessions_spawn::SPAWN_EXECUTION_CONTEXT.with(|context| context.task_input.clone());
                assert_eq!(actual.inherited_provenance.as_ref(), Some(&provenance));
            })
            .await;
    }
}
