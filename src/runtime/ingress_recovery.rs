//! Recovery consumes authenticated durable reception, never execution authority.
use crate::decision::key::{DecisionKey, KeyDomain};
use crate::memory::ingress_recovery::{
    IngressAppendOutcome, IngressCapsule, IngressEventLookup, IngressLifetime, IngressRecoveryCommit,
    IngressRecoveryScan, IngressRecoveryScope, IngressRecoveryState, MAX_CAPSULE_BYTES, MAX_RECOVERY_EVENT_BYTES,
    MAX_RECOVERY_PAGE, input_within_bounds, remaining, serialized_within,
};
use crate::memory::recovery_authority::{RecoveryMacDomain, RecoveryTag};
use crate::memory::{Memory, MemoryVisibility, MessageEvent, MessageEventInput};
use anyhow::Context;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Pinned recovery authority. A decision store supplies historical evidence only;
/// reception and interruption recovery do not require a decision service.
#[derive(Clone)]
pub(crate) struct RecoveryScope {
    pub(super) memory: Arc<dyn Memory>,
    pub(super) workspace: std::path::PathBuf,
    pub(super) recordable: bool,
    pub(super) durable_key: bool,
    pub(super) surface: crate::causal_tree::state::TurnSurfaceKind,
    pub(super) state_key: Arc<DecisionKey>,
    pub(super) store: Option<Arc<crate::causal_tree::store::CausalDecisionStore>>,
    pub(super) parent_authority_id: Option<crate::tools::capability::child::ParentAuthorityId>,
    pub(super) durable_events: Option<Arc<super::causal_events::DurableCausalEventSink>>,
}

impl RecoveryScope {
    pub(super) const fn unrecorded(
        memory: Arc<dyn Memory>,
        workspace: std::path::PathBuf,
        surface: crate::causal_tree::state::TurnSurfaceKind,
        state_key: Arc<DecisionKey>,
    ) -> Self {
        Self {
            memory,
            workspace,
            recordable: false,
            durable_key: false,
            surface,
            state_key,
            store: None,
            parent_authority_id: None,
            durable_events: None,
        }
    }

    /// Initialize durable recovery from pinned runtime inputs, without building
    /// a decision engine, catalog, or execution service. The caller supplies its
    /// single key-preparation result; missing authority never triggers a retry.
    pub(crate) async fn prepare(
        generation: &crate::config::ConfigGeneration,
        memory: Arc<dyn Memory>,
        surface: crate::causal_tree::state::TurnSurfaceKind,
        observer: Arc<dyn crate::decision::telemetry::CausalEventSink>,
        parent_grant: Option<&super::child_source::IssuedChildGrant>,
        owner: &Arc<super::turn_cleanup::CleanupTracker>,
        workspace_key: Option<Arc<DecisionKey>>,
    ) -> anyhow::Result<Arc<Self>> {
        let config = &generation.effective;
        let recording = config.memory.event_recording_config();
        let recordable = config.causal_tree.enabled
            && workspace_key.is_some()
            && crate::memory::serves_tool_execution_ledger(memory.name())
            && recording.enabled
            && recording.record_user_messages;
        let mut scope = Self {
            memory: Arc::clone(&memory),
            workspace: config.workspace_dir.clone(),
            recordable,
            durable_key: false,
            surface,
            state_key: workspace_key.unwrap_or_else(DecisionKey::ephemeral),
            store: None,
            parent_authority_id: parent_grant.map(|grant| grant.scope().gate_inputs().0.parent_authority_id),
            durable_events: None,
        };
        if recordable {
            scope.durable_key = true;
            let events = Arc::new(super::causal_events::DurableCausalEventSink::new(
                generation, memory, observer,
            )?);
            let ticket = owner.register(tokio_util::sync::CancellationToken::new())?;
            let prepared_events = Arc::clone(&events);
            let store = crate::causal_tree::store_preparation::open_store(
                config.workspace_dir.clone(),
                crate::causal_tree::store::Retention::days(
                    config.causal_tree.store.retention_days,
                    config.self_system.fitness.retention_days,
                ),
                events.clone(),
                Some(crate::causal_tree::store::StoreMode::ReadWrite),
                move |result| {
                    ticket.retain_prepared_store(result.as_ref().ok().cloned(), Some(prepared_events));
                    // A failed store open also fails cleanup so the owner cannot report a clean drain.
                    ticket.finish(result.is_ok());
                },
            )
            .await?;
            if let Some(namespace) = store.workspace_id() {
                events.bind_store_namespace(namespace)?;
            }
            scope.store = Some(store);
            scope.durable_events = Some(events);
        }
        let scope = Arc::new(scope);
        owner.retain_recovery(Arc::clone(&scope))?;
        recovery_ready(Arc::clone(&scope)).await?;
        Ok(scope)
    }

    pub(super) fn owns_prepared_queues(
        &self,
        store: Option<&Arc<crate::causal_tree::store::CausalDecisionStore>>,
        events: Option<&Arc<super::causal_events::DurableCausalEventSink>>,
    ) -> bool {
        matches!((&self.store, store), (Some(left), Some(right)) if Arc::ptr_eq(left, right))
            && match (&self.durable_events, events) {
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                (None, None) => true,
                _ => false,
            }
    }

