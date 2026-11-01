//! Turn hooks the ramflux channel installs on a real prx turn.
//!
//! Persona, attachment fetch, the final surface, and message deletion all
//! live here so the channel module stays a session object.

use super::Inner;
use crate::channels::rich::{RichReply, SurfaceBlock, SurfaceOp, SurfaceReply};
use crate::channels::traits::{ChannelMessage, ChatKind};

/// Surface action and task event projected onto the existing channel message.
pub(super) fn inbound_as_message(
    id: &str,
    sender: &str,
    conversation_id: &str,
    content: &str,
    display: Option<String>,
) -> ChannelMessage {
    ChannelMessage {
        id: id.to_string(),
        sender: sender.to_string(),
        reply_target: conversation_id.to_string(),
        content: content.to_string(),
        channel: "ramflux".to_string(),
        timestamp: super::socket::unix_ms(),
        chat_kind: ChatKind::Dm,
        chat_title: None,
        sender_display: display,
        thread_ts: None,
        mentioned_uuids: Vec::new(),
        mentioned: false,
        is_group_hint: false,
        sender_is_bot: false,
    }
}

/// Fetch structured attachments for `message_id` and append multimodal parts.
///
/// Ids come from the inbound attachment list, never from free text. Image
/// bytes become an `[IMAGE:data:…]` part. Text-like bytes become a named text
/// part. Anything else is a typed stub with size and sha256.
pub(super) async fn enrich_inbound(channel: &super::RamfluxChannel, content: &str, message_id: &str) -> String {
    let attachments = channel.take_attachments(message_id);
    if attachments.is_empty() {
        return content.to_string();
    }
    let limits = &channel.config().attachment_fetch;
    let mut parts = vec![content.to_string()];
    let mut fetched = 0u8;
    for attachment in attachments {
        if fetched >= limits.max_per_turn {
            parts.push(format!("[attachment {} skipped: turn limit]", attachment.object_id));
            continue;
        }
        fetched = fetched.saturating_add(1);
        match channel.fetch_attachment(&attachment.object_id, limits.max_bytes).await {
            Ok(bytes) => {
                channel.note_media(message_id, &attachment.object_id);
                parts.push(render_attachment(&attachment, &bytes));
            }
            Err(error) => {
                tracing::debug!(
                    error = %error,
                    object_len = attachment.object_id.len(),
                    "ramflux attachment was not fetched"
                );
                parts.push(format!("[attachment {} was not fetched]", attachment.object_id));
            }
        }
    }
    parts.join("\n")
}

fn render_attachment(attachment: &crate::channels::rich::AttachmentRef, bytes: &[u8]) -> String {
    let name = attachment.name.as_deref().unwrap_or(attachment.object_id.as_str());
    let mime = attachment.mime.as_str();
    if mime.starts_with("image/") {
        let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes);
        return format!("[IMAGE:data:{mime};base64,{encoded}]");
    }
    if is_text_mime(mime) {
        let text = String::from_utf8_lossy(bytes);
        return format!("[attachment {name}]\n{text}");
    }
    let digest = sha256_hex(bytes);
    format!("[attachment {name} type={mime} size={} sha256={digest}]", bytes.len())
}

