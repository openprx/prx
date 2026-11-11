//! Session address, envelope, and delivery types (design §2.1–2.2.2).

use anyhow::{Result, bail};
use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

/// Maximum Data body size: 256 KiB.
pub const MAX_BODY_BYTES: usize = 256 * 1024;

/// Logical session address. No execution-run field.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionAddr {
    /// Persisted `a2a.agent_id`.
    pub instance: String,
    /// Authenticated owner, never channel-derived.
    pub owner: String,
    /// Existing external session id.
    pub session: String,
    /// `"main"` or a named agent. Omission deserializes as `"main"`; explicit null is invalid.
    #[serde(default = "default_agent", deserialize_with = "deserialize_agent")]
    pub agent: String,
}

fn default_agent() -> String {
    "main".into()
}

fn deserialize_agent<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)?
        .ok_or_else(|| serde::de::Error::custom("explicit null agent is invalid"))
}

impl SessionAddr {
    /// Build an address, mapping an omitted agent to `"main"`.
    #[must_use]
    pub fn new(
        instance: impl Into<String>,
        owner: impl Into<String>,
        session: impl Into<String>,
        agent: Option<&str>,
    ) -> Self {
        Self {
            instance: instance.into(),
            owner: owner.into(),
            session: session.into(),
            agent: agent.unwrap_or("main").to_string(),
        }
    }
}

/// Optional live-run pin. Omitted `run` means the live execution, or queue if none.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct ExecRef {
    /// Live-run pin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
}

/// Mandatory recipient-scope pin on Data.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RecipientPin {
    /// Scope incarnation.
    pub incarnation: Uuid,
    /// Scope generation (`agent_scopes.generation`, integer ≥ 1).
    pub generation: i64,
    /// HMAC token binding scope_id + incarnation + generation + peer.
    pub token: String,
}

/// Idempotency key: 16–128 printable ASCII (0x20..=0x7E).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    /// Parse and reject non-printable or out-of-range keys.
    pub fn parse(value: &str) -> Result<Self> {
        if !(16..=128).contains(&value.len()) {
            bail!("idempotency key must be 16–128 bytes, got {}", value.len());
        }
        if !value.bytes().all(|byte| (0x20..=0x7E).contains(&byte)) {
            bail!("idempotency key rejects non-printable");
        }
        Ok(Self(value.to_string()))
    }

    /// Borrow the key bytes.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for IdempotencyKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for IdempotencyKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

/// Envelope kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Data,
    Ack,
    Nack,
}

/// Delivery ledger state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    Accepted,
    Delivered,
    Consumed,
}

/// Transport-set origin. Overwritten on ingress; never an AuthZ source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageOrigin {
    LocalOperator,
    LocalModel,
    RemotePeer,
}

/// Transport-set auth material. Absent on local messages.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MessageAuth {
    /// Verified issuer.
    pub issuer: String,
    /// Verified subject.
    pub subject: String,
    /// Key-possession method (`jwt` or `mtls`).
    pub method: String,
}

/// Envelope body. Bytes are capped at [`MAX_BODY_BYTES`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct MessageBody {
    /// MIME type (`text/plain`, `text/markdown`, …).
    #[serde(default)]
    pub mime: String,
    /// Raw body bytes.
    #[serde(default)]
    pub bytes: Vec<u8>,
}

/// One PRX message envelope (design §2.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrxMessage {
    /// Sender-minted; unique per `(to.instance, to.owner)`.
    pub id: Uuid,
    /// 16–128 printable ASCII.
    pub idempotency: IdempotencyKey,
    /// Sender-minted FIFO key. Mandatory on Data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_id: Option<Uuid>,
    /// Durable per `stream_id`; sender-allocated.
    pub seq: u64,
    /// Transport timestamp; not hashed.
    #[serde(default)]
    pub ts: i64,
    /// Overwritten on ingress.
    pub from: SessionAddr,
    /// Recipient address (no run).
    pub to: SessionAddr,
    /// Mandatory on Data.
    pub to_pin: RecipientPin,
    /// Parent message id, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<Uuid>,
    /// Wire ignored; receiver recomputes.
    #[serde(default)]
    pub hop_count: u8,
    /// Wire ignored; receiver recomputes; max 8.
    #[serde(default)]
    pub ancestry: Vec<Uuid>,
    /// Data, Ack, or Nack.
    pub kind: MessageKind,
    /// Body with size cap.
    #[serde(default)]
    pub body: MessageBody,
    /// Overwritten on ingress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<MessageOrigin>,
    /// Set by transport; absent on local.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<MessageAuth>,
    /// Optional live-run pin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec: Option<ExecRef>,
    /// Client-supplied hash; ignored; server recomputes [`super::request_digest`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_hash: Option<String>,
}