    pub(crate) async fn drain(&self, deadline: Instant) -> bool {
        let mut drained = true;
        if let Some(writer) = self.store.as_ref().and_then(|store| store.writer()) {
            #[cfg(test)]
            super::turn_cleanup::diagnostic::queue_presence("decision_writer_present", true);
            let writer_drained = writer.drain(deadline).await;
            #[cfg(test)]
            super::turn_cleanup::diagnostic::queue("decision_writer_drained", writer_drained);
            drained &= writer_drained;
        }
        #[cfg(test)]
        super::turn_cleanup::diagnostic::queue_presence("event_queue_present", self.durable_events.is_some());
        if let Some(events) = &self.durable_events {
            let events_drained = events.drain(deadline).await;
            #[cfg(test)]
            super::turn_cleanup::diagnostic::queue("event_queue_drained", events_drained);
            drained &= events_drained;
        }
        drained
    }

    pub(crate) const fn ingress_recovery_enabled(&self) -> bool {
        self.recordable && self.durable_key
    }

    pub(crate) fn accepts_ingress_recovery(
        &self,
        memory: &Arc<dyn Memory>,
        workspace: &str,
        surface: crate::causal_tree::state::TurnSurfaceKind,
    ) -> bool {
        Arc::ptr_eq(memory, &self.memory)
            && self.workspace.to_string_lossy() == workspace
            && self.surface == surface
            && self.ingress_recovery_enabled()
    }

    /// Reads only the pinned memory/workspace and authenticates the owner.
    /// It grants no execution authority.
    pub(crate) async fn rejoin_terminal(
        &self,
        terminal_id: &str,
        owner_id: &str,
    ) -> anyhow::Result<crate::causal_tree::store::Enqueue> {
        self.rejoin_terminal_inner(terminal_id, owner_id, None).await
    }

    pub(crate) async fn rejoin_ingress_terminal(
        &self,
        capsule: &IngressCapsule,
        deadline: Instant,
        owner: &Arc<super::turn_cleanup::CleanupTracker>,
    ) -> anyhow::Result<crate::causal_tree::store::Enqueue> {
        anyhow::ensure!(
            capsule.scope.workspace_id == self.workspace.to_string_lossy(),
            "recovery service workspace differs from capsule"
        );
        self.rejoin_terminal_inner(
            &capsule.run_id,
            &capsule.scope.owner_id,
            Some((&capsule.scope, deadline, owner)),
        )
        .await
    }
}

#[derive(Debug, thiserror::Error)]
#[error("input already accepted; execution was not repeated and its outcome remains unconfirmed")]
pub(crate) struct AlreadyAccepted;

/// Store/run metadata for diagnostics only; recovery classification is unchanged.
#[derive(Debug)]
pub(crate) struct IngressRecoveryDiagnostic {
    pub(crate) run_id: String,
    pub(crate) memory_store: std::path::PathBuf,
    pub(crate) decision_store: std::path::PathBuf,
}

impl std::fmt::Display for IngressRecoveryDiagnostic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "ingress recovery run={} memory_store={} decision_store={}",
            self.run_id,
            self.memory_store.display(),
            self.decision_store.display()
        )
    }
}

impl std::error::Error for IngressRecoveryDiagnostic {}

pub(crate) struct IngressRecorder {
    memory: Arc<dyn Memory>,
    lifetime: Arc<dyn IngressLifetime>,
    deadline: Instant,
    owner: Arc<super::turn_cleanup::CleanupTracker>,
    duplicate: std::sync::atomic::AtomicBool,
    workspace: std::path::PathBuf,
    run_id: std::sync::OnceLock<String>,
    warned_markers: parking_lot::Mutex<std::collections::HashSet<String>>,
    barrier: Arc<tokio::sync::Mutex<()>>,
    source: std::sync::OnceLock<(IngressRecoveryScope, crate::causal_tree::state::TurnSurfaceKind)>,
}

