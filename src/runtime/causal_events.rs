//! Bounded, nonblocking persistence for causal telemetry.
//!
//! The observer receives ordinary phase events synchronously. Applied events
//! notify it only after winning durable insertion; crash recovery can retain
//! the durable event without a notification. A single task writes a
//! metadata-only copy to `message_events` in order; no prompt, state,
//! request/response, endpoint, or configured model enters this copy.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{mpsc, oneshot};

use super::turn_decision::VerifiedDirectiveApplication;
use crate::config::ConfigGeneration;
use crate::decision::telemetry::{
    CausalEvent, CausalEventFields, CausalEventKind, CausalEventSink, CausalLineage, IncompleteReason,
};
use crate::memory::{Memory, MemoryVisibility, MessageEventInput, MessageEventSource};

const EVENT_QUEUE_CAPACITY: usize = 256;

enum QueueItem {
    Event(Box<CausalEvent>),
    Applied {
        event: Box<CausalEvent>,
        idempotency_key: String,
        original_generation: Option<(u64, Arc<str>)>,
        acknowledgement: Option<oneshot::Sender<bool>>,
    },
    Drain(oneshot::Sender<()>),
}

/// The decision service owns this sink only while the plane has a durable key
/// and writable store. Call `drain` before stopping its runtime.
pub(crate) struct DurableCausalEventSink {
    memory: Arc<dyn Memory>,
    workspace_id: String,
    generation_id: u64,
    revision: String,
    observer: Arc<dyn CausalEventSink>,
    queue: mpsc::Sender<QueueItem>,
}

impl DurableCausalEventSink {
    pub(crate) fn new(
        generation: &ConfigGeneration,
        memory: Arc<dyn Memory>,
        observer: Arc<dyn CausalEventSink>,
    ) -> Result<Self, tokio::runtime::TryCurrentError> {
        let handle = tokio::runtime::Handle::try_current()?;
        let (queue, mut receiver) = mpsc::channel(EVENT_QUEUE_CAPACITY);
        let workspace_id = generation.effective.workspace_dir.to_string_lossy().to_string();
        let config_generation_id = generation.id.0;
        let config_source_revision = generation.source_revision.fingerprint_sha256.clone();
        let replay_memory = Arc::clone(&memory);
        let replay_workspace = workspace_id.clone();
        let replay_revision = config_source_revision.clone();
        let worker_observer = Arc::clone(&observer);
        handle.spawn(async move {
            while let Some(item) = receiver.recv().await {
                match item {
                    QueueItem::Event(event) => {
                        let input =
                            to_message_input(&event, &workspace_id, config_generation_id, &config_source_revision);
                        if memory.append_message_event(input).await.is_err() {
                            report_incomplete(
                                worker_observer.as_ref(),
                                event.lineage.clone(),
                                IncompleteReason::StoreFailed,
                            );
                        }
                    }
                    QueueItem::Drain(reply) => {
                        let _ = reply.send(());
                    }
                    QueueItem::Applied {
                        event,
                        idempotency_key,
                        original_generation,
                        acknowledgement,
                    } => {
                        let (generation_id, revision) = original_generation.as_ref().map_or(
                            (config_generation_id, config_source_revision.as_str()),
                            |(id, revision)| (*id, revision.as_ref()),
                        );
                        let mut input = to_message_input(&event, &workspace_id, generation_id, revision);
                        let event_id = uuid::Uuid::new_v4().to_string();
                        input.event_id = Some(event_id.clone());
                        input.idempotency_key = Some(idempotency_key);
                        let persisted = match memory.append_message_event(input.clone()).await {
                            Ok(stored) if applied_input_matches(&stored, &input) => {
                                if stored.event_id == event_id {
                                    worker_observer.emit(*event);
                                }
                                true
                            }
                            Ok(_) => {
                                report_incomplete(worker_observer.as_ref(), event.lineage, IncompleteReason::Conflict);
                                false
                            }
                            Err(_) => {
                                report_incomplete(
                                    worker_observer.as_ref(),
                                    event.lineage,
                                    IncompleteReason::StoreFailed,
                                );
                                false
                            }
                        };
                        if let Some(reply) = acknowledgement {
                            let _ = reply.send(persisted);
                        }
                    }
                }
            }
        });
        Ok(Self {
            observer,
            queue,
            memory: replay_memory,
            workspace_id: replay_workspace,
            generation_id: config_generation_id,
            revision: replay_revision,
        })
    }

