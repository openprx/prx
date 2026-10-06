//! Own admission persistence until ownership transfers to a model terminal.
use super::{GatewaySourceReceipt, GatewayTurnProof};
use crate::config::ConfigGeneration;
use crate::memory::MemoryFabric;
use crate::runtime::turn_cleanup::CleanupTicket;
use anyhow::Context;
use std::sync::Arc;

type Acknowledgement = tokio::sync::oneshot::Sender<Result<(), String>>;

/// Own the cleanup ticket before starting the durable input write. The task
/// keeps ownership across receipt readback and the channel send; dropping the
/// receiving request drops the returned admission guard instead of the write.
pub(in crate::gateway) struct GatewayPendingInput {
    pub(in crate::gateway) generation: Arc<ConfigGeneration>,
    pub(in crate::gateway) fabric: MemoryFabric,
    pub(in crate::gateway) envelope: crate::runtime::envelope::RuntimeEnvelope,
    pub(in crate::gateway) ticket: CleanupTicket,
    pub(in crate::gateway) acknowledgement: Option<Acknowledgement>,
}

impl GatewayPendingInput {
    pub(in crate::gateway) async fn record(
        mut self,
        input: String,
        idempotency_key: Option<String>,
    ) -> anyhow::Result<(
        GatewaySourceReceipt,
        Option<crate::memory::MessageEvent>,
        GatewayAdmissionGuard,
    )> {
        self.fabric = self
            .fabric
            .with_ingress_recovery(&self.generation.effective, &self.ticket)
            .await?;
        let run_id = self
            .envelope
            .run_id
            .as_deref()
            .context("gateway input run is missing")?;
        let idempotency_key = idempotency_key.unwrap_or_else(|| format!("gateway:{run_id}:input"));
        let (sender, receiver) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let mut guard = GatewayAdmissionGuard {
                receipt: None,
                generation: self.generation,
                fabric: self.fabric,
                ticket: Some(self.ticket),
                acknowledgement: self.acknowledgement,
                pending: None,
            };
            let operation = async {
                let recorded = guard
                    .fabric
                    .record_inbound_user_message(
                        self.envelope.message_scope(),
                        input.clone(),
                        Some(idempotency_key.clone()),
                        None,
                    )
                    .await
                    .map_or_else(
                        |_| {
                            // Preserve disabled-mode best effort without exposing
                            // private input embedded in a backend's error text.
                            tracing::warn!("gateway input persistence is unconfirmed");
                            None
                        },
                        Some,
                    );
                if let Some(event) = recorded.as_ref().filter(|event| event.id > 0) {
                    anyhow::ensure!(
                        event.idempotency_key.as_deref() == Some(idempotency_key.as_str()),
                        "gateway input receipt differs from its issued key"
                    );
                }
                let receipt = GatewaySourceReceipt::issue_for_turn(
                    guard.fabric.memory().as_ref(),
                    &guard.generation,
                    &self.envelope,
                    &input,
                    recorded.as_ref(),
                )
                .await?;
                Ok::<_, anyhow::Error>((receipt, recorded))
            };
            // Match the runtime cleanup acknowledgement budget. A stalled
            // backend is an explicit unconfirmed failure, never a successful
            // cancellation or permission to admit another turn.
            let result = tokio::time::timeout(std::time::Duration::from_secs(15), operation)
                .await
                .map_err(|_| anyhow::anyhow!("gateway input persistence exceeded the cleanup deadline"))
                .and_then(std::convert::identity);
            let result = match result {
                Ok((receipt, recorded)) => {
                    guard.receipt = Some(receipt.clone());
                    Ok((receipt, recorded, guard))
                }
                Err(_error) if guard.fabric.ingress_already_accepted() => {
                    if let Some(ticket) = guard.ticket.take() {
                        ticket.finish(true);
                    }
                    if let Some(ack) = guard.acknowledgement.take() {
                        let _ = ack.send(Err("input already accepted; outcome unconfirmed".into()));
                    }
                    Err(crate::runtime::ingress_recovery::AlreadyAccepted.into())
                }
                Err(error) => {
                    // An append can commit before its caller observes an
                    // error or timeout. Recover only the exact issued key,
                    // workspace and owner, then repeat all receipt checks.
                    // A missing row is not proof that a late writer stopped.
                    let recovery = async {
                        let memory = guard.fabric.memory();
                        let event = memory
                            .find_owned_message_event_by_idempotency_key(
                                &self.envelope.workspace_id,
                                &self.envelope.resolved_owner_id(),
                                &idempotency_key,
                            )
                            .await?
                            .context("gateway input commit is unconfirmed")?;
                        anyhow::ensure!(
                            event.idempotency_key.as_deref() == Some(idempotency_key.as_str()),
                            "gateway recovered input differs from its issued key"
                        );
                        GatewaySourceReceipt::issue_for_turn(
                            memory.as_ref(),
                            &guard.generation,
                            &self.envelope,
                            &input,
                            Some(&event),
                        )
                        .await
                    };
                    if let Ok(Ok(receipt)) = tokio::time::timeout(std::time::Duration::from_secs(15), recovery).await {
                        guard.receipt = Some(receipt);
                        let cleanup = guard.finish_failure(sender.is_closed(), "gateway input admission failed");
                        // If this wait expires Drop retains the same proposal;
                        // it cannot replace a failed status with cancelled.
                        let _ = tokio::time::timeout(std::time::Duration::from_secs(15), cleanup).await;
                    } else {
                        if let Some(ticket) = guard.ticket.take() {
                            ticket.finish(false);
                        }
                        if let Some(acknowledgement) = guard.acknowledgement.take() {
                            let _ = acknowledgement.send(Err("gateway input persistence is unconfirmed".to_owned()));
                        }
                    }
                    Err(error)
                }
            };
            // On a closed receiver SendError owns the result, whose guard
            // performs exact-source cancellation cleanup through the tracker.
            let _ = sender.send(result);
        });
        receiver.await.context("gateway input owner ended without a receipt")?
    }
}