impl IngressRecorder {
    pub(crate) async fn prepare(
        memory: Arc<dyn Memory>,
        config: &crate::config::Config,
        ticket: &super::turn_cleanup::CleanupTicket,
    ) -> anyhow::Result<Option<Arc<Self>>> {
        let recording = config.memory.event_recording_config();
        if !config.causal_tree.enabled || !recording.enabled || !recording.record_user_messages {
            return Ok(None);
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        let workspace_path = config.workspace_dir.clone();
        let prepared_memory = Arc::clone(&memory);
        owned_store_operation(&ticket.owner(), move || {
            remaining(deadline)?;
            if let Some(repository) = prepared_memory.recovery_repository() {
                repository.activate_recovery_authority()?;
            } else {
                DecisionKey::load_or_create(&workspace_path)?;
            }
            remaining(deadline)?;
            Ok(())
        })
        .await?;
        let workspace = config.workspace_dir.to_string_lossy().into_owned();
        let lifetime = memory
            .acquire_ingress_lifetime(&workspace, &uuid::Uuid::new_v4().to_string(), deadline)
            .await?;
        ticket.retain_ingress_lifetime(format!("{}:{workspace}", lifetime.namespace()), lifetime.clone())?;
        lifetime.validate(deadline).await?;
        let barrier = recovery_gate(&lifetime)?;
        Ok(Some(Arc::new(Self {
            memory,
            lifetime,
            deadline,
            owner: ticket.owner(),
            duplicate: Default::default(),
            workspace: config.workspace_dir.clone(),
            run_id: Default::default(),
            warned_markers: Default::default(),
            barrier,
            source: Default::default(),
        })))
    }
    pub(crate) fn verify_receipt(&self, event: &MessageEvent) -> anyhow::Result<()> {
        let capsule = capsule_from_event(event)?;
        self.verify(&capsule, event).map(|_| ())
    }
    pub(crate) fn duplicate(&self) -> bool {
        self.duplicate.load(std::sync::atomic::Ordering::Acquire)
    }
    fn input_digest(&self, input: &MessageEventInput) -> anyhow::Result<(Option<String>, String)> {
        if let Some(repository) = self.memory.recovery_repository() {
            input_within_bounds(input, MAX_RECOVERY_EVENT_BYTES - MAX_CAPSULE_BYTES)?;
            let tag = repository.recovery_mac(None, RecoveryMacDomain::InputDigestV3, &serde_json::to_vec(input)?)?;
            return Ok((Some(tag.key_id), hex::encode(tag.tag)));
        }
        Ok((
            None,
            hex::encode(self.legacy_key()?.hmac(
                KeyDomain::IngressRecovery,
                &[b"input-digest-v2", digest(input)?.as_bytes()],
            )),
        ))
    }

    fn sign(&self, capsule: &IngressCapsule) -> anyhow::Result<String> {
        serialized_within(capsule, MAX_CAPSULE_BYTES - 64)?;
        let mut unsigned = capsule.clone();
        unsigned.authentication.clear();
        Ok(hex::encode(self.slot_mac(
            capsule,
            RecoveryMacDomain::CapsuleV3,
            &[&serde_json::to_vec(&unsigned)?],
            &serde_json::to_vec(&unsigned)?,
        )?))
    }
    fn verify(&self, capsule: &IngressCapsule, input: &MessageEvent) -> anyhow::Result<MessageEventInput> {
        capsule.validate_bounds()?;
        anyhow::ensure!(
            capsule.backend_namespace == self.lifetime.namespace(),
            "ingress capsule belongs to another backend"
        );
        let mut unsigned = capsule.clone();
        unsigned.authentication.clear();
        let tag = hex::decode(&capsule.authentication)?;
        anyhow::ensure!(
            self.verify_slot(
                capsule,
                RecoveryMacDomain::CapsuleV3,
                &[&serde_json::to_vec(&unsigned)?],
                &serde_json::to_vec(&unsigned)?,
                &tag
            )?,
            "invalid ingress capsule authentication"
        );
        let normalized = original_input(input, capsule)?;
        let digest_matches = if capsule.version == 1 {
            anyhow::ensure!(
                !self.memory.requires_scoped_execution(),
                "legacy unkeyed ingress capsule cannot be recovered in a protected scope"
            );
            digest(&normalized)? == capsule.input_digest
        } else {
            self.verify_slot(
                capsule,
                RecoveryMacDomain::InputDigestV3,
                &[b"input-digest-v2", digest(&normalized)?.as_bytes()],
                &serde_json::to_vec(&normalized)?,
                &hex::decode(&capsule.input_digest)?,
            )?
        };
        anyhow::ensure!(digest_matches, "ingress input digest differs from its capsule");
        Ok(normalized)
    }
    pub(crate) async fn append(self: &Arc<Self>, mut input: MessageEventInput) -> anyhow::Result<MessageEvent> {
        let _ = self
            .run_id
            .set(input.run_id.clone().context("durable ingress requires run identity")?);
        remaining(self.deadline)?;
        input_within_bounds(&input, MAX_RECOVERY_EVENT_BYTES - MAX_CAPSULE_BYTES)?;
        let attempted_digest = transport_digest(&input)?;
        let scope = IngressRecoveryScope {
            workspace_id: input.workspace_id.clone(),
            owner_id: input.owner_id.clone().context("durable ingress owner missing")?,
            source: input.source.clone(),
        };
        let surface =
            super::turn_source::ingress_event_surface(&input.source, input.channel.as_deref(), &input.visibility)
                .context("unsupported durable ingress surface")?;
        self.source
            .set((scope.clone(), surface))
            .map_err(|_| anyhow::anyhow!("ingress recorder already owns an input"))?;
        register_recorder(self)?;
        self.boxed_recover(&scope, None, self.deadline).await?;
        input.event_id = Some(uuid::Uuid::new_v4().to_string());
        let (key_id, input_digest) = self.input_digest(&input)?;
        let mut capsule = IngressCapsule {
            version: if key_id.is_some() { 3 } else { 2 },
            key_id,
            backend_namespace: self.lifetime.namespace().to_owned(),
            scope,
            lifetime: self.lifetime.token().to_owned(),
            input_event_id: input.event_id.clone().context("ingress event identity missing")?,
            input_key: input
                .idempotency_key
                .clone()
                .context("durable ingress requires stable input key")?,
            run_id: input.run_id.clone().context("durable ingress requires run identity")?,
            input_digest,
            authentication: String::new(),
        };
        capsule.authentication = self.sign(&capsule)?;
        capsule.validate_bounds()?;
        #[derive(serde::Serialize)]
        struct Payload<'a> {
            prx_ingress_capsule: &'a IngressCapsule,
            original_payload_json: Option<String>,
        }
        input.raw_payload_json = Some(serde_json::to_string(&Payload {
            prx_ingress_capsule: &capsule,
            original_payload_json: input.raw_payload_json.take(),
        })?);
        match self
            .memory
            .append_ingress_input(input, capsule.clone(), self.lifetime.clone(), self.deadline)
            .await?
        {
            IngressAppendOutcome::Inserted(event) => {
                self.verify(&capsule, &event)?;
                remaining(self.deadline)?;
                Ok(event)
            }
            IngressAppendOutcome::AlreadyAccepted(event) => {
                self.duplicate.store(true, std::sync::atomic::Ordering::Release);
                // A transport retry may have a fresh run UUID. Authenticate the
                // OLD capsule first; compare stable transport content separately.
                let old = capsule_from_event(&event)?;
                let previous = self.verify(&old, &event)?;
                anyhow::ensure!(
                    transport_digest(&previous)? == attempted_digest,
                    "ingress stable key conflicts with a different input"
                );
                Err(AlreadyAccepted.into())
            }
        }
    }
    async fn recover(
        &self,
        scope: &IngressRecoveryScope,
        recovery: Option<&RecoveryScope>,
        deadline: Instant,
    ) -> anyhow::Result<()> {
        let _barrier = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), self.barrier.lock())
            .await
            .context("ingress recovery barrier deadline elapsed")?;
        let mut cursor = 0;
        loop {
            let candidates = self
                .memory
                .list_ingress_recovery_candidates(
                    scope,
                    self.lifetime.clone(),
                    if recovery.is_some() {
                        IngressRecoveryScan::IncludeUnconfirmed
                    } else {
                        IngressRecoveryScan::PendingOnly
                    },
                    cursor,
                    MAX_RECOVERY_PAGE,
                    deadline,
                )
                .await?;
            remaining(deadline)?;
            if candidates.is_empty() {
                return Ok(());
            }
            for candidate in candidates {
                anyhow::ensure!(
                    candidate.cursor > cursor && candidate.capsule.scope == *scope,
                    "ingress recovery cursor or owner changed"
                );
                cursor = candidate.cursor;
                let capsule = candidate.capsule;
                let recovered = async {
                    if capsule.lifetime == self.lifetime.token() {
                        return Ok(());
                    }
                    if self.boxed_quarantine_marker(scope, &capsule).await? {
                        return Ok(());
                    }
                    let Some(claim) = self
                        .memory
                        .claim_ingress_recovery(&scope.workspace_id, &capsule.lifetime, deadline)
                        .await?
                    else {
                        anyhow::bail!("another ingress lifetime remains active; recovery is not ready");
                    };
                    let event = self
                        .memory
                        .read_ingress_event(
                            scope,
                            &IngressEventLookup::EventId(capsule.input_event_id.clone()),
                            deadline,
                        )
                        .await?
                        .context("durable ingress input is missing")?;
                    let verified = match self.verify(&capsule, &event) {
                        Ok(input) => input,
                        Err(error) if unverifiable_ingress(&error).is_some() => {
                            self.boxed_record_quarantine(scope, &capsule, unverifiable_ingress(&error).unwrap_or("unverifiable ingress"))
                                .await?;
                            return Ok(());
                        }
                        Err(error) => return Err(error),
                    };
                    let final_event = if recovery.is_some() {
                        self.memory
                            .read_ingress_event(
                                scope,
                                &IngressEventLookup::IdempotencyKey(format!("turn:{}:final", capsule.run_id)),
                                deadline,
                            )
                            .await?
                    } else {
                        None
                    };
                    let state = if let (Some(recovery), Some(_)) = (recovery, final_event.as_ref()) {
                        match recovery.rejoin_ingress_terminal(&capsule, deadline, &self.owner).await {
                            Ok(_) => IngressRecoveryState::Reconciled,
                            Err(error) if unverifiable_ingress(&error).is_some() => {
                                self.boxed_record_quarantine(scope, &capsule, unverifiable_ingress(&error).unwrap_or("unverifiable terminal"))
                                    .await?;
                                return Ok(());
                            }
                            Err(error) => return Err(error),
                        }
                    } else {
                        IngressRecoveryState::Unconfirmed
                    };
                    let mut evidence = verified;
                    let state_label = if state == IngressRecoveryState::Reconciled {
                        "reconciled"
                    } else {
                        "unconfirmed"
                    };
                    // Absence of a verified final is never interpreted as no charge
                    // or no provider execution. A late final remains replayable by
                    // the existing signed terminal verifier.
                    evidence.event_id = None;
                    evidence.causation_event_id = Some(capsule.input_event_id.clone());
                    evidence.idempotency_key = Some(format!("ingress:{}:{state_label}", capsule.input_event_id));
                    evidence.role = "event".into();
                    evidence.event_type = if state == IngressRecoveryState::Reconciled {
                        "turn.ingress.reconciled"
                    } else {
                        "turn.ingress.interrupted"
                    }
                    .into();
                    evidence.content = if state == IngressRecoveryState::Reconciled {
                        "turn.ingress.reconciled authenticated final evidence rejoined"
                    } else {
                        "turn.ingress.interrupted outcome and usage remain unconfirmed"
                    }
                    .into();
                    let authentication = hex::encode(self.slot_mac(
                        &capsule, RecoveryMacDomain::StateV3,
                        &[
                            b"recovery-state",
                            state_label.as_bytes(),
                            &serde_json::to_vec(&capsule)?,
                        ],
                        &serde_json::to_vec(&(state_label, &capsule))?,
                    )?);
                    evidence.raw_payload_json = Some(serde_json::to_string(
                        &serde_json::json!({"ingress_recovery":{"capsule":capsule,"state":state_label,"evidence_authentication":authentication}}),
                    )?);
                    let expected = evidence.clone();
                    let recorded = self
                        .memory
                        .commit_ingress_recovery(IngressRecoveryCommit {
                            capsule: capsule.clone(),
                            state,
                            evidence,
                            claim,
                            deadline,
                        })
                        .await?;
                    anyhow::ensure!(
                        recorded.idempotency_key == expected.idempotency_key
                            && recorded.content == expected.content
                            && recorded.raw_payload_json == expected.raw_payload_json,
                        "interruption evidence receipt differs from proposal"
                    );
                    let stored = self
                        .memory
                        .read_ingress_event(scope, &IngressEventLookup::EventId(recorded.event_id.clone()), deadline)
                        .await?
                        .context("interruption evidence readback missing")?;
                    anyhow::ensure!(
                        serde_json::to_value(stored)? == serde_json::to_value(recorded)?,
                        "interruption evidence is not durably confirmed"
                    );
                    Ok(())
                }.await;
                recovered.with_context(|| self.diagnostic(&capsule.run_id))?;
            }
        }
    }

    // `append` and service startup await recovery. Storing that state machine
    // in those futures overflows the test thread stack.
    #[inline(never)]
    fn boxed_recover<'a>(
        &'a self,
        scope: &'a IngressRecoveryScope,
        recovery: Option<&'a RecoveryScope>,
        deadline: Instant,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            self.recover(scope, recovery, deadline).await.map_err(|error| {
                if error.downcast_ref::<IngressRecoveryDiagnostic>().is_some() {
                    error
                } else {
                    error.context(self.diagnostic(self.run_id.get().map(String::as_str).unwrap_or("unassigned")))
                }
            })
        })
    }

    // The marker futures stay out of `recover` for the same reason.
    #[inline(never)]
    fn boxed_quarantine_marker<'a>(
        &'a self,
        scope: &'a IngressRecoveryScope,
        capsule: &'a IngressCapsule,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<bool>> + Send + 'a>> {
        Box::pin(self.quarantine_marker_present(scope, capsule))
    }

    #[inline(never)]
    fn boxed_record_quarantine<'a>(
        &'a self,
        scope: &'a IngressRecoveryScope,
        capsule: &'a IngressCapsule,
        reason: &'static str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(self.record_quarantine(scope, capsule, reason))
    }

    fn quarantine_key(capsule: &IngressCapsule) -> String {
        format!("ingress:{}:quarantine", capsule.input_event_id)
    }

    fn diagnostic(&self, run: &str) -> IngressRecoveryDiagnostic {
        IngressRecoveryDiagnostic {
            run_id: run.to_owned(),
            memory_store: self.workspace.join("memory/brain.db"),
            decision_store: crate::causal_tree::store::db_path(&self.workspace),
        }
    }

    fn authenticated_quarantine_key(&self, capsule: &IngressCapsule) -> anyhow::Result<String> {
        let tag = self.slot_mac(
            capsule,
            RecoveryMacDomain::QuarantineKeyV3,
            &[b"quarantine-key-v1", &serde_json::to_vec(capsule)?],
            &serde_json::to_vec(capsule)?,
        )?;
        Ok(format!("{}:{}", Self::quarantine_key(capsule), hex::encode(tag)))
    }

    fn verify_quarantine_marker(&self, event: &MessageEvent, capsule: &IngressCapsule) -> anyhow::Result<bool> {
        if event.event_type != "turn.ingress.quarantined"
            || event.run_id.as_deref() != Some(capsule.run_id.as_str())
            || event.causation_event_id.as_deref() != Some(capsule.input_event_id.as_str())
        {
            return Ok(false);
        }
        let Some(raw) = event.raw_payload_json.as_deref() else {
            return Ok(false);
        };
        let Ok(mut payload) = serde_json::from_str::<serde_json::Value>(raw) else {
            return Ok(false);
        };
        let Some(object) = payload.as_object_mut() else {
            return Ok(false);
        };
        let Some(tag) = object
            .remove("authentication")
            .and_then(|tag| tag.as_str().map(str::to_owned))
        else {
            return Ok(false);
        };
        let Ok(tag) = hex::decode(tag) else { return Ok(false) };
        Ok(payload.get("capsule") == Some(&serde_json::to_value(capsule)?)
            && payload.get("reason").and_then(serde_json::Value::as_str).is_some()
            && self.verify_slot(
                capsule,
                RecoveryMacDomain::QuarantineV3,
                &[b"quarantine-v1", &serde_json::to_vec(&payload)?],
                &serde_json::to_vec(&payload)?,
                &tag,
            )?)
    }

    async fn quarantine_marker_present(
        &self,
        scope: &IngressRecoveryScope,
        capsule: &IngressCapsule,
    ) -> anyhow::Result<bool> {
        for key in [
            Self::quarantine_key(capsule),
            self.authenticated_quarantine_key(capsule)?,
        ] {
            if let Some(event) = self
                .memory
                .find_owned_message_event_by_idempotency_key(&scope.workspace_id, &scope.owner_id, &key)
                .await?
            {
                if self.verify_quarantine_marker(&event, capsule)? {
                    return Ok(true);
                }
                let should_warn = self.warned_markers.lock().insert(event.event_id.clone());
                if should_warn {
                    tracing::warn!(run_id = %capsule.run_id,
                        marker_id = %event.event_id,
                        brain_db = %self.workspace.join("memory/brain.db").display(),
                        decisions_db = %crate::causal_tree::store::db_path(&self.workspace).display(),
                        "ignored unauthenticated ingress quarantine marker; recovery will proceed; remove the unauthenticated marker row or rerun ingress recovery");
                }
            }
        }
        Ok(false)
    }

    /// Leave the unverifiable row in place and remember that it must not block
    /// a later turn. The marker carries no user content and no key material.
    async fn record_quarantine(
        &self,
        scope: &IngressRecoveryScope,
        capsule: &IngressCapsule,
        reason: &'static str,
    ) -> anyhow::Result<()> {
        if self.quarantine_marker_present(scope, capsule).await? {
            return Ok(());
        }
        let primary = Self::quarantine_key(capsule);
        let key = if self
            .memory
            .find_owned_message_event_by_idempotency_key(&scope.workspace_id, &scope.owner_id, &primary)
            .await?
            .is_some()
        {
            self.authenticated_quarantine_key(capsule)?
        } else {
            primary
        };
        let mut payload = serde_json::json!({"capsule": capsule, "reason": reason});
        let authentication = hex::encode(self.slot_mac(
            capsule,
            RecoveryMacDomain::QuarantineV3,
            &[b"quarantine-v1", &serde_json::to_vec(&payload)?],
            &serde_json::to_vec(&payload)?,
        )?);
        payload
            .as_object_mut()
            .context("quarantine payload is not an object")?
            .insert("authentication".into(), authentication.into());
        let recorded = self
            .memory
            .append_message_event(MessageEventInput {
                event_id: None,
                idempotency_key: Some(key.clone()),
                workspace_id: scope.workspace_id.clone(),
                owner_id: Some(scope.owner_id.clone()),
                source: scope.source.clone(),
                channel: None,
                session_key: None,
                parent_session_key: None,
                run_id: Some(capsule.run_id.clone()),
                parent_run_id: None,
                agent_id: None,
                persona_id: None,
                sender: None,
                recipient: None,
                role: "event".to_owned(),
                event_type: "turn.ingress.quarantined".to_owned(),
                subject: None,
                goal_id: None,
                causation_event_id: Some(capsule.input_event_id.clone()),
                correlation_id: None,
                attempt_id: None,
                lease_epoch: None,
                config_generation_id: None,
                config_source_revision: None,
                content: format!("turn.ingress.quarantined {reason}"),
                raw_payload_json: Some(serde_json::to_string(&payload)?),
                visibility: MemoryVisibility::System,
            })
            .await?;
        anyhow::ensure!(
            recorded.idempotency_key.as_deref() == Some(key.as_str())
                && self.verify_quarantine_marker(&recorded, capsule)?,
            "quarantine marker was not recorded"
        );
        tracing::error!(reason, run_id = %capsule.run_id,
            brain_db = %self.workspace.join("memory/brain.db").display(),
            decisions_db = %crate::causal_tree::store::db_path(&self.workspace).display(),
            "quarantined unverifiable ingress record; startup recovery will continue");
        Ok(())
    }
}

