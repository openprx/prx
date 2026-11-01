//! Bounded metadata-only rehearsal. These probes cannot execute a candidate,
//! refresh a catalog, read a Skill body, or grant selection authority.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use async_trait::async_trait;

use super::branch::{
    CandidateId, CapabilityRef, CapabilityTarget, CatalogGeneration, CausalActionKind, CausalCandidate, SideEffectClass,
};
use crate::tools::capability::catalog::CatalogView;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PinnedCount {
    pub generation: CatalogGeneration,
    pub present: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("rehearsal probe unavailable")]
pub(crate) struct ProbeUnavailable;

/// The adapter resolves an opaque query reference locally and returns only a
/// count. Neither query text nor memory content crosses this interface.
#[async_trait]
pub(crate) trait MemoryProbe: Send + Sync {
    async fn count_hits(
        &self,
        principal: &str,
        query_ref: [u8; 32],
        k: u8,
        deadline: tokio::time::Instant,
    ) -> Result<u32, ProbeUnavailable>;
}

/// Reads availability from the pinned catalog snapshot. Implementations must
/// not refresh MCP, start a process, or discover another generation.
#[async_trait]
pub(crate) trait CatalogProbe: Send + Sync {
    async fn availability(
        &self,
        generation: CatalogGeneration,
        ids: &[CapabilityRef],
    ) -> Result<PinnedCount, ProbeUnavailable>;
}

/// Reads cataloged Skill metadata only; no body loader is exposed.
#[async_trait]
pub(crate) trait SkillProbe: Send + Sync {
    async fn metadata(
        &self,
        generation: CatalogGeneration,
        ids: &[CapabilityRef],
    ) -> Result<PinnedCount, ProbeUnavailable>;
}

pub(crate) struct RehearsalPorts<'a> {
    pub memory: Option<&'a dyn MemoryProbe>,
    pub catalog: Option<&'a dyn CatalogProbe>,
    pub skill: Option<&'a dyn SkillProbe>,
}