pub(in crate::gateway) struct GatewayAdmissionGuard {
    receipt: Option<GatewaySourceReceipt>,
    generation: Arc<ConfigGeneration>,
    fabric: MemoryFabric,
    ticket: Option<CleanupTicket>,
    acknowledgement: Option<Acknowledgement>,
    pending: Option<(bool, String)>,
}

impl GatewayAdmissionGuard {
    pub(in crate::gateway) fn ingress_fabric(&self) -> MemoryFabric {
        self.fabric.clone()
    }

    pub(in crate::gateway) fn new(
        proof: &GatewayTurnProof,
        fabric: MemoryFabric,
        ticket: CleanupTicket,
        acknowledgement: Option<Acknowledgement>,
    ) -> Self {
        Self {
            receipt: Some(GatewaySourceReceipt {
                input: Arc::clone(&proof.input),
                envelope: proof.envelope.clone(),
            }),
            generation: Arc::clone(&proof.generation),
            fabric,
            ticket: Some(ticket),
            acknowledgement,
            pending: None,
        }
    }

    /// Attach the independent job monitor before its lazy work can be spawned.
    /// Replacing an existing owner would lose its acknowledgement, so reject it.
    pub(in crate::gateway) fn attach_acknowledgement(
        &mut self,
        acknowledgement: Acknowledgement,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.acknowledgement.is_none(),
            "admission cleanup acknowledgement already attached"
        );
        anyhow::ensure!(
            self.receipt.is_some() && self.ticket.is_some(),
            "admission cleanup owner is missing"
        );
        self.acknowledgement = Some(acknowledgement);
        Ok(())
    }

    /// Synchronous transfer leaves no await point without a cleanup owner.
    pub(in crate::gateway) fn into_terminal_parts(
        mut self,
    ) -> anyhow::Result<(CleanupTicket, Option<Acknowledgement>)> {
        anyhow::ensure!(self.pending.is_none(), "admission failure is already pending");
        let ticket = self.ticket.take().context("admission cleanup ticket missing")?;
        self.receipt = None;
        Ok((ticket, self.acknowledgement.take()))
    }

    pub(in crate::gateway) async fn finish_failure(&mut self, cancelled: bool, summary: &str) -> anyhow::Result<()> {
        anyhow::ensure!(self.pending.is_none(), "admission failure is already pending");
        self.pending = Some((cancelled, crate::security::redaction::redact_secrets(summary)));
        let (cancelled, summary) = self.pending.as_ref().context("admission failure missing")?;
        let result = self
            .receipt
            .as_ref()
            .context("admission receipt missing")?
            .clone()
            .record_admission_failure(&self.fabric, &self.generation, *cancelled, summary)
            .await;
        // Keep the proposal and receipt until the write returns: cancellation
        // during this await must replay the same status and summary in Drop.
        self.receipt = None;
        if let Some(ticket) = self.ticket.take() {
            ticket.finish(result.is_ok());
        }
        if let Some(acknowledgement) = self.acknowledgement.take() {
            let _ = acknowledgement.send(
                result
                    .as_ref()
                    .map(|_| ())
                    .map_err(|_| "admission failure persistence is unconfirmed".to_owned()),
            );
        }
        result
    }
}

