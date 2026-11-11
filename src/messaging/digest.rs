//! Server-computed request digest (design §2.2.2).

use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::types::{MessageKind, PrxMessage};

/// SHA-256 hex of the immutable request fields after identity overwrite,
/// without hop/ancestry and without parent lookup.
///
/// Canonical JSON is serde_json with sorted keys and no whitespace (C3).
#[must_use]
pub fn request_digest(message: &PrxMessage) -> String {
    let payload = digest_payload(message);
    let mut hasher = Sha256::new();
    hasher.update(payload.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn digest_payload(message: &PrxMessage) -> String {
    let mut map = serde_json::Map::new();
    insert(&mut map, "id", serde_json::json!(message.id));
    insert(&mut map, "idempotency", serde_json::json!(message.idempotency.as_str()));
    insert(&mut map, "stream_id", opt_uuid(message.stream_id));
    insert(&mut map, "seq", serde_json::json!(message.seq));
    insert(&mut map, "from.instance", serde_json::json!(message.from.instance));
    insert(&mut map, "from.owner", serde_json::json!(message.from.owner));
    insert(&mut map, "from.session", serde_json::json!(message.from.session));
    insert(&mut map, "from.agent", serde_json::json!(message.from.agent));
    insert(&mut map, "to.instance", serde_json::json!(message.to.instance));
    insert(&mut map, "to.owner", serde_json::json!(message.to.owner));
    insert(&mut map, "to.session", serde_json::json!(message.to.session));
    insert(&mut map, "to.agent", serde_json::json!(message.to.agent));
    insert(
        &mut map,
        "to_pin.incarnation",
        serde_json::json!(message.to_pin.incarnation),
    );
    insert(
        &mut map,
        "to_pin.generation",
        serde_json::json!(message.to_pin.generation),
    );
    insert(
        &mut map,
        "exec.run",
        message
            .exec
            .as_ref()
            .and_then(|exec| exec.run.as_deref())
            .map_or(serde_json::Value::Null, |run| serde_json::json!(run)),
    );
    insert(
        &mut map,
        "kind",
        serde_json::json!(match message.kind {
            MessageKind::Data => "data",
            MessageKind::Ack => "ack",
            MessageKind::Nack => "nack",
        }),
    );
    insert(
        &mut map,
        "body",
        serde_json::json!({
            "mime": message.body.mime,
            "bytes": message.body.bytes,
        }),
    );
    insert(&mut map, "in_reply_to", opt_uuid(message.in_reply_to));
    serde_json::Value::Object(map).to_string()
}

fn insert(map: &mut serde_json::Map<String, serde_json::Value>, key: &str, value: serde_json::Value) {
    map.insert(key.to_string(), value);
}

fn opt_uuid(value: Option<Uuid>) -> serde_json::Value {
    value.map_or(serde_json::Value::Null, |id| serde_json::json!(id))
}
