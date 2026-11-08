//! Turn hooks the ramflux channel installs on a real prx turn.
//!
//! Persona, attachment fetch, the final surface, and message deletion all
//! live here so the channel module stays a session object.

use super::Inner;
use crate::channels::rich::{RichReply, SurfaceBlock, SurfaceOp, SurfaceReply};

/// Fetch structured attachments for `message_id` and append multimodal parts.
///
/// Ids come from the inbound attachment list, never from free text. Image
/// bytes become an `[IMAGE:data:…]` part. Text-like bytes become a named text
/// part. Anything else is a typed stub with size and sha256.
pub(super) async fn enrich_inbound(
    channel: &super::RamfluxChannel,
    content: &str,
    conversation_id: &str,
    message_id: &str,
) -> String {
    let attachments = channel.take_attachments(conversation_id, message_id);
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
                channel.note_media(conversation_id, message_id, &attachment.object_id);
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
    let entry = super::erase_index::load(&inner.store, &inner.crypto, account_id, conversation_id, message_id)?;
    super::erase::erase_message(&inner.store, account_id, &scope, message_id).map_err(|error| error.to_string())?;
    if let Some(entry) = entry {
        for object_id in &entry.media_ids {
            super::erase::erase_media(&inner.store, account_id, conversation_id, object_id)
                .map_err(|error| error.to_string())?;
        }
    }
    if let Some(tracked) = inner.take_tracked(conversation_id, message_id) {
        for object_id in &tracked.objects {
            super::erase::erase_media(&inner.store, account_id, conversation_id, object_id)
                .map_err(|error| error.to_string())?;
        }
    }
    refresh_history(inner, account_id, conversation_id).await?;
    Ok(())
}

/// Reload by persisted row identity after erasure, never by vector offsets.
pub(super) async fn refresh_history(inner: &Inner, account: &str, conversation: &str) -> Result<(), String> {
    let Some(memory) = inner.attached_memory.lock().clone() else {
        return Err("memory is not attached".to_owned());
    };
    let scoped = inner.scoped_memory(memory, account, conversation);
    let principal = crate::memory::MemoryPrincipal {
        workspace_id: inner.workspace.to_string_lossy().into_owned(),
        agent_id: None,
        persona_id: None,
        session_key: None,
        channel: Some("ramflux".to_owned()),
        sender: None,
        owner_id: None,
        require_owner_match: false,
        legacy_session_key: None,
        agent_partition: None,
    };
    let loaded = scoped
        .load_recent_conversation_histories(
            &principal,
            super::super::MAX_CHANNEL_HISTORY,
            super::super::MAX_HYDRATED_SESSIONS,
        )
        .await
        .map_err(|error| error.to_string())?;
    let old = inner.history_sessions.lock().remove(conversation).unwrap_or_default();
    let current = loaded.keys().cloned().collect();
    inner.history_sessions.lock().insert(conversation.to_owned(), current);
    let histories = inner.histories.lock().clone();
    if let Some(histories) = histories {
        let mut histories = histories.lock();
        for session in old {
            histories.remove(&session);
        }
        for (session, turns) in loaded {
            histories.insert(
                session,
                turns
                    .into_iter()
                    .map(|turn| crate::agent::turn_contracts::stored_turn_message(turn.role, turn.content))
                    .collect(),
            );
        }
    }
    Ok(())
}

struct RevocationPause<'a>(&'a std::sync::atomic::AtomicU64);
impl Drop for RevocationPause<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// Revoke the configured retrieval scope, including events and ingress receipts.
pub(super) async fn erase_conversation_scope(
    inner: &Inner,
    conversation: &str,
    whole_account: bool,
) -> Result<(), String> {
    let account = inner
        .account_id
        .lock()
        .clone()
        .ok_or_else(|| "bridge account missing".to_owned())?;
    let backend = inner.store.backend().map_err(|error| error.to_string())?;
    let memory = inner
        .attached_memory
        .lock()
        .clone()
        .ok_or_else(|| "memory is not attached".to_owned())?;
    let across = whole_account
        || inner
            .persona
            .lock()
            .as_ref()
            .is_some_and(|persona| persona.cross_conversation_retrieval);
    let conversations = {
        let _admission = inner.admission.lock();
        inner.revocations.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let conversations: Vec<String> = if across {
            inner
                .tracked
                .lock()
                .values()
                .map(|tracked| tracked.conversation_id.clone())
                .chain(inner.turns.lock().keys().cloned())
                .chain(inner.history_sessions.lock().keys().cloned())
                .collect()
        } else {
            vec![conversation.to_owned()]
        };
        conversations
    };
    let _pause = RevocationPause(&inner.revocations);
    for conv in &conversations {
        let runs = inner
            .turns
            .lock()
            .get(conv)
            .map(|slots| slots.iter().map(|slot| slot.turn_id.clone()).collect::<Vec<_>>())
            .unwrap_or_default();
        inner.rpc.fence_turns(&runs);
        inner.cancel_conversation(conv);
    }
    loop {
        let changed = inner.turns_changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if !conversations.iter().any(|conv| {
            inner
                .slot_turns
                .lock()
                .values()
                .any(|key| &key.key.conversation == conv)
        }) {
            break;
        }
        changed.await;
    }
    let origin = crate::memory::erasure::ErasureOrigin::new(&account, conversation, "");
    let origins = super::eraser::storage(backend.clone(), move |store| {
        store.origins(
            &origin.account,
            if across {
                None
            } else {
                Some(origin.conversation.as_str())
            },
        )
    })
    .await?;
    for origin in &origins {
        super::eraser::fence_runs(inner, backend.clone(), origin.clone()).await?;
        let origin = origin.clone();
        super::eraser::storage(backend.clone(), move |store| {
            store.begin_delete(&origin).and_then(|()| store.erase_rows(&origin))
        })
        .await?;
    }
    memory.checkpoint_wal().await.map_err(|error| error.to_string())?;
    for conv in &conversations {
        refresh_history(inner, &account, conv).await?;
        super::erase::erase_conversation(&inner.store, &account, conv).map_err(|error| error.to_string())?;
    }
    for origin in origins {
        super::eraser::storage(backend.clone(), move |store| store.finish_delete(&origin)).await?;
    }
    Ok(())
}
