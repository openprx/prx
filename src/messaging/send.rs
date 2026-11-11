//! Dormant sender port. Resolvers are runtime capabilities, never model JSON.
use super::mailbox::{MailboxAuthority, MailboxError, MailboxPublished, MailboxResult, validate_address};
use super::{RecipientPin, SessionAddr};
use async_trait::async_trait;
use serde_json::{Value, json};

/// A runtime-authorized destination and its freshly resolved recipient pin.
pub struct MailboxDestination {
    pub authority: MailboxAuthority,
    pub pin: RecipientPin,
    pub pin_expires_at: i64,
}

#[async_trait]
pub trait MailboxResolver: Send + Sync {
    async fn resolve(&self, target: &SessionAddr) -> MailboxResult<MailboxDestination>;
}

/// Sender identity and inherited provenance are sealed in the implementation.
#[async_trait]
pub trait MailboxSender: Send + Sync {
    fn source(&self) -> &SessionAddr;
    async fn send(&self, target: SessionAddr, text: String, mime: String) -> MailboxResult<MailboxPublished>;
}

/// Apply defaults only to absent keys; explicit null and unknown keys fail.
pub fn parse_target(value: &Value, caller: &SessionAddr) -> MailboxResult<SessionAddr> {
    let object = value.as_object().ok_or(MailboxError::InvalidAddress)?;
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "instance" | "owner" | "session" | "agent"))
    {
        return Err(MailboxError::InvalidAddress);
    }
    let field = |key: &str, default: Option<&str>| -> MailboxResult<String> {
        match object.get(key) {
            Some(Value::String(value)) => Ok(value.clone()),
            None => default.map(str::to_owned).ok_or(MailboxError::InvalidAddress),
            _ => Err(MailboxError::InvalidAddress),
        }
    };
    let target = SessionAddr {
        instance: field("instance", Some(&caller.instance))?,
        owner: field("owner", Some(&caller.owner))?,
        session: field("session", None)?,
        agent: field("agent", Some("main"))?,
    };
    validate_address(&target)?;
    Ok(target)
}

pub fn target_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"properties":{
        "instance":{"type":"string"},"owner":{"type":"string"},
        "session":{"type":"string","minLength":1},"agent":{"type":"string"}},"required":["session"]})
}

pub fn send_result(result: MailboxResult<MailboxPublished>) -> Value {
    use super::mailbox::MailboxSendState;
    match result {
        Ok(sent) => {
            json!({"status":match sent.state {MailboxSendState::Failed=>"failed",MailboxSendState::Acked=>"accepted",_=>"queued"},
            "message_id":sent.message_id,"state":sent.ack_state.unwrap_or(super::DeliveryState::Accepted),"error":sent.error})
        }
        Err(error) => json!({"status":"failed","message_id":null,"state":null,"error":error.to_string()}),
    }
}