    /// Wait at most until `deadline` for every event queued before this marker.
    /// Ordinary decisions never wait on telemetry.
    pub(crate) async fn drain(&self, deadline: Instant) -> bool {
        let (reply, received) = oneshot::channel();
        let remaining = deadline.saturating_duration_since(Instant::now());
        tokio::time::timeout(remaining, async {
            self.queue.send(QueueItem::Drain(reply)).await.is_ok() && received.await.is_ok()
        })
        .await
        .unwrap_or(false)
    }

    /// Admission is the cancellation boundary: an admitted verified event
    /// persists as completion telemetry even if the caller later cancels.
    /// Queue or store failure never changes an already committed close.
    pub(super) fn emit_applied(&self, application: VerifiedDirectiveApplication) {
        let (event, idempotency_key, original_generation) = application.into_parts();
        let lineage = event.lineage.clone();
        if self
            .queue
            .try_send(QueueItem::Applied {
                event: Box::new(event),
                idempotency_key,
                original_generation,
                acknowledgement: None,
            })
            .is_err()
        {
            report_incomplete(self.observer.as_ref(), lineage, IncompleteReason::StoreDropped);
        }
    }
    /// Recovery waits for the actual idempotent insert/readback result, not
    /// merely queue admission. The private application carries authenticated
    /// original-generation metadata across a service restart.
    pub(super) async fn repair_applied_until(
        &self,
        application: VerifiedDirectiveApplication,
        deadline: Instant,
    ) -> anyhow::Result<()> {
        let (event, idempotency_key, original_generation) = application.into_parts();
        let (generation, revision) = original_generation
            .as_ref()
            .map_or((self.generation_id, self.revision.as_str()), |(id, revision)| {
                (*id, revision.as_ref())
            });
        let mut input = to_message_input(&event, &self.workspace_id, generation, revision);
        input.event_id = Some(uuid::Uuid::new_v4().to_string());
        input.idempotency_key = Some(idempotency_key);
        let stored = self.memory.append_message_event_until(input.clone(), deadline).await?;
        anyhow::ensure!(
            applied_input_matches(&stored, &input),
            "rejoined applied event conflicts with authenticated terminal"
        );
        let scope = crate::memory::ingress_recovery::IngressRecoveryScope {
            workspace_id: input.workspace_id.clone(),
            owner_id: input
                .owner_id
                .clone()
                .ok_or_else(|| anyhow::anyhow!("applied recovery owner missing"))?,
            source: input.source.clone(),
        };
        let readback = self
            .memory
            .read_ingress_event(
                &scope,
                &crate::memory::ingress_recovery::IngressEventLookup::EventId(stored.event_id.clone()),
                deadline,
            )
            .await?
            .ok_or_else(|| anyhow::anyhow!("applied recovery readback missing"))?;
        anyhow::ensure!(
            applied_input_matches(&readback, &input),
            "applied recovery readback changed"
        );
        if input.event_id.as_deref() == Some(stored.event_id.as_str()) {
            self.observer.emit(event);
        }
        Ok(())
    }

    pub(super) async fn repair_applied(&self, application: VerifiedDirectiveApplication, deadline: Instant) -> bool {
        let (event, idempotency_key, original_generation) = application.into_parts();
        let (reply, received) = oneshot::channel();
        let remaining = deadline.saturating_duration_since(Instant::now());
        tokio::time::timeout(remaining, async {
            if self
                .queue
                .send(QueueItem::Applied {
                    event: Box::new(event),
                    idempotency_key,
                    original_generation,
                    acknowledgement: Some(reply),
                })
                .await
                .is_err()
            {
                return false;
            }
            received.await.unwrap_or(false)
        })
        .await
        .unwrap_or(false)
    }
}

fn applied_input_matches(stored: &crate::memory::MessageEvent, input: &MessageEventInput) -> bool {
    let (Ok(serde_json::Value::Object(stored_fields)), Ok(serde_json::Value::Object(input_fields))) =
        (serde_json::to_value(stored), serde_json::to_value(input))
    else {
        return false;
    };
    // Event id identifies the insertion winner; all other input fields must
    // agree for a retry. Row id, timestamp and content hash are store-owned.
    stored.id > 0
        && !stored.event_id.is_empty()
        && input_fields
            .iter()
            .filter(|(key, _)| key.as_str() != "event_id")
            .all(|(key, value)| stored_fields.get(key) == Some(value))
}

