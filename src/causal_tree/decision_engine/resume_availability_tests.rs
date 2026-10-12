#![cfg(test)]
//! Exact selector availability only. Genuine issuer/review/resume and G9 are
//! exercised by the recorded Chat collection tests, not manufactured here.

use super::*;
use crate::causal_tree::branch::{CandidateSpec, Ns8};
use crate::causal_tree::state::fixtures;
use crate::tools::capability::catalog::{CapabilityCatalog, CatalogScope, CatalogSources, SurfacePolicy};

#[test]
fn resume_availability_is_single_current_reference_and_never_catalog_exposure() -> anyhow::Result<()> {
    let policy = Arc::new(SecurityPolicy::default());
    let registry: Vec<Box<dyn crate::tools::Tool>> = vec![
        Box::new(crate::tools::file_write::FileWriteTool::new(policy)),
        Box::new(crate::tools::web_fetch::WebFetchTool::new(Vec::new(), 100, 1)),
    ];
    let catalog = CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default())?;
    let view = catalog.view(&CatalogScope::default());
    assert_eq!(view.entries().len(), 2);
    let key = fixtures::key();
    let mut candidates = Vec::new();
    let mut references = Vec::new();
    // The generator selects at most one exact candidate per family. Native
    // FileWrite/FileEdit share a candidate ID, so use a real Web-family peer.
    for name in ["file_write", "web_fetch"] {
        let entry = view
            .entries()
            .iter()
            .find(|entry| entry.public_name == name)
            .ok_or_else(|| anyhow::anyhow!("missing actual file capability"))?;
        assert!(matches!(entry.exposure, Exposure::Searchable));
        let reference = CapabilityRef {
            id: entry.id.clone(),
            fingerprint: entry.fingerprint,
            generation: entry.generation,
        };
        candidates.push(CausalCandidate::new(
            CausalActionKind::UseTool,
            entry.family,
            CandidateSpec {
                namespace: Some(Ns8::of(&key, &entry.namespace)),
                required_capabilities: vec![reference.clone()],
                side_effect: entry.effect,
                approval: entry.approval.clone(),
                estimated_cost: None,
                estimated_latency_ms: None,
            },
        ));
        references.push(reference);
    }
    let first = candidates
        .first()
        .ok_or_else(|| anyhow::anyhow!("missing first candidate"))?
        .candidate_id()
        .clone();
    let second = candidates
        .get(1)
        .ok_or_else(|| anyhow::anyhow!("missing peer candidate"))?
        .candidate_id()
        .clone();
    assert_ne!(
        first, second,
        "peer exclusion requires distinct real candidate identities"
    );
    let target = references
        .first()
        .ok_or_else(|| anyhow::anyhow!("missing exact reference"))?;
    let set = CandidateSet::Legal {
        candidates,
        exact_mentions: BTreeSet::new(),
        truncated: Vec::new(),
        discoverable_families: Default::default(),
        approval_scopes: BTreeMap::new(),
        report: GateReport {
            outcomes: BTreeMap::new(),
            gate_set_hash: [0; 32],
        },
    };
    assert!(
        selector_context(&set, &view, true, None, &DiscoveredRefs::empty())
            .direct_candidates
            .is_empty()
    );
    let selected = selector_context(&set, &view, true, Some(target), &DiscoveredRefs::empty());
    assert_eq!(selected.direct_candidates, BTreeSet::from([first]));
    assert!(!selected.direct_candidates.contains(&second));
    for stale_schema in [true, false] {
        let mut stale = target.clone();
        let byte = if stale_schema {
            stale.fingerprint.0.first_mut()
        } else {
            stale.generation.0.first_mut()
        }
        .ok_or_else(|| anyhow::anyhow!("empty fixed fingerprint"))?;
        *byte ^= 1;
        assert!(
            selector_context(&set, &view, true, Some(&stale), &DiscoveredRefs::empty())
                .direct_candidates
                .is_empty()
        );
    }
    assert!(
        view.entries()
            .iter()
            .all(|entry| matches!(entry.exposure, Exposure::Searchable))
    );
    // Availability for one authenticated reference cannot satisfy a compound
    // candidate requiring a second Searchable capability.
    let entry = view
        .entry_any(&target.id)
        .ok_or_else(|| anyhow::anyhow!("missing pinned entry"))?;
    let compound = CandidateSet::Legal {
        candidates: vec![CausalCandidate::new(
            CausalActionKind::UseTool,
            entry.family,
            CandidateSpec {
                namespace: None,
                required_capabilities: references.clone(),
                side_effect: entry.effect,
                approval: entry.approval.clone(),
                estimated_cost: None,
                estimated_latency_ms: None,
            },
        )],
        exact_mentions: BTreeSet::new(),
        truncated: Vec::new(),
        discoverable_families: Default::default(),
        approval_scopes: BTreeMap::new(),
        report: GateReport {
            outcomes: BTreeMap::new(),
            gate_set_hash: [0; 32],
        },
    };
    assert_eq!(references.len(), 2);
    assert!(
        selector_context(&compound, &view, true, Some(target), &DiscoveredRefs::empty())
            .direct_candidates
            .is_empty()
    );
    // Losing the resume input after this selection does not promote the entry.
    assert!(
        selector_context(&set, &view, true, None, &DiscoveredRefs::empty())
            .direct_candidates
            .is_empty()
    );
    // Filesystem remains unsupported for the selected-discovery path.
    let unsupported = DiscoveredRefs::from_refs_for_test(BTreeSet::from([target.clone()]));
    assert!(
        selector_context(&set, &view, true, None, &unsupported)
            .direct_candidates
            .is_empty()
    );
    Ok(())
}
