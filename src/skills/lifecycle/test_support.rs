//! Fixtures for tests outside `src/skills` that need an operator revision
//! published. The raw store writes are `pub(in crate::skills)`, so the
//! compiler keeps every other module away from them; this is the one named
//! door, and it exists only in test builds.

use super::bundle::Bundle;
use super::journal::TransactionId;
use super::publish::PublishRequest;
use super::store::SkillLifecycleStore;
use super::types::{NewRevision, RevisionId, SkillId, SourceKind};

/// Writes `bundle` as a revision of `skill` and publishes it from
/// `expected_generation`, enabled. Returns the revision id.
///
/// # Panics
/// Panics when the store refuses either step; test fixtures only.
pub(crate) fn publish_revision(
    store: &SkillLifecycleStore,
    skill: &SkillId,
    kind: SourceKind,
    bundle: &Bundle,
    expected_generation: u64,
) -> RevisionId {
    let written = store
        .write_revision(&NewRevision::new(skill.clone(), kind), bundle)
        .expect("test: write revision");
    store
        .publish(&PublishRequest {
            transaction_id: TransactionId::generate(),
            skill_id: skill.clone(),
            candidate: written.revision.revision_id.clone(),
            candidate_record_digest: written.record_digest,
            expected_generation,
            enabled: true,
        })
        .expect("test: publish revision");
    written.revision.revision_id
}

/// Like [`publish_revision`], for an executable bundle the operator reviewed:
/// the revision records the review of its own bundle hash, as an executable
/// import does.
///
/// # Panics
/// Panics when the store refuses either step; test fixtures only.
pub(crate) fn publish_reviewed_executable(
    store: &SkillLifecycleStore,
    skill: &SkillId,
    bundle: &Bundle,
    expected_generation: u64,
) -> RevisionId {
    let mut new = NewRevision::new(skill.clone(), SourceKind::Operator);
    new.review_receipt_ids = vec![super::executable::review_receipt_id(bundle.revision_id())];
    let written = store.write_revision(&new, bundle).expect("test: write revision");
    store
        .publish(&PublishRequest {
            transaction_id: TransactionId::generate(),
            skill_id: skill.clone(),
            candidate: written.revision.revision_id.clone(),
            candidate_record_digest: written.record_digest,
            expected_generation,
            enabled: true,
        })
        .expect("test: publish revision");
    written.revision.revision_id
}

/// Build managed prompts with pinned projections and optional usable evidence.
/// The evidence follows the lifecycle proposal fixtures: two independent tasks
/// followed the pinned guidance and failed without infrastructure or permission causes.
pub(crate) fn managed_prompt_fixture(
    workspace: &std::path::Path,
    skills: &[(&str, bool)],
) -> std::sync::Arc<super::SkillsLifecycleService> {
    use super::*;
    use std::sync::Arc;
    let store =
        Arc::new(SkillLifecycleStore::open(workspace, OwnerScope::new("owner:ws:cli:prx_user").unwrap()).unwrap());
    let mut suites = Vec::new();
    for &(name, evidence) in skills {
        let skill = SkillId::parse(name).unwrap();
        let mut content = format!("---\nname: {name}\ndescription: Project checks\n---\n\n# Checks\n\n");
        for n in 1..=20 {
            content.push_str(&format!("Step {n}: run project check number {n} before continuing.\n"));
        }
        let bundle = Bundle::from_entries(vec![BundleEntry {
            path: "SKILL.md".into(),
            bytes: content.as_bytes().to_vec(),
            executable: false,
        }])
        .unwrap();
        let mut new = NewRevision::new(skill.clone(), SourceKind::Operator);
        new.capability_requirements = vec!["native_tool:native:project_check".into()];
        let written = store.write_revision(&new, &bundle).unwrap();
        store
            .publish(&PublishRequest {
                transaction_id: TransactionId::generate(),
                skill_id: skill.clone(),
                candidate: written.revision.revision_id,
                candidate_record_digest: written.record_digest,
                expected_generation: 0,
                enabled: true,
            })
            .unwrap();
        let projection = workspace.join("skills").join(name);
        std::fs::create_dir_all(&projection).unwrap();
        std::fs::write(projection.join("SKILL.md"), &content).unwrap();
        if evidence {
            let pin = store.pin_active(&skill).unwrap().unwrap();
            let facts = TerminalFacts {
                infrastructure_unavailable: FactState::Absent,
                permission_refused: FactState::Absent,
                adherence: vec![SkillAdherence {
                    skill_id: skill.clone(),
                    revision_id: pin.revision_id.clone(),
                    verdict: Adherence::Followed,
                    missing: Vec::new(),
                    diagnostic: None,
                }],
            };
            let mut ids = Vec::new();
            for n in 1..=2 {
                let task = format!("{name}-task-{n}");
                store
                    .record_hydration(&task, &pin, "SKILL.md", b"x", &Correlation::default())
                    .unwrap();
                store
                    .join_terminal_with(&task, "term", ExperienceOutcome::Failure, &facts)
                    .unwrap();
                ids.push(experience_id(store.owner_scope(), &task));
            }
            let pattern = store
                .consolidate_pattern(&ConsolidationRequest {
                    task_family: "project checks".into(),
                    failure_class: FailureClass::MissingGuidance,
                    skill_id: Some(skill.clone()),
                    scope: PatternScope {
                        environment: "local".into(),
                        capabilities: vec![],
                    },
                    statement: "The guidance omits result verification.".into(),
                    applicability: vec!["project checks".into()],
                    exceptions: vec![],
                    evidence: ids,
                    counterevidence: vec![],
                })
                .unwrap()
                .pattern;
            assert!(pattern.usable_for_proposal());
        }
        suites.push((skill, "a".repeat(64)));
    }
    Arc::new(
        SkillsLifecycleService::new(
            store,
            SkillsLifecycleSettings {
                enabled: true,
                suites: SuiteRegistry::new(suites).unwrap(),
                ..Default::default()
            },
        )
        .unwrap(),
    )
}

/// A counted model that returns a small lifecycle patch, never a whole document.
pub(crate) struct ManagedPromptModel {
    binding: super::ModelBinding,
    pub(crate) calls: std::sync::atomic::AtomicUsize,
    pub(crate) addition: String,
}

impl ManagedPromptModel {
    pub(crate) fn new(addition: &str) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            binding: super::ModelBinding {
                provider_label: "fixture".into(),
                model: "fixture".into(),
                temperature_milli: 0,
            },
            calls: std::sync::atomic::AtomicUsize::new(0),
            addition: addition.into(),
        })
    }
}

#[async_trait::async_trait]
impl super::ProposerModel for ManagedPromptModel {
    fn binding(&self) -> &super::ModelBinding {
        &self.binding
    }
    async fn complete(
        &self,
        _: &str,
        _: &str,
        _: std::sync::Arc<super::TallyCounters>,
    ) -> Result<super::ModelReply, super::ModelCallError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(super::ModelReply {
            accounting_scope_id: None,
            text: serde_json::json!({"summary": "Clarify result verification", "edits": [
                {"file": "SKILL.md", "op": "append", "new": self.addition}
            ]})
            .to_string(),
            usage: super::ReportedUsage::default(),
        })
    }
}
