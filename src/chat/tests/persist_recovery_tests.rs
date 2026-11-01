//! Chat and agent share one SQLite store. An unverifiable ingress capsule or
//! terminal record is quarantined so the next chat write can proceed.
//! A fresh `SqliteMemory` is a new process: one store caches its ingress lifetime.

use super::*;
use crate::memory::{Memory, MemoryVisibility, MessageEvent, MessageEventInput, MessageEventSource, SqliteMemory};
use anyhow::Context;
use std::path::Path;
use std::sync::Arc;

fn open_store(dir: &Path) -> Arc<dyn Memory> {
    Arc::new(SqliteMemory::new(dir).expect("agent store"))
}

fn config_at(dir: &Path) -> crate::config::Config {
    let mut config = crate::config::Config::default();
    config.workspace_dir = dir.to_path_buf();
    config.causal_tree.enabled = true;
    config
}

async fn record_agent_note(memory: &Arc<dyn Memory>, workspace: &Path) {
    let workspace_id = workspace.to_string_lossy().into_owned();
    memory
        .append_message_event(MessageEventInput {
            event_id: None,
            idempotency_key: Some("agent:prx-note".to_owned()),
            workspace_id: workspace_id.clone(),
            owner_id: Some(format!("owner:{workspace_id}:agent")),
            source: MessageEventSource::Agent,
            channel: Some("cli".to_owned()),
            session_key: Some("agent:prx".to_owned()),
            parent_session_key: None,
            run_id: Some("prx-agent-run".to_owned()),
            parent_run_id: None,
            agent_id: None,
            persona_id: None,
            sender: Some("local-user".to_owned()),
            recipient: None,
            role: "user".to_owned(),
            event_type: "message.created".to_owned(),
            subject: None,
            goal_id: None,
            causation_event_id: None,
            correlation_id: None,
            attempt_id: None,
            lease_epoch: None,
            config_generation_id: None,
            config_source_revision: None,
            content: "prx agent note".to_owned(),
            raw_payload_json: None,
            visibility: MemoryVisibility::Session,
        })
        .await
        .expect("agent note in the shared store");
}

async fn write_chat(
    config: &Arc<crate::config::Config>,
    memory: &Arc<dyn Memory>,
    session: &str,
    run: &str,
    text: &str,
    key: &str,
) -> anyhow::Result<MessageEvent> {
    let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())?;
    let config = Arc::clone(config);
    let memory = Arc::clone(memory);
    let session = session.to_owned();
    let run = run.to_owned();
    let text = text.to_owned();
    let key = key.to_owned();
    cleanup
        .scope_admission(async move {
            let tracker = crate::runtime::turn_cleanup::current_tracker().context("cleanup owner")?;
            let ticket = tracker.register(tokio_util::sync::CancellationToken::new())?;
            let fabric = MemoryFabric::new(memory, config.workspace_dir.to_string_lossy())
                .with_event_recording(config.memory.event_recording_config())
                .with_ingress_recovery(&config, &ticket)
                .await?;
            let scope = chat_message_event_scope(fabric.workspace_id(), &session, &run, "ollama", "prx-local-model");
            let event = fabric.record_inbound_user_message(scope, text, Some(key), None).await;
            let ok = event.is_ok();
            ticket.finish(ok);
            event
        })
        .await
}

async fn append_bad_final(memory: &Arc<dyn Memory>, source: &MessageEvent) {
    let run = source.run_id.clone().expect("run");
    memory
        .append_message_event(MessageEventInput {
            event_id: None,
            idempotency_key: Some(format!("turn:{run}:final")),
            workspace_id: source.workspace_id.clone(),
            owner_id: source.owner_id.clone(),
            source: source.source.clone(),
            channel: source.channel.clone(),
            session_key: source.session_key.clone(),
            parent_session_key: None,
            run_id: Some(run.clone()),
            parent_run_id: None,
            agent_id: None,
            persona_id: None,
            sender: Some("prx_runtime".to_owned()),
            recipient: Some("local-user".to_owned()),
            role: "event".to_owned(),
            event_type: "turn.finalized".to_owned(),
            subject: None,
            goal_id: None,
            causation_event_id: Some(source.event_id.clone()),
            correlation_id: None,
            attempt_id: None,
            lease_epoch: None,
            config_generation_id: source.config_generation_id,
            config_source_revision: source.config_source_revision.clone(),
            content: "terminal_id status=completed".to_owned(),
            raw_payload_json: Some(
                serde_json::json!({
                    "terminal_id": run,
                    "terminal_replay_mac_v1": "00".repeat(32),
                })
                .to_string(),
            ),
            visibility: source.visibility.clone(),
        })
        .await
        .expect("unverifiable terminal marker");
}