impl CausalEventSink for DurableCausalEventSink {
    fn emit(&self, event: CausalEvent) {
        if event.kind == CausalEventKind::DirectiveApplied {
            report_incomplete(self.observer.as_ref(), event.lineage, IncompleteReason::UnknownDecision);
            return;
        }
        self.observer.emit(event.clone());
        if self.queue.try_send(QueueItem::Event(Box::new(event.clone()))).is_err() {
            report_incomplete(self.observer.as_ref(), event.lineage, IncompleteReason::StoreDropped);
        }
    }
}

fn report_incomplete(observer: &dyn CausalEventSink, lineage: CausalLineage, reason: IncompleteReason) {
    observer.emit(CausalEvent {
        kind: CausalEventKind::OutcomeIncomplete,
        lineage,
        fields: CausalEventFields {
            incomplete: Some(reason),
            ..CausalEventFields::default()
        },
    });
}

fn to_message_input(
    event: &CausalEvent,
    workspace_id: &str,
    config_generation_id: u64,
    config_source_revision: &str,
) -> MessageEventInput {
    let fields = &event.fields;
    let payload = serde_json::json!({
        "schema_version": 1,
        "item_count": fields.item_count,
        "backend": fields.backend.map(|value| value.as_str()),
        "locality": fields.locality.map(|value| value.as_str()),
        "error_class": fields.error_class,
        "error_rule": fields.error_rule,
        "to_backend": fields.to_backend.map(|value| value.as_str()),
        "circuit": fields.circuit.map(|value| value.as_str()),
        "served": fields.served.map(|value| value.as_str()),
        "incomplete": fields.incomplete.map(|value| value.as_str()),
        "latency_ms": fields.latency_ms,
        "source_event_id": event.lineage.source_event_id.as_deref(),
    });
    MessageEventInput {
        event_id: None,
        idempotency_key: None,
        workspace_id: workspace_id.to_string(),
        owner_id: event.lineage.owner_id.as_deref().map(ToString::to_string),
        source: MessageEventSource::Other("causal".to_string()),
        channel: None,
        session_key: event.lineage.session_key.as_deref().map(ToString::to_string),
        parent_session_key: None,
        run_id: event.lineage.run_id.as_deref().map(ToString::to_string),
        parent_run_id: None,
        agent_id: None,
        persona_id: None,
        sender: None,
        recipient: None,
        role: "system".to_string(),
        event_type: event.kind.as_str().to_string(),
        subject: None,
        goal_id: None,
        causation_event_id: event
            .lineage
            .causation_event_id
            .as_deref()
            .or(event.lineage.source_event_id.as_deref())
            .map(ToString::to_string),
        correlation_id: event.lineage.decision_id.as_deref().map(ToString::to_string),
        attempt_id: None,
        lease_epoch: None,
        config_generation_id: Some(config_generation_id),
        config_source_revision: Some(config_source_revision.to_string()),
        content: event.kind.as_str().to_string(),
        raw_payload_json: Some(payload.to_string()),
        visibility: if event.lineage.session_key.is_some() {
            MemoryVisibility::Session
        } else {
            MemoryVisibility::Workspace
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decision::protocol::{BackendKind, BackendLocality};
    use crate::decision::telemetry::testing::RecordingSink;
    use crate::memory::{MemoryPrincipal, NoneMemory, SqliteMemory};

    #[tokio::test(flavor = "current_thread")]
    async fn queue_overflow_and_store_failure_report_incomplete_without_blocking() {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        let generation = crate::config::new_shared(config).pin();
        let observer = Arc::new(RecordingSink::default());
        let sink =
            DurableCausalEventSink::new(&generation, Arc::new(NoneMemory), observer.clone()).expect("runtime sink");
        let event = CausalEvent {
            kind: CausalEventKind::PredictionFailed,
            lineage: CausalLineage::default(),
            fields: CausalEventFields::default(),
        };
        for _ in 0..=EVENT_QUEUE_CAPACITY {
            sink.emit(event.clone());
        }
        assert!(
            observer
                .events
                .lock()
                .iter()
                .any(|entry| entry.fields.incomplete == Some(IncompleteReason::StoreDropped))
        );
        assert!(!sink.drain(Instant::now()).await, "expired drain must return promptly");
        assert!(sink.drain(Instant::now() + std::time::Duration::from_secs(2)).await);
        assert!(
            observer
                .events
                .lock()
                .iter()
                .any(|entry| entry.fields.incomplete == Some(IncompleteReason::StoreFailed))
        );
    }

    #[tokio::test]
    async fn durable_sink_records_bounded_fields_without_model_or_request_text() {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        let generation = crate::config::new_shared(config).pin();
        let memory = Arc::new(SqliteMemory::new(workspace.path()).expect("event backend"));
        let observer = Arc::new(RecordingSink::default());
        let sink = DurableCausalEventSink::new(&generation, memory.clone(), observer.clone()).expect("runtime sink");
        sink.emit(CausalEvent {
            kind: CausalEventKind::PredictionFailed,
            lineage: CausalLineage {
                workspace_id: Arc::from("foreign_workspace"),
                owner_id: Some(Arc::from("prx_owner")),
                session_key: Some(Arc::from("prx_session")),
                source_event_id: Some(Arc::from("prx_source")),
                decision_id: Some(Arc::from("prx_decision")),
                ..CausalLineage::default()
            },
            fields: CausalEventFields {
                backend: Some(BackendKind::CompatibleHttp),
                locality: Some(BackendLocality::Loopback),
                configured_model: Some(Arc::from("PRX-PRIVATE-MODEL-SENTINEL")),
                error_class: Some("unavailable"),
                latency_ms: Some(17),
                ..CausalEventFields::default()
            },
        });
        sink.emit(CausalEvent {
            kind: CausalEventKind::CandidateGated,
            lineage: CausalLineage {
                owner_id: Some(Arc::from("prx_owner")),
                session_key: Some(Arc::from("prx_session")),
                source_event_id: Some(Arc::from("prx_source")),
                decision_id: Some(Arc::from("prx_decision")),
                ..CausalLineage::default()
            },
            fields: CausalEventFields {
                item_count: Some(3),
                ..CausalEventFields::default()
            },
        });
        assert!(sink.drain(Instant::now() + std::time::Duration::from_secs(2)).await);
        let principal = MemoryPrincipal {
            workspace_id: workspace.path().to_string_lossy().to_string(),
            owner_id: Some("prx_owner".to_string()),
            session_key: Some("prx_session".to_string()),
            ..MemoryPrincipal::default()
        };
        let events = memory
            .list_message_events_since(&principal, 0, 10)
            .await
            .expect("read durable event");
        assert_eq!(events.len(), 2);
        let event = events.first().expect("one event");
        assert_eq!(event.event_type, "causal.prediction.failed");
        assert_eq!(event.role, "system");
        assert_eq!(event.workspace_id, workspace.path().to_string_lossy());
        assert_eq!(event.correlation_id.as_deref(), Some("prx_decision"));
        let payload = event.raw_payload_json.as_deref().expect("bounded payload");
        assert!(payload.contains("\"latency_ms\":17"));
        assert!(!payload.contains("PRX-PRIVATE-MODEL-SENTINEL"));
        let gated = events.get(1).expect("gated phase event");
        assert_eq!(gated.event_type, "causal.candidate.gated");
        assert_eq!(gated.correlation_id.as_deref(), Some("prx_decision"));
        assert_eq!(gated.causation_event_id.as_deref(), Some("prx_source"));
        let gated_payload = gated.raw_payload_json.as_deref().expect("bounded phase payload");
        assert!(gated_payload.contains("\"item_count\":3"));
        assert!(!gated_payload.contains("PRX-PRIVATE-MODEL-SENTINEL"));
        assert_eq!(observer.events.lock().len(), 2);
    }
    fn applied_fixture(workspace: &str, terminal: &str) -> CausalEvent {
        CausalEvent {
            kind: CausalEventKind::DirectiveApplied,
            lineage: CausalLineage {
                workspace_id: Arc::from(workspace),
                owner_id: Some(Arc::from("prx_owner")),
                session_key: Some(Arc::from("prx_session")),
                run_id: Some(Arc::from("prx_run")),
                source_event_id: Some(Arc::from("prx_source")),
                causation_event_id: Some(Arc::from(terminal)),
                decision_id: Some(Arc::from("prx_decision")),
                ..CausalLineage::default()
            },
            fields: CausalEventFields {
                item_count: Some(1),
                ..CausalEventFields::default()
            },
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn applied_admission_deduplicates_after_sink_reconstruction_and_rejects_conflicts() {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        let generation = crate::config::new_shared(config).pin();
        let memory = Arc::new(SqliteMemory::new(workspace.path()).expect("event backend"));
        let observer = Arc::new(RecordingSink::default());
        let sink = DurableCausalEventSink::new(&generation, memory.clone(), observer.clone()).expect("sink");
        let event = applied_fixture(&workspace.path().to_string_lossy(), "prx_terminal");
        let key = "causal:prx_decision:directive-applied".to_owned();
        sink.emit(event.clone());
        assert!(
            observer
                .events
                .lock()
                .iter()
                .all(|event| event.kind != CausalEventKind::DirectiveApplied),
            "generic emit has no application authority"
        );
        for _ in 0..2 {
            sink.emit_applied(VerifiedDirectiveApplication::fixture(event.clone(), key.clone()));
        }
        assert!(sink.drain(Instant::now() + std::time::Duration::from_secs(5)).await);
        assert_eq!(
            observer
                .events
                .lock()
                .iter()
                .filter(|event| event.kind == CausalEventKind::DirectiveApplied)
                .count(),
            1
        );
        let row = memory
            .find_message_event_by_idempotency_key(&workspace.path().to_string_lossy(), &key)
            .await
            .expect("read row")
            .expect("durable application");
        assert_eq!(row.causation_event_id.as_deref(), Some("prx_terminal"));
        drop(sink);
        let reopened = Arc::new(SqliteMemory::new(workspace.path()).expect("reopened event backend"));
        let restarted_observer = Arc::new(RecordingSink::default());
        let restarted = DurableCausalEventSink::new(&generation, reopened.clone(), restarted_observer.clone())
            .expect("reconstructed sink");
        restarted.emit_applied(VerifiedDirectiveApplication::fixture(event, key.clone()));
        restarted.emit_applied(VerifiedDirectiveApplication::fixture(
            applied_fixture(&workspace.path().to_string_lossy(), "prx_other_terminal"),
            key.clone(),
        ));
        assert!(
            restarted
                .drain(Instant::now() + std::time::Duration::from_secs(5))
                .await
        );
        assert!(
            restarted_observer
                .events
                .lock()
                .iter()
                .all(|event| event.kind != CausalEventKind::DirectiveApplied)
        );
        assert!(
            restarted_observer
                .events
                .lock()
                .iter()
                .any(|event| event.fields.incomplete == Some(IncompleteReason::Conflict))
        );
        assert_eq!(
            reopened
                .find_message_event_by_idempotency_key(&workspace.path().to_string_lossy(), &key)
                .await
                .expect("retained row")
                .expect("retained application")
                .id,
            row.id
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn applied_queue_or_store_failure_leaves_retry_available() {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        let generation = crate::config::new_shared(config).pin();
        let memory = Arc::new(SqliteMemory::new(workspace.path()).expect("event backend"));
        let observer = Arc::new(RecordingSink::default());
        let sink = DurableCausalEventSink::new(&generation, memory.clone(), observer.clone()).expect("sink");
        let event = applied_fixture(&workspace.path().to_string_lossy(), "prx_terminal");
        let key = "causal:prx_decision:directive-applied".to_owned();
        for _ in 0..EVENT_QUEUE_CAPACITY {
            sink.emit(CausalEvent {
                kind: CausalEventKind::SnapshotCreated,
                lineage: event.lineage.clone(),
                fields: CausalEventFields::default(),
            });
        }
        sink.emit_applied(VerifiedDirectiveApplication::fixture(event.clone(), key.clone()));
        assert!(
            observer
                .events
                .lock()
                .iter()
                .any(|event| event.fields.incomplete == Some(IncompleteReason::StoreDropped))
        );
        assert!(sink.drain(Instant::now() + std::time::Duration::from_secs(5)).await);
        assert!(
            memory
                .find_message_event_by_idempotency_key(&workspace.path().to_string_lossy(), &key)
                .await
                .expect("dropped admission")
                .is_none()
        );
        let failed_observer = Arc::new(RecordingSink::default());
        let failed = DurableCausalEventSink::new(&generation, Arc::new(NoneMemory), failed_observer.clone())
            .expect("failing sink");
        failed.emit_applied(VerifiedDirectiveApplication::fixture(event.clone(), key.clone()));
        assert!(failed.drain(Instant::now() + std::time::Duration::from_secs(5)).await);
        assert!(
            failed_observer
                .events
                .lock()
                .iter()
                .any(|event| event.fields.incomplete == Some(IncompleteReason::StoreFailed))
        );
        assert!(
            failed_observer
                .events
                .lock()
                .iter()
                .all(|event| event.kind != CausalEventKind::DirectiveApplied)
        );
        sink.emit_applied(VerifiedDirectiveApplication::fixture(event, key.clone()));
        assert!(sink.drain(Instant::now() + std::time::Duration::from_secs(5)).await);
        assert!(
            memory
                .find_message_event_by_idempotency_key(&workspace.path().to_string_lossy(), &key)
                .await
                .expect("valid retry")
                .is_some()
        );
        assert_eq!(
            observer
                .events
                .lock()
                .iter()
                .filter(|event| event.kind == CausalEventKind::DirectiveApplied)
                .count(),
            1
        );
    }
}