impl Drop for GatewayAdmissionGuard {
    fn drop(&mut self) {
        let Some(receipt) = self.receipt.take() else {
            return;
        };
        let ticket = self.ticket.take();
        let acknowledgement = self.acknowledgement.take();
        let (cancelled, summary) = self
            .pending
            .take()
            .unwrap_or_else(|| (true, "decision admission task was dropped".to_owned()));
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            if let Some(ticket) = ticket {
                ticket.finish(false);
            }
            if let Some(acknowledgement) = acknowledgement {
                let _ = acknowledgement.send(Err("admission cleanup runtime unavailable".to_owned()));
            }
            return;
        };
        let fabric = self.fabric.clone();
        let generation = Arc::clone(&self.generation);
        runtime.spawn(async move {
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(15),
                receipt.record_admission_failure(&fabric, &generation, cancelled, &summary),
            )
            .await
            .map_err(|_| anyhow::anyhow!("gateway admission cleanup timed out"))
            .and_then(std::convert::identity);
            if let Some(ticket) = ticket {
                ticket.finish(result.is_ok());
            }
            if let Some(acknowledgement) = acknowledgement {
                let _ = acknowledgement
                    .send(result.map_err(|_| "admission cancellation persistence is unconfirmed".to_owned()));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod job_cleanup;
    use crate::memory::{Memory, MemoryVisibility, SqliteMemory};
    use crate::runtime::envelope::RuntimeEnvelope;

    #[tokio::test]
    async fn dropped_admission_acknowledges_persistence_and_handoff_does_not_write_failure() {
        for drop_before_decision in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let mut config = crate::config::Config::default();
            config.workspace_dir = dir.path().to_path_buf();
            config.config_path = dir.path().join("config.toml");
            let generation = crate::config::new_shared(config).pin();
            let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(dir.path()).unwrap());
            let fabric = MemoryFabric::new(memory.clone(), dir.path().to_string_lossy());
            let envelope = RuntimeEnvelope::gateway(
                fabric.workspace_id(),
                "prx_session",
                "webhook",
                "prx_user",
                "prx_service",
                MemoryVisibility::Session,
            )
            .with_run_id("prx_run")
            .with_config_generation(&generation);
            let input = fabric
                .record_inbound_user_message(envelope.message_scope(), "prx input", None, None)
                .await
                .unwrap();
            let receipt = GatewaySourceReceipt::issue(memory.as_ref(), &generation, &envelope, "prx input", &input)
                .await
                .unwrap();
            let tracker = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
            let ticket = tracker.register(tokio_util::sync::CancellationToken::new()).unwrap();
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let guard = GatewayAdmissionGuard {
                receipt: Some(receipt),
                generation,
                fabric: fabric.clone(),
                ticket: Some(ticket),
                acknowledgement: Some(sender),
                pending: None,
            };
            if drop_before_decision {
                drop(guard);
            } else {
                let (ticket, acknowledgement) = guard.into_terminal_parts().unwrap();
                ticket.finish(true);
                acknowledgement.unwrap().send(Ok(())).unwrap();
            }
            tokio::time::timeout(std::time::Duration::from_secs(5), receiver)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            tracker
                .drain(tokio::time::Instant::now() + std::time::Duration::from_secs(5))
                .await
                .unwrap();
            let event = memory
                .find_message_event_by_idempotency_key(fabric.workspace_id(), "turn:prx_run:admission-failed")
                .await
                .unwrap();
            assert_eq!(event.is_some(), drop_before_decision);
            if let Some(event) = event {
                assert!(event.id > 0);
                assert_eq!(event.causation_event_id.as_deref(), Some(input.event_id.as_str()));
                let payload: serde_json::Value =
                    serde_json::from_str(event.raw_payload_json.as_deref().unwrap()).unwrap();
                assert_eq!(payload.get("status").expect("test: terminal field"), "cancelled");
            }
        }
    }
    struct CommitThenSuspend {
        inner: Arc<dyn Memory>,
        suspend: std::sync::atomic::AtomicBool,
        committed: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl Memory for CommitThenSuspend {
        fn name(&self) -> &str {
            "prx_commit_then_suspend"
        }
        async fn store(
            &self,
            key: &str,
            content: &str,
            category: crate::memory::MemoryCategory,
            session: Option<&str>,
        ) -> anyhow::Result<()> {
            self.inner.store(key, content, category, session).await
        }
        async fn recall(
            &self,
            query: &str,
            limit: usize,
            session: Option<&str>,
        ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
            self.inner.recall(query, limit, session).await
        }
        async fn get(&self, key: &str) -> anyhow::Result<Option<crate::memory::MemoryEntry>> {
            self.inner.get(key).await
        }
        async fn list(
            &self,
            category: Option<&crate::memory::MemoryCategory>,
            session: Option<&str>,
        ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
            self.inner.list(category, session).await
        }
        async fn forget(&self, key: &str) -> anyhow::Result<bool> {
            self.inner.forget(key).await
        }
        async fn count(&self) -> anyhow::Result<usize> {
            self.inner.count().await
        }
        async fn health_check(&self) -> bool {
            self.inner.health_check().await
        }
        async fn find_owned_message_event(
            &self,
            workspace: &str,
            owner: &str,
            id: &str,
        ) -> anyhow::Result<Option<crate::memory::MessageEvent>> {
            self.inner.find_owned_message_event(workspace, owner, id).await
        }
        async fn append_message_event(
            &self,
            input: crate::memory::MessageEventInput,
        ) -> anyhow::Result<crate::memory::MessageEvent> {
            let admission = input.event_type == "turn.admission.failed";
            let event = self.inner.append_message_event(input).await?;
            if admission && self.suspend.swap(false, std::sync::atomic::Ordering::SeqCst) {
                self.committed.notify_one();
                std::future::pending::<()>().await;
            }
            Ok(event)
        }
    }