fn unverifiable_ingress(error: &anyhow::Error) -> Option<&'static str> {
    const REASONS: &[&str] = &[
        "terminal replay authentication failed",
        "terminal replay event authentication failed",
        "invalid ingress capsule authentication",
        "unsigned terminal cannot be replayed",
        "invalid terminal replay signature",
        "terminal replay binding does not match the committed marker",
        "original terminal source event is missing",
        "terminal replay decision row is missing",
    ];
    error.chain().find_map(|cause| {
        let text = cause.to_string();
        REASONS.iter().copied().find(|reason| text == *reason)
    })
}

fn digest_value(value: &impl serde::Serialize) -> anyhow::Result<String> {
    struct DigestWriter(Sha256);
    impl std::io::Write for DigestWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = DigestWriter(Sha256::new());
    serde_json::to_writer(&mut writer, value)?;
    Ok(hex::encode(writer.0.finalize()))
}
fn digest(input: &MessageEventInput) -> anyhow::Result<String> {
    digest_value(input)
}
fn transport_digest(input: &MessageEventInput) -> anyhow::Result<String> {
    // Local retry run and config generation may change; creator/lease and
    // transport attribution never do. Hash borrowed fields without cloning.
    digest_value(&(
        &input.workspace_id,
        &input.owner_id,
        &input.source,
        &input.channel,
        &input.session_key,
        (
            &input.parent_session_key,
            &input.parent_run_id,
            &input.agent_id,
            &input.persona_id,
            &input.sender,
            &input.recipient,
        ),
        (
            &input.role,
            &input.event_type,
            &input.subject,
            &input.goal_id,
            &input.causation_event_id,
            &input.correlation_id,
        ),
        (
            &input.attempt_id,
            &input.lease_epoch,
            &input.content,
            &input.raw_payload_json,
            &input.visibility,
            &input.idempotency_key,
        ),
    ))
}
fn capsule_from_event(event: &MessageEvent) -> anyhow::Result<IngressCapsule> {
    let raw = event
        .raw_payload_json
        .as_deref()
        .context("input already accepted without a recoverable capsule")?;
    anyhow::ensure!(raw.len() <= MAX_RECOVERY_EVENT_BYTES, "oversized ingress payload");
    let value: serde_json::Value = serde_json::from_str(raw)?;
    IngressCapsule::decode(&serde_json::to_string(
        value
            .get("prx_ingress_capsule")
            .context("input already accepted without a recoverable capsule")?,
    )?)
}
fn original_input(event: &MessageEvent, capsule: &IngressCapsule) -> anyhow::Result<MessageEventInput> {
    anyhow::ensure!(
        event.workspace_id == capsule.scope.workspace_id
            && event.owner_id.as_deref() == Some(capsule.scope.owner_id.as_str())
            && event.source == capsule.scope.source
            && event.event_id == capsule.input_event_id
            && event.run_id.as_deref() == Some(capsule.run_id.as_str())
            && event.idempotency_key.as_deref() == Some(capsule.input_key.as_str())
            && event.role == "user"
            && event.event_type == "message.created",
        "ingress receipt identity mismatch"
    );
    let mut value = serde_json::to_value(event)?;
    let payload: serde_json::Value = serde_json::from_str(
        event
            .raw_payload_json
            .as_deref()
            .context("ingress capsule payload missing")?,
    )?;
    let stored = capsule_from_event(event)?;
    anyhow::ensure!(
        serde_json::to_value(stored)? == serde_json::to_value(capsule)?,
        "ingress projection differs from input capsule"
    );
    *value
        .get_mut("raw_payload_json")
        .context("ingress source projection lacks its payload field")? = payload
        .get("original_payload_json")
        .context("original ingress payload missing")?
        .clone();
    Ok(serde_json::from_value(value)?)
}