impl PrxMessage {
    /// Grammar and size checks for a new accept. Does not look up parents.
    pub fn validate(&self) -> Result<()> {
        if self.kind == MessageKind::Data && self.stream_id.is_none() {
            bail!("stream_id_required");
        }
        if self.body.bytes.len() > MAX_BODY_BYTES {
            bail!(
                "envelope body exceeds {MAX_BODY_BYTES} bytes (got {})",
                self.body.bytes.len()
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messaging::request_digest;

    fn pin() -> RecipientPin {
        RecipientPin {
            incarnation: Uuid::nil(),
            generation: 1,
            token: "token".into(),
        }
    }

    fn addr(session: &str) -> SessionAddr {
        SessionAddr::new("prx-default", "owner-a", session, None)
    }

    fn data_message(stream: Option<Uuid>, body: Vec<u8>) -> PrxMessage {
        PrxMessage {
            id: Uuid::nil(),
            idempotency: IdempotencyKey::parse("0123456789abcdef").expect("test: key"),
            stream_id: stream,
            seq: 1,
            ts: 0,
            from: addr("from"),
            to: addr("to"),
            to_pin: pin(),
            in_reply_to: None,
            hop_count: 9,
            ancestry: vec![Uuid::from_u128(1)],
            kind: MessageKind::Data,
            body: MessageBody {
                mime: "text/plain".into(),
                bytes: body,
            },
            origin: Some(MessageOrigin::LocalModel),
            auth: None,
            exec: None,
            canonical_hash: Some("deadbeef".into()),
        }
    }

    #[test]
    fn session_addr_has_no_run_field_and_roundtrips() {
        let addr = SessionAddr::new("inst", "owner", "sess", Some("agent-a"));
        let json = serde_json::to_value(&addr).expect("test: serialize");
        assert!(json.get("run").is_none(), "{json}");
        let parsed: SessionAddr = serde_json::from_value(json).expect("test: deserialize");
        assert_eq!(parsed, addr);
        let omitted: SessionAddr =
            serde_json::from_str(r#"{"instance":"i","owner":"o","session":"s"}"#).expect("test: default agent");
        assert_eq!(omitted.agent, "main");
        let null_err =
            serde_json::from_str::<SessionAddr>(r#"{"instance":"i","owner":"o","session":"s","agent":null}"#)
                .expect_err("test: explicit null");
        assert!(null_err.to_string().contains("explicit null"), "{null_err}");
    }

    #[test]
    fn envelope_rejects_oversize_body() {
        let message = data_message(Some(Uuid::nil()), vec![0; MAX_BODY_BYTES + 1]);
        let error = message.validate().expect_err("test: oversize");
        assert!(error.to_string().contains("exceeds"), "{error}");
        let ok = data_message(Some(Uuid::nil()), vec![0; MAX_BODY_BYTES]);
        ok.validate().expect("test: max body is accepted");
    }

    #[test]
    fn idempotency_key_rejects_non_printable() {
        let err = IdempotencyKey::parse("0123456789abcde\n").expect_err("test: control char");
        assert!(err.to_string().contains("non-printable"), "{err}");
        IdempotencyKey::parse("0123456789abcdef").expect("test: printable");
        let json_err = serde_json::from_str::<IdempotencyKey>("\"0123456789abcde\\u0001\"").expect_err("test: json");
        assert!(json_err.to_string().contains("non-printable"), "{json_err}");
    }

    #[test]
    fn missing_stream_id_is_rejected() {
        let message = data_message(None, b"hi".to_vec());
        let error = message.validate().expect_err("test: missing stream");
        assert!(error.to_string().contains("stream_id_required"), "{error}");
    }

    #[test]
    fn wire_canonical_hash_is_ignored_server_recomputes() {
        let message = data_message(Some(Uuid::from_u128(7)), b"body".to_vec());
        let digest = request_digest(&message);
        assert_ne!(digest, "deadbeef");
        assert_eq!(digest.len(), 64);
        let mut clone = message;
        clone.canonical_hash = Some("ffff".into());
        clone.ts = 99;
        clone.hop_count = 3;
        clone.ancestry.clear();
        clone.origin = Some(MessageOrigin::RemotePeer);
        clone.to_pin.token = "other".into();
        assert_eq!(request_digest(&clone), digest);
    }

    #[test]
    fn request_digest_includes_stream_id() {
        let a = data_message(Some(Uuid::from_u128(1)), b"body".to_vec());
        let b = data_message(Some(Uuid::from_u128(2)), b"body".to_vec());
        assert_ne!(request_digest(&a), request_digest(&b));
    }
}