    #[tokio::test]
    async fn interrupted_failure_write_replays_original_failure_after_commit() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.workspace_dir = dir.path().to_path_buf();
        config.config_path = dir.path().join("config.toml");
        let generation = crate::config::new_shared(config).pin();
        let sqlite: Arc<dyn Memory> = Arc::new(SqliteMemory::new(dir.path()).unwrap());
        let backend = Arc::new(CommitThenSuspend {
            inner: sqlite.clone(),
            suspend: std::sync::atomic::AtomicBool::new(true),
            committed: tokio::sync::Notify::new(),
        });
        let fabric = MemoryFabric::new(backend.clone(), dir.path().to_string_lossy());
        let envelope = RuntimeEnvelope::gateway(
            fabric.workspace_id(),
            "prx_session",
            "webhook",
            "prx_user",
            "prx_service",
            MemoryVisibility::Session,
        )
        .with_run_id("prx_run")
        .with_config_generation(&generation);
        let input = fabric
            .record_inbound_user_message(envelope.message_scope(), "prx input", None, None)
            .await
            .unwrap();
        let receipt = GatewaySourceReceipt::issue(backend.as_ref(), &generation, &envelope, "prx input", &input)
            .await
            .unwrap();
        let tracker = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
        let ticket = tracker.register(tokio_util::sync::CancellationToken::new()).unwrap();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let mut guard = GatewayAdmissionGuard {
            receipt: Some(receipt),
            generation,
            fabric: fabric.clone(),
            ticket: Some(ticket),
            acknowledgement: Some(sender),
            pending: None,
        };
        {
            let write = guard.finish_failure(false, "prx original failure");
            tokio::pin!(write);
            tokio::select! {
                result = &mut write => panic!("write unexpectedly returned: {result:?}"),
                () = backend.committed.notified() => {},
                () = tokio::time::sleep(std::time::Duration::from_secs(5)) => panic!("write did not reach commit barrier"),
            }
        }
        let before = sqlite
            .find_message_event_by_idempotency_key(fabric.workspace_id(), "turn:prx_run:admission-failed")
            .await
            .unwrap()
            .unwrap();
        drop(guard);
        tokio::time::timeout(std::time::Duration::from_secs(5), receiver)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        tracker
            .drain(tokio::time::Instant::now() + std::time::Duration::from_secs(5))
            .await
            .unwrap();
        let after = sqlite
            .find_message_event_by_idempotency_key(fabric.workspace_id(), "turn:prx_run:admission-failed")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(before.event_id, after.event_id);
        assert_eq!(before.raw_payload_json, after.raw_payload_json);
        let payload: serde_json::Value = serde_json::from_str(after.raw_payload_json.as_deref().unwrap()).unwrap();
        assert_eq!(payload.get("status").expect("test: terminal field"), "failed");
        assert_eq!(
            payload.get("summary").expect("test: terminal field"),
            "prx original failure"
        );
    }
}