fn is_text_mime(mime: &str) -> bool {
    mime.starts_with("text/")
        || mime == "application/json"
        || mime == "application/pdf"
        || mime.ends_with("+json")
        || mime.ends_with("/markdown")
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Best-effort markdown surface. An invalid surface is skipped.
///
/// # Errors
///
/// Returns an error only when a valid surface cannot be written.
pub(super) async fn project_final_surface(
    inner: &Inner,
    conversation_id: &str,
    turn_id: &str,
    text: &str,
) -> anyhow::Result<()> {
    if text.trim().is_empty() {
        return Ok(());
    }
    let surface = SurfaceReply {
        conversation_id: conversation_id.to_string(),
        turn_id: turn_id.to_string(),
        op: SurfaceOp::Create,
        surface_id: format!("final-{turn_id}"),
        version: 1,
        base_hash: None,
        blocks: Some(vec![SurfaceBlock {
            id: "body".to_string(),
            kind: "Markdown".to_string(),
            data: serde_json::json!({"text": text}),
        }]),
        actions: None,
        ops: None,
        block_id: None,
        text_delta: None,
        is_final: Some(true),
        fallback_text: Some(text.to_string()),
    };
    if super::surface_schema::validate(
        &surface,
        &inner.config.surface_limits,
        &inner.config.allowed_block_types,
    )
    .is_err()
    {
        tracing::debug!("ramflux final surface skipped");
        return Ok(());
    }
    super::outbound::send_reply(inner, &RichReply::Surface(surface)).await
}

/// Erase files and scoped memory rows for one message. Message-event rows are
/// deleted by the eraser task, not here.
pub(super) async fn erase_local(
    inner: &Inner,
    account_id: &str,
    conversation_id: &str,
    message_id: &str,
) -> Result<(), String> {
    let persona = inner.persona.lock().clone();
    let scope = super::accounts::memory_scope(persona.as_ref(), account_id, conversation_id);
    let entry = super::erase_index::load(&inner.store, &inner.crypto, &account_id, conversation_id, message_id);
    super::erase::erase_message(&inner.store, &account_id, &scope, message_id).map_err(|error| error.to_string())?;
    if let Some(entry) = entry {
        for object_id in &entry.media_ids {
            super::erase::erase_media(&inner.store, &account_id, conversation_id, object_id)
                .map_err(|error| error.to_string())?;
        }
        prune_history(inner, &entry.history_keys);
    }
    super::erase_index::remove(&inner.store, &account_id, conversation_id, message_id)
        .map_err(|error| error.to_string())?;
    if let Some(tracked) = inner.take_tracked(message_id) {
        for object_id in &tracked.objects {
            super::erase::erase_media(&inner.store, &account_id, conversation_id, object_id)
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

/// Erase every indexed message in one conversation, plus its media and rows.
pub(super) async fn erase_conversation_scope(inner: &Inner, conversation_id: &str) -> Result<(), String> {
    let account_id = inner.account_id.lock().clone().unwrap_or_default();
    let entries = super::erase_index::entries(&inner.store, &inner.crypto, &account_id, conversation_id);
    for entry in &entries {
        let account_id = inner.account_id.lock().clone().unwrap_or_default();
        erase_local(inner, &account_id, conversation_id, &entry.message_id).await?;
    }
    let persona = inner.persona.lock().clone();
    let scope = super::accounts::memory_scope(persona.as_ref(), &account_id, conversation_id);
    super::erase::erase_conversation(&inner.store, &account_id, &scope).map_err(|error| error.to_string())?;
    super::erase::erase_conversation(&inner.store, &account_id, conversation_id).map_err(|error| error.to_string())?;
    let memory = inner.attached_memory.lock().clone();
    if let Some(memory) = memory {
        let scoped = inner.scoped_memory(memory, &account_id, conversation_id);
        let _ = scoped.forget_key_prefix("").await;
    }
    Ok(())
}

fn prune_history(inner: &Inner, keys: &[String]) {
    if keys.is_empty() {
        return;
    }
    let Some(histories) = inner.histories.lock().clone() else {
        return;
    };
    let mut histories = histories.lock();
    let mut index = inner.history_keys.lock();
    for (session, turns) in histories.iter_mut() {
        let recorded = index.entry(session.clone()).or_default();
        let mut kept_turns = Vec::new();
        let mut kept_keys = Vec::new();
        for (offset, turn) in turns.drain(..).enumerate() {
            let key = recorded.get(offset).cloned().unwrap_or_default();
            if keys.iter().any(|wanted| wanted == &key) {
                continue;
            }
            kept_keys.push(key);
            kept_turns.push(turn);
        }
        *recorded = kept_keys;
        *turns = kept_turns;
    }
}