/// Blocking operations remain registered until the actual closure exits.
async fn owned_store_operation<T: Send + 'static>(
    owner: &Arc<super::turn_cleanup::CleanupTracker>,
    work: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    let ticket = owner.register(tokio_util::sync::CancellationToken::new())?;
    crate::runtime::blocking::spawn_blocking(move || {
        let result = work();
        ticket.finish(result.is_ok());
        result
    })
    .await?
}

/// A completed read error is evidence failure, not an unfinished write.
/// The owning recovery operation still decides whether that failure quarantines admission.
pub(crate) async fn owned_store_read<T: Send + 'static>(
    owner: &Arc<super::turn_cleanup::CleanupTracker>,
    work: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    owned_store_operation(owner, move || Ok(work())).await?
}

fn recorders() -> &'static parking_lot::Mutex<Vec<std::sync::Weak<IngressRecorder>>> {
    static RECORDERS: std::sync::OnceLock<parking_lot::Mutex<Vec<std::sync::Weak<IngressRecorder>>>> =
        std::sync::OnceLock::new();
    RECORDERS.get_or_init(Default::default)
}
fn register_recorder(recorder: &Arc<IngressRecorder>) -> anyhow::Result<()> {
    let mut registry = recorders().lock();
    registry.retain(|item| item.strong_count() > 0);
    anyhow::ensure!(registry.len() < 128, "active ingress recovery capacity exhausted");
    registry.push(Arc::downgrade(recorder));
    Ok(())
}
/// Recover matching pinned recorders without a decision service. Historical
/// terminal repair uses the optional evidence store carried by the scope.
pub(crate) async fn recovery_ready(recovery: Arc<RecoveryScope>) -> anyhow::Result<()> {
    if !recovery.ingress_recovery_enabled() {
        return Ok(());
    }
    let active = {
        let mut registry = recorders().lock();
        registry.retain(|item| item.strong_count() > 0);
        registry.iter().filter_map(std::sync::Weak::upgrade).collect::<Vec<_>>()
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    for recorder in active {
        let Some((scope, surface)) = recorder.source.get() else {
            continue;
        };
        if !recovery.accepts_ingress_recovery(&recorder.memory, &scope.workspace_id, *surface) {
            continue;
        }
        let scope = scope.clone();
        let recovery = Arc::clone(&recovery);
        let ownership = recorder.owner.register(tokio_util::sync::CancellationToken::new())?;
        let (sent, received) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = recorder.boxed_recover(&scope, Some(&recovery), deadline).await;
            ownership.finish(result.is_ok());
            let _ = sent.send(result);
        });
        received
            .await
            .context("ingress recovery worker ended without a receipt")??;
    }
    Ok(())
}