async fn quarantine_marker(memory: &Arc<dyn Memory>, source: &MessageEvent) -> MessageEvent {
    let owner = source.owner_id.as_deref().expect("owner");
    memory
        .find_owned_message_event_by_idempotency_key(
            &source.workspace_id,
            owner,
            &format!("ingress:{}:quarantine", source.event_id),
        )
        .await
        .expect("lookup")
        .expect("quarantine marker")
}

#[tokio::test]
async fn chat_inbound_on_an_agent_store_survives_an_unverifiable_capsule() {
    let dir = tempfile::tempdir().expect("dir");
    let config = Arc::new(config_at(dir.path()));
    let first = {
        let memory = open_store(dir.path());
        record_agent_note(&memory, dir.path()).await;
        let first = write_chat(&config, &memory, "prx-session", "prx-run-1", "prx hello", "chat:1:user")
            .await
            .expect("first chat write");
        drop(memory);
        first
    };
    std::fs::rename(dir.path().join("self"), dir.path().join("self-previous")).expect("replace decision key directory");
    let memory = open_store(dir.path());
    let second = write_chat(&config, &memory, "prx-session", "prx-run-2", "prx again", "chat:2:user")
        .await
        .expect("chat inbound after an unverifiable capsule");
    assert!(second.id > 0);
    let marker = quarantine_marker(&memory, &first).await;
    assert_eq!(marker.event_type, "turn.ingress.quarantined");
    assert!(marker.content.contains("invalid ingress capsule authentication"));
    assert!(!marker.content.contains("prx hello"));
}

#[tokio::test]
async fn unverifiable_terminal_is_quarantined_so_chat_can_start() {
    let dir = tempfile::tempdir().expect("dir");
    let first = {
        let memory = open_store(dir.path());
        record_agent_note(&memory, dir.path()).await;
        let config = Arc::new(config_at(dir.path()));
        let first = write_chat(&config, &memory, "prx-session", "prx-run-1", "prx hello", "chat:1:user")
            .await
            .expect("first chat write");
        append_bad_final(&memory, &first).await;
        drop(memory);
        first
    };
    let memory = open_store(dir.path());
    let config = Arc::new(config_at(dir.path()));
    let generation = crate::config::new_shared(config_at(dir.path())).pin();
    let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())
        .expect("cleanup");
    let memory_for_publish = Arc::clone(&memory);
    cleanup
        .scope_admission(async move {
            let tracker = crate::runtime::turn_cleanup::current_tracker().context("cleanup owner")?;
            let ticket = tracker.register(tokio_util::sync::CancellationToken::new())?;
            let fabric = MemoryFabric::new(Arc::clone(&memory_for_publish), config.workspace_dir.to_string_lossy())
                .with_event_recording(config.memory.event_recording_config())
                .with_ingress_recovery(&config, &ticket)
                .await?;
            let scope = chat_message_event_scope(
                fabric.workspace_id(),
                "prx-session",
                "prx-run-2",
                "ollama",
                "prx-local-model",
            );
            let second = fabric
                .record_inbound_user_message(scope, "prx again", Some("chat:2:user".to_owned()), None)
                .await?;
            assert!(second.id > 0);
            let published = crate::runtime::turn_publication::TurnDecisionPublication {
                generation: Arc::clone(&generation),
                registry: Arc::new(Vec::new()),
                mcp_tool: None,
                security: Arc::new(crate::security::SecurityPolicy::from_config(
                    &generation.effective.autonomy,
                    &generation.effective.workspace_dir,
                )),
                observer: Arc::new(crate::observability::noop::NoopObserver),
                hooks: Arc::new(crate::hooks::HookManager::new(
                    generation.effective.workspace_dir.clone(),
                )),
                memory: memory_for_publish,
                plugin_generation: 0,
            }
            .publish(crate::causal_tree::state::TurnSurfaceKind::Interactive)
            .await;
            if let Ok(service) = &published {
                service
                    .shutdown(std::time::Instant::now() + std::time::Duration::from_secs(2))
                    .await;
            }
            let ok = published.is_ok();
            ticket.finish(ok);
            published.map(|_| ())
        })
        .await
        .expect("chat start after an unverifiable terminal");
    let marker = quarantine_marker(&memory, &first).await;
    assert_eq!(marker.event_type, "turn.ingress.quarantined");
    assert!(marker.content.contains("terminal replay authentication failed"));
}
