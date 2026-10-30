//! Verify the actual serialized logical slot without rebuilding transport bytes.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub(super) struct FinalContext {
    pub(super) sha256: String,
}

pub(super) fn verify(
    body: &[u8],
    model: &str,
    block: &str,
    expected_slot_bytes: u64,
    expected_slot_sha256: &str,
) -> Result<FinalContext> {
    ensure!(
        !block.is_empty() && block.is_ascii(),
        "lesson block is not bounded ASCII context"
    );
    let request: Value = serde_json::from_slice(body).context("lesson request format is unsupported")?;
    ensure!(
        request.get("model").and_then(Value::as_str) == Some(model),
        "lesson request model changed"
    );
    // Only the two existing text-only managed-provider formats are admitted.
    // Mixed/unknown formats fail closed rather than guessing another slot.
    let messages = match (request.get("messages"), request.get("input")) {
        (Some(messages), None) | (None, Some(messages)) => messages
            .as_array()
            .context("lesson request message list is unsupported")?,
        _ => anyhow::bail!("lesson request message layout is unsupported"),
    };
    let mut users = messages
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("user"));
    let user = users.next().context("lesson request has no user slot")?;
    ensure!(users.next().is_none(), "lesson request has additional user slots");
    let slot = user
        .get("content")
        .and_then(Value::as_str)
        .context("lesson user slot is not plain text")?;
    let sha256 = hex::encode(Sha256::digest(slot.as_bytes()));
    ensure!(
        u64::try_from(slot.len())? == expected_slot_bytes && sha256 == expected_slot_sha256,
        "lesson final user slot differs from its prepared pin"
    );
    ensure!(
        slot.matches(block).count() == 1 && occurrences(&request, block) == 1,
        "lesson block is missing, repeated or in another logical slot"
    );
    Ok(FinalContext { sha256 })
}

fn occurrences(value: &Value, block: &str) -> usize {
    match value {
        Value::String(text) => text.matches(block).take(2).count(),
        Value::Array(items) => items
            .iter()
            .map(|item| occurrences(item, block))
            .fold(0_usize, |sum, next| sum.saturating_add(next).min(2)),
        Value::Object(fields) => fields
            .iter()
            .map(|(key, value)| {
                key.matches(block)
                    .take(2)
                    .count()
                    .saturating_add(occurrences(value, block))
            })
            .fold(0_usize, |sum, next| sum.saturating_add(next).min(2)),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: &str = "LESSON fixture: preserve the verified parent. END LESSON";

    #[test]
    fn final_wire_checks_the_complete_decoded_slot_and_unique_block() -> Result<()> {
        let slot = format!("Editor input with quoted \"text\".\n{BLOCK}");
        let digest = hex::encode(Sha256::digest(slot.as_bytes()));
        let slot_bytes = u64::try_from(slot.len())?;
        for format in ["messages", "input"] {
            let mut body = serde_json::json!({"model":"fixture-model"});
            body.as_object_mut().context("fixture body is not an object")?.insert(
                format.into(),
                serde_json::json!([
                    {"role":"system","content":"Edit instructions only."},
                    {"role":"user","content":slot}
                ]),
            );
            let bytes = serde_json::to_vec(&body)?;
            assert_eq!(
                verify(&bytes, "fixture-model", BLOCK, slot_bytes, &digest)?.sha256,
                digest
            );
            assert!(verify(&bytes, "other-model", BLOCK, slot_bytes, &digest).is_err());
            assert!(verify(&bytes, "fixture-model", "missing block", slot_bytes, &digest).is_err());
        }
        for wrong in [
            serde_json::json!({"model":"fixture-model","messages":[{"role":"user","content":BLOCK}]}),
            serde_json::json!({"model":"fixture-model","messages":[{"role":"user","content":slot}],"extra":BLOCK}),
            serde_json::json!({"model":"fixture-model","messages":[{"role":"user","content":slot},{"role":"user","content":"extra"}]}),
            serde_json::json!({"model":"fixture-model","messages":[{"role":"user","content":[{"type":"text","text":slot}]}]}),
            serde_json::json!({"model":"fixture-model","messages":[{"role":"user","content":slot}],"input":[]}),
        ] {
            assert!(
                verify(
                    &serde_json::to_vec(&wrong)?,
                    "fixture-model",
                    BLOCK,
                    slot_bytes,
                    &digest
                )
                .is_err()
            );
        }
        Ok(())
    }
}