/// Called after real publication and again before source-bound admission, so a
/// cold startup can consume final evidence that pre-proof reception could not.
pub(crate) async fn service_ready(service: &super::turn_decision::TurnDecisionService) -> anyhow::Result<()> {
    recovery_ready(service.recovery_scope()).await?;
    if !service.recovery_scope().ingress_recovery_enabled() {
        return Ok(());
    }
    let active = {
        let mut registry = recorders().lock();
        registry.retain(|item| item.strong_count() > 0);
        registry.iter().filter_map(std::sync::Weak::upgrade).collect::<Vec<_>>()
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    for recorder in active {
        let Some((scope, surface)) = recorder.source.get() else {
            continue;
        };
        if service
            .recovery_scope()
            .accepts_ingress_recovery(&recorder.memory, &scope.workspace_id, *surface)
        {
            let service = {
                let mut registry = services().lock();
                registry.retain(|item| item.strong_count() > 0);
                registry
                    .iter()
                    .filter_map(std::sync::Weak::upgrade)
                    .find(|issued| std::ptr::eq(issued.as_ref(), service))
                    .context("ingress recovery requires the published service owner")?
            };
            let scope = scope.clone();
            let ownership = recorder.owner.register(tokio_util::sync::CancellationToken::new())?;
            let (sent, received) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                let result = async {
                    // Historical child repair is independent of the ordinary
                    // ingress capsule state; its report cannot mark that input
                    // reconciled or authorize execution of a child again.
                    let report = service
                        .reconcile_child_telemetry(&scope, &recorder.lifetime, &recorder.owner, deadline)
                        .await?;
                    tracing::debug!(
                        scanned = report.scanned,
                        repaired = report.repaired,
                        already_recorded = report.already_recorded,
                        unconfirmed = report.unconfirmed,
                        no_final = report.no_final,
                        other_verified_authority = report.other_verified_authority,
                        deadline_reached = report.deadline_reached,
                        wrapped = report.wrapped,
                        scan_unavailable = report.scan_unavailable,
                        deferred = report.deferred,
                        "retained child telemetry reconciliation slice"
                    );
                    Ok::<(), anyhow::Error>(())
                }
                .await;
                ownership.finish(result.is_ok());
                let _ = sent.send(result);
            });
            received
                .await
                .context("ingress recovery worker ended without a receipt")??;
        }
    }
    Ok(())
}
fn services() -> &'static parking_lot::Mutex<Vec<std::sync::Weak<super::turn_decision::TurnDecisionService>>> {
    static SERVICES: std::sync::OnceLock<
        parking_lot::Mutex<Vec<std::sync::Weak<super::turn_decision::TurnDecisionService>>>,
    > = std::sync::OnceLock::new();
    SERVICES.get_or_init(Default::default)
}
pub(crate) fn register_service(service: &Arc<super::turn_decision::TurnDecisionService>) -> anyhow::Result<()> {
    if !service.recovery_scope().ingress_recovery_enabled() {
        return Ok(());
    }
    let mut registry = services().lock();
    registry.retain(|item| item.strong_count() > 0);
    anyhow::ensure!(registry.len() < 128, "published recovery service capacity exhausted");
    registry.push(Arc::downgrade(service));
    Ok(())
}

fn recovery_gate(lifetime: &Arc<dyn IngressLifetime>) -> anyhow::Result<Arc<tokio::sync::Mutex<()>>> {
    type Gate = (
        std::sync::Weak<dyn IngressLifetime>,
        std::sync::Weak<tokio::sync::Mutex<()>>,
    );
    static GATES: std::sync::OnceLock<parking_lot::Mutex<Vec<Gate>>> = std::sync::OnceLock::new();
    let mut gates = GATES.get_or_init(Default::default).lock();
    gates.retain(|(owner, gate)| owner.strong_count() > 0 && gate.strong_count() > 0);
    for (owner, gate) in gates.iter() {
        if let (Some(owner), Some(gate)) = (owner.upgrade(), gate.upgrade()) {
            if Arc::ptr_eq(&owner, lifetime) {
                return Ok(gate);
            }
        }
    }
    anyhow::ensure!(gates.len() < 128, "ingress recovery gate capacity exhausted");
    let gate = Arc::new(tokio::sync::Mutex::new(()));
    gates.push((Arc::downgrade(lifetime), Arc::downgrade(&gate)));
    Ok(gate)
}

#[cfg(test)]
mod tests;

mod authority;

#[cfg(test)]
mod authority_tests;
