#![cfg(test)]
//! Actual admission guard and job monitor ownership before the work's first poll.
use super::*;
use crate::gateway::jobs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

struct BlockedAdmissionMemory {
    inner: Arc<dyn Memory>,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
    fail: bool,
}
#[async_trait::async_trait]
impl Memory for BlockedAdmissionMemory {
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
        if input.event_type == "turn.admission.failed" {
            self.started.notify_one();
            self.release.notified().await;
            anyhow::ensure!(!self.fail, "controlled admission persistence failure");
        }
        self.inner.append_message_event(input).await
    }
}

#[tokio::test(flavor = "current_thread")]
async fn pre_poll_abort_waits_for_actual_admission_cleanup_and_surfaces_failure() {
    for fail in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.workspace_dir = dir.path().to_path_buf();
        config.config_path = dir.path().join("config.toml");
        let generation = crate::config::new_shared(config).pin();
        let inner: Arc<dyn Memory> = Arc::new(SqliteMemory::new(dir.path()).unwrap());
        let memory = Arc::new(BlockedAdmissionMemory {
            inner: Arc::clone(&inner),
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            fail,
        });
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
        assert!(input.id > 0, "the input precedes job submission durably");
        let tracker = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
        let ticket = tracker.register(tokio_util::sync::CancellationToken::new()).unwrap();
        let mut guard = GatewayAdmissionGuard {
            receipt: Some(receipt),
            generation,
            fabric: fabric.clone(),
            ticket: Some(ticket),
            acknowledgement: None,
            pending: None,
        };
        let (sender, receiver) = tokio::sync::oneshot::channel();
        guard.attach_acknowledgement(sender).unwrap();
        let polls = Arc::new(AtomicUsize::new(0));
        let work_polls = Arc::clone(&polls);
        let handle = jobs::submit_with_cleanup(
            jobs::KIND_SESSION_MESSAGE,
            "prx pre-poll admission",
            async move {
                let _guard = guard;
                work_polls.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<Result<jobs::JobOutput, String>>().await
            },
            Some(receiver),
        );
        let accepted = handle.accepted_body();
        let id = uuid::Uuid::parse_str(accepted.get("job_id").expect("accepted job id").as_str().unwrap()).unwrap();
        let work_id = crate::runtime::registry::WorkId::parse(
            accepted.get("work_id").expect("accepted work id").as_str().unwrap(),
        )
        .unwrap();
        // current_thread has not yielded since submit. kill issues cancellation
        // and abort synchronously before its first verification await.
        tokio::time::timeout(Duration::from_secs(5), crate::runtime::registry::kill(work_id, false))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), memory.started.notified())
            .await
            .unwrap();
        assert_eq!(polls.load(Ordering::SeqCst), 0, "work must never have polled");
        let mut waiter = Box::pin(handle.wait());
        let early = tokio::time::timeout(Duration::from_millis(20), waiter.as_mut()).await;
        assert!(
            early.is_err(),
            "blocked durable cleanup cannot publish a terminal job: {early:?}"
        );
        assert_eq!(jobs::snapshot(id).unwrap().status, "running");
        assert!(
            inner
                .find_message_event_by_idempotency_key(fabric.workspace_id(), "turn:prx_run:admission-failed")
                .await
                .unwrap()
                .is_none()
        );
        memory.release.notify_one();
        let outcome = tokio::time::timeout(Duration::from_secs(5), waiter).await.unwrap();
        if fail {
            assert!(
                matches!(outcome, jobs::JobOutcome::Failed(ref error) if error.contains("persistence is unconfirmed")),
                "{outcome:?}"
            );
            assert_eq!(jobs::snapshot(id).unwrap().status, "failed");
        } else {
            assert!(matches!(outcome, jobs::JobOutcome::Cancelled), "{outcome:?}");
            assert_eq!(jobs::snapshot(id).unwrap().status, "cancelled");
        }
        let drained = tracker
            .drain(tokio::time::Instant::now() + Duration::from_secs(5))
            .await;
        assert_eq!(drained.is_err(), fail);
        let terminal = inner
            .find_message_event_by_idempotency_key(fabric.workspace_id(), "turn:prx_run:admission-failed")
            .await
            .unwrap();
        assert_eq!(terminal.is_none(), fail);
        if let Some(terminal) = terminal {
            assert!(terminal.id > input.id);
            assert_eq!(terminal.causation_event_id.as_deref(), Some(input.event_id.as_str()));
        }
    }
}
