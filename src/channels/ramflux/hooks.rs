//! Turn hooks the ramflux channel installs on a real prx turn.
//!
//! Persona, attachment fetch, the final surface, and message deletion all
//! live here so the channel module stays a session object.

use super::Inner;
use crate::channels::rich::{RichReply, SurfaceBlock, SurfaceOp, SurfaceReply};
use crate::channels::traits::{ChannelMessage, ChatKind};
use crate::providers::ChatMessage;

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

/// Fetch attachment ids named in `content`, under the configured limits.
///
/// The fetched bytes are stored and not logged.
pub(super) async fn enrich_inbound(channel: &super::RamfluxChannel, content: &str) -> String {
    let limits = &channel.config().attachment_fetch;
    let mut notes = Vec::new();
    let mut fetched = 0u8;
    for object_id in attachment_ids(content) {
        if fetched >= limits.max_per_turn {
            notes.push(format!("attachment {object_id} skipped: turn limit"));
            continue;
        }
        fetched = fetched.saturating_add(1);
        match channel.fetch_attachment(&object_id, limits.max_bytes).await {
            Ok(bytes) => {
                let len = bytes.len();
                notes.push(format!("attachment {object_id} fetched ({len} bytes)"));
            }
            Err(error) => {
                tracing::debug!(error = %error, "ramflux attachment was not fetched");
                notes.push(format!("attachment {object_id} was not fetched"));
            }
        }
    }
    if notes.is_empty() {
        content.to_string()
    } else {
        format!("{content}\n{}", notes.join("\n"))
    }
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

/// Remove one message from the channel store, memory, and in-memory history.
pub(super) async fn erase_deleted(inner: &Inner, conversation_id: &str, message_id: &str) -> Result<(), String> {
    let account_id = inner.account_id.lock().clone().unwrap_or_default();
    let persona = inner.persona.lock().clone();
    let scope = super::accounts::memory_scope(persona.as_ref(), &account_id, conversation_id);
    let tracked = inner.take_tracked(message_id);
    super::erase::erase_message(&inner.store, &account_id, &scope, message_id).map_err(|error| error.to_string())?;
    if let Some(tracked) = tracked.as_ref() {
        for object_id in &tracked.objects {
            super::erase::erase_media(&inner.store, &account_id, object_id).map_err(|error| error.to_string())?;
        }
        scrub_history(inner, &tracked.content);
        let memory = inner.attached_memory.lock().clone();
        if let Some(memory) = memory {
            let scoped = crate::memory::encrypted_scope::create_ramflux_scoped_memory(
                memory,
                std::sync::Arc::clone(&inner.crypto),
                account_id,
                scope,
            );
            let _ = scoped.forget(&tracked.memory_key).await;
            let _ = scoped.forget(&format!("convturn:{conversation_id}:{message_id}")).await;
        }
    }
    Ok(())
}

fn scrub_history(inner: &Inner, content: &str) {
    if content.is_empty() {
        return;
    }
    let Some(histories) = inner.histories.lock().clone() else {
        return;
    };
    let mut histories = histories.lock();
    for turns in histories.values_mut() {
        turns.retain(|turn: &ChatMessage| !turn.content.contains(content));
    }
}

fn attachment_ids(content: &str) -> Vec<String> {
    let mut ids = Vec::new();
    for line in content.lines() {
        let Some(rest) = line.trim().strip_prefix("[attachment:") else {
            continue;
        };
        let Some(id) = rest.strip_suffix(']') else {
            continue;
        };
        if !id.is_empty() {
            ids.push(id.to_string());
        }
    }
    ids
}
