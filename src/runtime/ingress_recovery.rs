//! Recovery consumes authenticated durable reception, never execution authority.
use crate::decision::key::{DecisionKey, KeyDomain};
use crate::memory::ingress_recovery::{
    IngressAppendOutcome, IngressCapsule, IngressEventLookup, IngressLifetime, IngressRecoveryCommit,
    IngressRecoveryScan, IngressRecoveryScope, IngressRecoveryState, MAX_CAPSULE_BYTES, MAX_RECOVERY_EVENT_BYTES,
    MAX_RECOVERY_PAGE, input_within_bounds, remaining, serialized_within,
};
use crate::memory::{Memory, MemoryVisibility, MessageEvent, MessageEventInput};
use anyhow::Context;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
#[error("input already accepted; execution was not repeated and its outcome remains unconfirmed")]
pub(crate) struct AlreadyAccepted;

pub(crate) struct IngressRecorder {
    memory: Arc<dyn Memory>,
    key: Arc<DecisionKey>,
    lifetime: Arc<dyn IngressLifetime>,
    deadline: Instant,
    owner: Arc<super::turn_cleanup::CleanupTracker>,
    duplicate: std::sync::atomic::AtomicBool,
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
        let key = owned_store_operation(&ticket.owner(), move || {
            remaining(deadline)?;
            let key = DecisionKey::load_or_create(&workspace_path)?;
            remaining(deadline)?;
            Ok(key)
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
            key,
            lifetime,
            deadline,
            owner: ticket.owner(),
            duplicate: Default::default(),
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
    fn sign(&self, capsule: &IngressCapsule) -> anyhow::Result<String> {
        serialized_within(capsule, MAX_CAPSULE_BYTES - 64)?;
        let mut unsigned = capsule.clone();
        unsigned.authentication.clear();
        Ok(hex::encode(
            self.key
                .hmac(KeyDomain::IngressRecovery, &[&serde_json::to_vec(&unsigned)?]),
        ))
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
            self.key
                .verify_hmac(KeyDomain::IngressRecovery, &[&serde_json::to_vec(&unsigned)?], &tag),
            "invalid ingress capsule authentication"
        );
        let normalized = original_input(input, capsule)?;
        anyhow::ensure!(
            digest(&normalized)? == capsule.input_digest,
            "ingress input digest differs from its capsule"
        );
        Ok(normalized)
    }
    pub(crate) async fn append(self: &Arc<Self>, mut input: MessageEventInput) -> anyhow::Result<MessageEvent> {
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
        let mut capsule = IngressCapsule {
            version: 1,
            backend_namespace: self.lifetime.namespace().to_owned(),
            scope,
            lifetime: self.lifetime.token().to_owned(),
            input_event_id: input.event_id.clone().context("ingress event identity missing")?,
            input_key: input
                .idempotency_key
                .clone()
                .context("durable ingress requires stable input key")?,
            run_id: input.run_id.clone().context("durable ingress requires run identity")?,
            input_digest: digest(&input)?,
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
        service: Option<&super::turn_decision::TurnDecisionService>,
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
                    if service.is_some() {
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
                if capsule.lifetime == self.lifetime.token() {
                    continue;
                }
                if self.boxed_quarantine_marker(scope, &capsule).await? {
                    continue;
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
                    Err(error) if unverifiable_ingress(&error) => {
                        self.boxed_record_quarantine(scope, &capsule, "invalid ingress capsule authentication")
                            .await?;
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                let final_event = if service.is_some() {
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
                let state = if let (Some(service), Some(_)) = (service, final_event.as_ref()) {
                    match service.rejoin_ingress_terminal(&capsule, deadline, &self.owner).await {
                        Ok(_) => IngressRecoveryState::Reconciled,
                        Err(error) if unverifiable_ingress(&error) => {
                            self.boxed_record_quarantine(scope, &capsule, "terminal replay authentication failed")
                                .await?;
                            continue;
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
                let authentication = hex::encode(self.key.hmac(
                    KeyDomain::IngressRecovery,
                    &[
                        b"recovery-state",
                        state_label.as_bytes(),
                        &serde_json::to_vec(&capsule)?,
                    ],
                ));
                evidence.raw_payload_json = Some(serde_json::to_string(
                    &serde_json::json!({"ingress_recovery":{"capsule":capsule,"state":state_label,"evidence_authentication":authentication}}),
                )?);
                let expected = evidence.clone();
                let recorded = self
                    .memory
                    .commit_ingress_recovery(IngressRecoveryCommit {
                        capsule,
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
            }
        }
    }

    // `append` and service startup await recovery. Storing that state machine
    // in those futures overflows the test thread stack.
    #[inline(never)]
    fn boxed_recover<'a>(
        &'a self,
        scope: &'a IngressRecoveryScope,
        service: Option<&'a super::turn_decision::TurnDecisionService>,
        deadline: Instant,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(self.recover(scope, service, deadline))
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

    async fn quarantine_marker_present(
        &self,
        scope: &IngressRecoveryScope,
        capsule: &IngressCapsule,
    ) -> anyhow::Result<bool> {
        Ok(self
            .memory
            .find_owned_message_event_by_idempotency_key(
                &scope.workspace_id,
                &scope.owner_id,
                &Self::quarantine_key(capsule),
            )
            .await?
            .is_some())
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
        tracing::error!(reason, "quarantined unverifiable terminal record; chat will continue");
        let key = Self::quarantine_key(capsule);
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
                raw_payload_json: None,
                visibility: MemoryVisibility::System,
            })
            .await?;
        anyhow::ensure!(
            recorded.idempotency_key.as_deref() == Some(key.as_str())
                && recorded.event_type == "turn.ingress.quarantined",
            "quarantine marker was not recorded"
        );
        Ok(())
    }
}

fn unverifiable_ingress(error: &anyhow::Error) -> bool {
    const REASONS: &[&str] = &[
        "terminal replay authentication failed",
        "terminal replay event authentication failed",
        "invalid ingress capsule authentication",
        "unsigned terminal cannot be replayed",
        "invalid terminal replay signature",
    ];
    error.chain().any(|cause| {
        let text = cause.to_string();
        REASONS.iter().any(|reason| text == *reason)
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
/// Called after real publication and again before source-bound admission, so a
/// cold startup can consume final evidence that pre-proof reception could not.
pub(crate) async fn service_ready(service: &super::turn_decision::TurnDecisionService) -> anyhow::Result<()> {
    if !service.ingress_recovery_enabled() {
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
        if service.accepts_ingress_recovery(&recorder.memory, &scope.workspace_id, *surface) {
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
                    recorder.boxed_recover(&scope, Some(&service), deadline).await?;
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
    if !service.ingress_recovery_enabled() {
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