pub(crate) struct RehearsalContext<'a> {
    pub principal: &'a str,
    pub query_ref: [u8; 32],
    pub memory_k: u8,
    pub stage_cap: Duration,
    /// One absolute deadline shared by the entire candidate batch.
    pub deadline: tokio::time::Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RehearsalStatus {
    MemoryHits { hits: u32, limit: u32 },
    Availability { present: u32, required: u32 },
    NoProbe,
    Refused,
    Unavailable,
    TimedOut,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RehearsalEvidence {
    pub status: RehearsalStatus,
    /// A scorer feature, never a gate result. Errors and refusals cannot
    /// increase utility.
    pub bounded_delta: f64,
}

impl RehearsalEvidence {
    fn new(status: RehearsalStatus) -> Self {
        let bounded_delta = match status {
            RehearsalStatus::MemoryHits { hits, limit } if limit > 0 => {
                if hits == 0 {
                    -0.1
                } else {
                    0.1 * (f64::from(hits.min(limit)) / f64::from(limit))
                }
            }
            RehearsalStatus::Availability { present, required } if required > 0 && present == required => 0.1,
            RehearsalStatus::Availability { .. } => -0.1,
            RehearsalStatus::Unavailable | RehearsalStatus::TimedOut => -0.1,
            RehearsalStatus::NoProbe | RehearsalStatus::Refused | RehearsalStatus::MemoryHits { .. } => 0.0,
        };
        Self { status, bounded_delta }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum RehearsalError {
    #[error("rehearsal candidate ids must be unique")]
    DuplicateCandidate,
}

pub(crate) struct BoundedRehearsal;

impl BoundedRehearsal {
    pub(crate) async fn run(
        candidates: &[CausalCandidate],
        catalog: &CatalogView<'_>,
        ports: &RehearsalPorts<'_>,
        context: &RehearsalContext<'_>,
    ) -> Result<BTreeMap<CandidateId, RehearsalEvidence>, RehearsalError> {
        let unique: BTreeSet<_> = candidates.iter().map(CausalCandidate::candidate_id).collect();
        if unique.len() != candidates.len() {
            return Err(RehearsalError::DuplicateCandidate);
        }
        let mut evidence = BTreeMap::new();
        for candidate in candidates {
            let status = probe_one(candidate, catalog, ports, context).await;
            evidence.insert(candidate.candidate_id().clone(), RehearsalEvidence::new(status));
        }
        Ok(evidence)
    }
}

/// The stage cap, and never longer than the decision deadline.
/// A logical clock spends the cap in logical time.
async fn stage_wait<T>(
    context: &RehearsalContext<'_>,
    stage_deadline: tokio::time::Instant,
    call: impl std::future::Future<Output = T>,
) -> Result<T, ()> {
    if super::logical_clock::installed() {
        let now = super::logical_clock::now();
        let remaining = context.deadline.into_std().saturating_duration_since(now);
        let limit = context.stage_cap.min(remaining);
        let deadline = now.checked_add(limit).unwrap_or(now);
        super::logical_clock::timeout_at_std(deadline, call).await
    } else {
        tokio::time::timeout_at(stage_deadline, call).await.map_err(|_| ())
    }
}

async fn probe_one(
    candidate: &CausalCandidate,
    catalog: &CatalogView<'_>,
    ports: &RehearsalPorts<'_>,
    context: &RehearsalContext<'_>,
) -> RehearsalStatus {
    if !matches!(
        candidate.action(),
        CausalActionKind::Answer | CausalActionKind::Retrieve | CausalActionKind::Discover | CausalActionKind::UseTool
    ) || candidate.side_effect() != SideEffectClass::ReadOnly
    {
        return RehearsalStatus::Refused;
    }
    let references = candidate.required_capabilities();
    if references.iter().any(|reference| {
        catalog
            .entry_any(&reference.id)
            .is_some_and(|entry| entry.effect > SideEffectClass::ReadOnly)
    }) {
        return RehearsalStatus::Refused;
    }
    if references.iter().any(|reference| {
        reference.generation != catalog.generation()
            || catalog.entry_any(&reference.id).is_none_or(|entry| {
                entry.generation != catalog.generation()
                    || entry.fingerprint != reference.fingerprint
                    || !catalog.contains(&entry.id)
            })
    }) {
        return RehearsalStatus::Unavailable;
    }
    if super::logical_clock::tokio_deadline_reached(context.deadline) || context.stage_cap.is_zero() {
        return RehearsalStatus::TimedOut;
    }
    let now = tokio::time::Instant::now();
    let stage_deadline = now
        .checked_add(context.stage_cap)
        .map_or(context.deadline, |cap| cap.min(context.deadline));
    // A logical clock keeps the probe's wall deadline at the decision deadline,
    // which sits ahead of wall time. The stage cap is then logical.
    let probe_deadline = if super::logical_clock::installed() {
        context.deadline
    } else {
        stage_deadline
    };
    let generation = catalog.generation();
    if candidate.action() == CausalActionKind::Retrieve && candidate.target() == CapabilityTarget::Memory {
        let Some(port) = ports.memory else {
            return RehearsalStatus::NoProbe;
        };
        let requested = u32::from(context.memory_k);
        if requested == 0 {
            return RehearsalStatus::NoProbe;
        }
        return match stage_wait(
            context,
            stage_deadline,
            port.count_hits(context.principal, context.query_ref, context.memory_k, probe_deadline),
        )
        .await
        {
            Ok(Ok(hits)) if hits <= requested => RehearsalStatus::MemoryHits { hits, limit: requested },
            Ok(Ok(_) | Err(_)) => RehearsalStatus::Unavailable,
            Err(_) => RehearsalStatus::TimedOut,
        };
    }
    if references.is_empty() {
        return RehearsalStatus::NoProbe;
    }
    let response = if candidate.target() == CapabilityTarget::Skill {
        let Some(port) = ports.skill else {
            return RehearsalStatus::NoProbe;
        };
        stage_wait(context, stage_deadline, port.metadata(generation, references)).await
    } else {
        let Some(port) = ports.catalog else {
            return RehearsalStatus::NoProbe;
        };
        stage_wait(context, stage_deadline, port.availability(generation, references)).await
    };
    match response {
        Ok(Ok(count)) if count.generation == generation && count.present <= references.len() as u32 => {
            RehearsalStatus::Availability {
                present: count.present,
                required: references.len() as u32,
            }
        }
        Ok(Ok(_) | Err(_)) => RehearsalStatus::Unavailable,
        Err(_) => RehearsalStatus::TimedOut,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::causal_tree::branch::{ApprovalRequirement, CandidateSpec, CostEstimate, Ns8};
    use crate::causal_tree::state::fixtures;
    use crate::tools::capability::catalog::{CapabilityCatalog, CatalogScope, CatalogSources, SurfacePolicy};
    use crate::tools::traits::{Tool, ToolResult};
    use serde_json::{Value, json};

    struct NamedTool(&'static str);

    #[async_trait]
    impl Tool for NamedTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "Local fixture"
        }
        fn parameters_schema(&self) -> Value {
            json!({"type":"object","properties":{}})
        }
        async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
            panic!("rehearsal must never execute a tool")
        }
    }

    fn candidate(
        action: CausalActionKind,
        target: CapabilityTarget,
        effect: SideEffectClass,
        references: Vec<CapabilityRef>,
        namespace: Option<Ns8>,
    ) -> CausalCandidate {
        CausalCandidate::new(
            action,
            target,
            CandidateSpec {
                namespace,
                required_capabilities: references,
                side_effect: effect,
                approval: ApprovalRequirement::None,
                estimated_cost: Some(CostEstimate {
                    tokens: 1,
                    micro_usd: 0,
                }),
                estimated_latency_ms: Some(1),
            },
        )
    }

    fn context() -> RehearsalContext<'static> {
        RehearsalContext {
            principal: "prx_user",
            query_ref: [7; 32],
            memory_k: 2,
            stage_cap: Duration::from_millis(100),
            deadline: tokio::time::Instant::now() + Duration::from_secs(1),
        }
    }

    struct MemoryCounter {
        calls: AtomicUsize,
        delay: Duration,
        fail: bool,
    }

    #[async_trait]
    impl MemoryProbe for MemoryCounter {
        async fn count_hits(
            &self,
            principal: &str,
            query_ref: [u8; 32],
            k: u8,
            _deadline: tokio::time::Instant,
        ) -> Result<u32, ProbeUnavailable> {
            assert_eq!(principal, "prx_user");
            assert_eq!(query_ref, [7; 32]);
            assert_eq!(k, 2);
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            if self.fail { Err(ProbeUnavailable) } else { Ok(1) }
        }
    }

    struct CatalogCounter {
        calls: AtomicUsize,
        wrong_generation: bool,
        present_override: Option<u32>,
    }

    #[async_trait]
    impl CatalogProbe for CatalogCounter {
        async fn availability(
            &self,
            generation: CatalogGeneration,
            ids: &[CapabilityRef],
        ) -> Result<PinnedCount, ProbeUnavailable> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(PinnedCount {
                generation: if self.wrong_generation {
                    CatalogGeneration([9; 32])
                } else {
                    generation
                },
                present: self.present_override.unwrap_or(ids.len() as u32),
            })
        }
    }

    #[tokio::test]
    async fn write_and_effectively_write_candidates_never_call_probes() {
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(NamedTool("shell"))];
        let catalog = CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default())
            .unwrap_or_else(|error| panic!("{error}"));
        let view = catalog.view(&CatalogScope::default());
        let Some(entry) = view.entries().first() else {
            panic!("expected shell entry")
        };
        assert!(entry.effect > SideEffectClass::ReadOnly);
        let reference = CapabilityRef {
            id: entry.id.clone(),
            fingerprint: entry.fingerprint,
            generation: entry.generation,
        };
        let candidates = [
            candidate(
                CausalActionKind::UseTool,
                CapabilityTarget::NativeTool,
                SideEffectClass::LocalWrite,
                Vec::new(),
                None,
            ),
            candidate(
                CausalActionKind::Retrieve,
                entry.family,
                SideEffectClass::ReadOnly,
                vec![reference],
                None,
            ),
            candidate(
                CausalActionKind::Clarify,
                CapabilityTarget::None,
                SideEffectClass::ReadOnly,
                Vec::new(),
                None,
            ),
        ];
        let memory = MemoryCounter {
            calls: AtomicUsize::new(0),
            delay: Duration::ZERO,
            fail: false,
        };
        let catalog_probe = CatalogCounter {
            calls: AtomicUsize::new(0),
            wrong_generation: false,
            present_override: None,
        };
        let ports = RehearsalPorts {
            memory: Some(&memory),
            catalog: Some(&catalog_probe),
            skill: None,
        };
        let evidence = BoundedRehearsal::run(&candidates, &view, &ports, &context())
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        for item in &candidates {
            assert_eq!(
                evidence.get(item.candidate_id()).map(|value| value.status),
                Some(RehearsalStatus::Refused)
            );
        }
        assert_eq!(memory.calls.load(Ordering::SeqCst), 0);
        assert_eq!(catalog_probe.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn serial_slow_probes_share_one_deadline() {
        let catalog = CapabilityCatalog::build(&[], &CatalogSources::default(), &SurfacePolicy::default())
            .unwrap_or_else(|error| panic!("{error}"));
        let view = catalog.view(&CatalogScope::default());
        let key = fixtures::key();
        let candidates = [
            candidate(
                CausalActionKind::Retrieve,
                CapabilityTarget::Memory,
                SideEffectClass::ReadOnly,
                Vec::new(),
                Some(Ns8::of(&key, "first")),
            ),
            candidate(
                CausalActionKind::Retrieve,
                CapabilityTarget::Memory,
                SideEffectClass::ReadOnly,
                Vec::new(),
                Some(Ns8::of(&key, "second")),
            ),
        ];
        let memory = MemoryCounter {
            calls: AtomicUsize::new(0),
            delay: Duration::from_millis(20),
            fail: false,
        };
        let ports = RehearsalPorts {
            memory: Some(&memory),
            catalog: None,
            skill: None,
        };
        let mut ctx = context();
        ctx.deadline = tokio::time::Instant::now() + Duration::from_millis(25);
        let evidence = BoundedRehearsal::run(&candidates, &view, &ports, &ctx)
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            evidence
                .get(candidates.first().unwrap().candidate_id())
                .map(|value| value.status),
            Some(RehearsalStatus::MemoryHits { hits: 1, limit: 2 })
        );
        assert_eq!(
            evidence
                .get(candidates.last().unwrap().candidate_id())
                .map(|value| value.status),
            Some(RehearsalStatus::TimedOut)
        );
        assert_eq!(memory.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn stale_catalog_and_probe_error_never_raise_score() {
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(NamedTool("file_read"))];
        let catalog = CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default())
            .unwrap_or_else(|error| panic!("{error}"));
        let view = catalog.view(&CatalogScope::default());
        let Some(entry) = view.entries().first() else {
            panic!("expected read entry")
        };
        assert_eq!(entry.effect, SideEffectClass::ReadOnly);
        let reference = CapabilityRef {
            id: entry.id.clone(),
            fingerprint: entry.fingerprint,
            generation: entry.generation,
        };
        let read = candidate(
            CausalActionKind::UseTool,
            entry.family,
            SideEffectClass::ReadOnly,
            vec![reference],
            None,
        );
        let catalog_probe = CatalogCounter {
            calls: AtomicUsize::new(0),
            wrong_generation: true,
            present_override: None,
        };
        let ports = RehearsalPorts {
            memory: None,
            catalog: Some(&catalog_probe),
            skill: None,
        };
        let evidence = BoundedRehearsal::run(std::slice::from_ref(&read), &view, &ports, &context())
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        let Some(result) = evidence.get(read.candidate_id()) else {
            panic!("expected evidence")
        };
        assert_eq!(result.status, RehearsalStatus::Unavailable);
        assert!(result.bounded_delta <= 0.0);
        assert_eq!(catalog_probe.calls.load(Ordering::SeqCst), 1);

        let memory = MemoryCounter {
            calls: AtomicUsize::new(0),
            delay: Duration::ZERO,
            fail: true,
        };
        let ports = RehearsalPorts {
            memory: Some(&memory),
            catalog: None,
            skill: None,
        };
        let retrieval = candidate(
            CausalActionKind::Retrieve,
            CapabilityTarget::Memory,
            SideEffectClass::ReadOnly,
            Vec::new(),
            None,
        );
        let evidence = BoundedRehearsal::run(std::slice::from_ref(&retrieval), &view, &ports, &context())
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        let Some(result) = evidence.get(retrieval.candidate_id()) else {
            panic!("expected evidence")
        };
        assert_eq!(result.status, RehearsalStatus::Unavailable);
        assert!(result.bounded_delta <= 0.0);
    }

    #[tokio::test]
    async fn duplicate_candidate_batch_calls_no_port() {
        let catalog = CapabilityCatalog::build(&[], &CatalogSources::default(), &SurfacePolicy::default())
            .unwrap_or_else(|error| panic!("{error}"));
        let view = catalog.view(&CatalogScope::default());
        let retrieval = candidate(
            CausalActionKind::Retrieve,
            CapabilityTarget::Memory,
            SideEffectClass::ReadOnly,
            Vec::new(),
            None,
        );
        let memory = MemoryCounter {
            calls: AtomicUsize::new(0),
            delay: Duration::ZERO,
            fail: false,
        };
        let ports = RehearsalPorts {
            memory: Some(&memory),
            catalog: None,
            skill: None,
        };
        let result = BoundedRehearsal::run(&[retrieval.clone(), retrieval], &view, &ports, &context()).await;
        assert!(matches!(result, Err(RehearsalError::DuplicateCandidate)));
        assert_eq!(memory.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn partial_required_availability_is_adverse() {
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(NamedTool("file_read")), Box::new(NamedTool("memory_search"))];
        let catalog = CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default())
            .unwrap_or_else(|error| panic!("{error}"));
        let view = catalog.view(&CatalogScope::default());
        let references: Vec<_> = view
            .entries()
            .iter()
            .map(|entry| {
                assert_eq!(entry.effect, SideEffectClass::ReadOnly);
                CapabilityRef {
                    id: entry.id.clone(),
                    fingerprint: entry.fingerprint,
                    generation: entry.generation,
                }
            })
            .collect();
        assert_eq!(references.len(), 2);
        let read = candidate(
            CausalActionKind::UseTool,
            CapabilityTarget::Filesystem,
            SideEffectClass::ReadOnly,
            references,
            None,
        );
        let catalog_probe = CatalogCounter {
            calls: AtomicUsize::new(0),
            wrong_generation: false,
            present_override: Some(1),
        };
        let ports = RehearsalPorts {
            memory: None,
            catalog: Some(&catalog_probe),
            skill: None,
        };
        let evidence = BoundedRehearsal::run(std::slice::from_ref(&read), &view, &ports, &context())
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        let Some(result) = evidence.get(read.candidate_id()) else {
            panic!("expected evidence")
        };
        assert_eq!(
            result.status,
            RehearsalStatus::Availability {
                present: 1,
                required: 2
            }
        );
        assert!(result.bounded_delta <= 0.0);
        assert_eq!(catalog_probe.calls.load(Ordering::SeqCst), 1);
    }
}
