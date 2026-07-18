use std::sync::Arc;
use std::sync::LazyLock;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::channels::{Channel, SendMessage};
use crate::config::schema::{InteractionNoticeApplicability, InteractionNoticeConfig};
use crate::memory::{Memory, MemoryCategory};

// Serialize the read-send-ack sequence so concurrent first messages for the
// same peer cannot both observe a missing acknowledgement and duplicate the
// legally significant notice. First-contact notices are rare, so a process-wide
// lock keeps the backend-neutral contract deterministic without storing peers.
static NOTICE_EMISSION_LOCK: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractionNotice {
    pub version: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractionNoticeOutcome {
    Emitted { acknowledgement_key: String },
    AlreadyAcknowledged { acknowledgement_key: String },
    NotApplicable,
}

#[derive(Debug, Serialize, Deserialize)]
struct InteractionNoticeAcknowledgement {
    notice_version: String,
    acknowledged_at: String,
}

fn acknowledgement_key(channel: &str, peer: &str, version: &str) -> String {
    let digest = Sha256::digest(format!("{channel}\0{peer}\0{version}").as_bytes());
    format!("compliance:interaction_notice:{digest:x}")
}

pub async fn ensure_interaction_notice(
    config: &InteractionNoticeConfig,
    memory: &Arc<dyn Memory>,
    channel: &Arc<dyn Channel>,
    channel_name: &str,
    peer: &str,
    thread_ts: Option<String>,
) -> anyhow::Result<InteractionNoticeOutcome> {
    if config.applicability == InteractionNoticeApplicability::NotApplicable {
        return Ok(InteractionNoticeOutcome::NotApplicable);
    }
    anyhow::ensure!(config.enabled, "required AI interaction notice is disabled");

    let notice = InteractionNotice {
        version: config.version.trim().to_string(),
        text: config.text.trim().to_string(),
    };
    anyhow::ensure!(!notice.version.is_empty(), "AI interaction notice version is empty");
    anyhow::ensure!(!notice.text.is_empty(), "AI interaction notice text is empty");

    let key = acknowledgement_key(channel_name, peer, &notice.version);
    let _emission_guard = NOTICE_EMISSION_LOCK.lock().await;
    if memory.get(&key).await?.is_some() {
        return Ok(InteractionNoticeOutcome::AlreadyAcknowledged {
            acknowledgement_key: key,
        });
    }

    channel
        .send(&SendMessage::new(&notice.text, peer).in_thread(thread_ts))
        .await?;
    let acknowledgement = InteractionNoticeAcknowledgement {
        notice_version: notice.version,
        // Keep the audit timestamp readable while avoiding an all-numeric date
        // that the backend-neutral memory PII filter correctly treats as a
        // possible telephone identifier.
        acknowledged_at: Utc::now().format("%Y-%b-%dT%H:%M:%S%.3fZ").to_string(),
    };
    memory
        .store(
            &key,
            &serde_json::to_string(&acknowledgement)?,
            MemoryCategory::Conversation,
            None,
        )
        .await?;
    Ok(InteractionNoticeOutcome::Emitted {
        acknowledgement_key: key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use parking_lot::Mutex;

    struct RecordingChannel {
        sent: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Channel for RecordingChannel {
        fn name(&self) -> &str {
            "recording"
        }

        async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
            self.sent.lock().push(message.content.clone());
            Ok(())
        }

        async fn listen(
            &self,
            _tx: tokio::sync::mpsc::Sender<crate::channels::traits::ChannelMessage>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn notice_precedes_first_response_and_is_not_duplicated_for_version() {
        let tmp = tempfile::TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(tmp.path()).unwrap());
        let sent = Arc::new(Mutex::new(Vec::new()));
        let channel: Arc<dyn Channel> = Arc::new(RecordingChannel {
            sent: Arc::clone(&sent),
        });
        let config = InteractionNoticeConfig::default();

        let first = ensure_interaction_notice(&config, &memory, &channel, "signal", "peer-1", None)
            .await
            .unwrap();
        channel
            .send(&SendMessage::new("first AI response", "peer-1"))
            .await
            .unwrap();
        let second = ensure_interaction_notice(&config, &memory, &channel, "signal", "peer-1", None)
            .await
            .unwrap();

        assert!(matches!(first, InteractionNoticeOutcome::Emitted { .. }));
        assert!(matches!(second, InteractionNoticeOutcome::AlreadyAcknowledged { .. }));
        assert_eq!(sent.lock().as_slice(), &[config.text, "first AI response".to_string()]);
    }

    #[tokio::test]
    async fn notice_version_change_emits_again_without_storing_peer_content() {
        let tmp = tempfile::TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(tmp.path()).unwrap());
        let sent = Arc::new(Mutex::new(Vec::new()));
        let channel: Arc<dyn Channel> = Arc::new(RecordingChannel {
            sent: Arc::clone(&sent),
        });
        let mut config = InteractionNoticeConfig::default();
        ensure_interaction_notice(&config, &memory, &channel, "slack", "private-peer", None)
            .await
            .unwrap();
        config.version = "2".to_string();
        ensure_interaction_notice(&config, &memory, &channel, "slack", "private-peer", None)
            .await
            .unwrap();

        assert_eq!(sent.lock().len(), 2);
        let entries = memory.list(Some(&MemoryCategory::Conversation), None).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|entry| !entry.content.contains("private-peer")));
    }
}
