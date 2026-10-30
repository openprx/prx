//! Mint an annotation only from one exact, already finalized publication.
//! This owner never publishes, reconciles, repairs, judges or grants a gate.
use super::context::{LessonCompatibility, LessonRef};
use super::record::{AdoptionRecord, LessonEdit, SupportedGuidance, VerifiedAdoptionRecord, digest, valid_digest};
use crate::skills::lifecycle::authorize::{MAX_PUBLICATION_ATTEMPTS, authorization_pin, publication_tx};
use crate::skills::lifecycle::experiment::{ExperimentId, ExperimentJournal, ExperimentResources};
use crate::skills::lifecycle::proposal::{EditOp, ProposalLock};
use crate::skills::lifecycle::{
    GateError, GateReviewPin, ProposalOperation, ProposalStatus, SkillLifecycleStore, TransactionId, TransactionKind,
    TransactionState, VerifiedGateReviewHistory,
};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Caller intent and exact pins only. Deserialization establishes no authority;
/// source, owner, compatibility, exclusions and content are resolved by service.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordAdoptionRequest {
    pub proposal_id: String,
    pub publication_tx: String,
    pub experiment_id: String,
    pub experiment_sha256: String,
    pub gate_stage: String,
    pub gate_witness_sha256: String,
    pub gate_id: String,
    pub gate_sha256: String,
    pub task_family: String,
    /// Explicit eligibility deadline, never an automatic retention/GC TTL.
    pub expires_at: DateTime<Utc>,
}
impl RecordAdoptionRequest {
    pub(in crate::skills::lifecycle) fn experiment(&self) -> Result<ExperimentId> {
        Ok(ExperimentId::parse(&self.experiment_id)?)
    }
    pub(in crate::skills::lifecycle) fn gate_pin(&self) -> Result<GateReviewPin> {
        ensure!(
            [&self.experiment_sha256, &self.gate_witness_sha256, &self.gate_sha256]
                .into_iter()
                .all(|value| valid_digest(value)),
            "adoption confirmation contains an invalid digest"
        );
        crate::skills::lifecycle::TransactionId::parse(&self.gate_stage)?;
        Ok(GateReviewPin {
            stage: self.gate_stage.clone(),
            witness_sha256: self.gate_witness_sha256.clone(),
            gate_id: self.gate_id.clone(),
            gate_sha256: self.gate_sha256.clone(),
        })
    }
}

/// Complete actual-owner coverage, not another balance. Each admitted attempt
/// appears once under its original scope; six-component usage comes from sealed
/// StageSettled and physical calls/tokens/fees are matched to the existing ledger.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::skills::lifecycle) struct AdoptionRootCostEvidence {
    pub schema_version: u32,
    pub owner_scope: crate::skills::lifecycle::OwnerScope,
    pub experiment_id: String,
    pub experiment_sha256: String,
    pub experiment_head_sha256: String,
    pub usage: ExperimentResources,
    pub models: Vec<String>,
    pub stages: Vec<AdoptionStageCostEvidence>,
}
impl AdoptionRootCostEvidence {
    pub(super) fn digest(&self) -> Result<String> {
        self.validate()?;
        Ok(digest(&serde_json::to_vec(self)?))
    }
    pub(super) fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1
                && valid_digest(&self.experiment_sha256)
                && valid_digest(&self.experiment_head_sha256)
                && !self.stages.is_empty()
                && self.models.windows(2).all(|pair| pair.first() < pair.get(1))
                && self.models.iter().all(|model| !model.is_empty()),
            "invalid root cost snapshot header"
        );
        let mut names = std::collections::BTreeSet::new();
        let mut scopes = std::collections::BTreeSet::new();
        let mut attempts = std::collections::BTreeSet::new();
        let mut total = ExperimentResources::default();
        for stage in &self.stages {
            ensure!(
                names.insert(&stage.stage)
                    && scopes.insert(&stage.physical_scope_id)
                    && valid_digest(&stage.physical_scope_id)
                    && stage
                        .model_indices
                        .iter()
                        .all(|index| usize::try_from(*index).is_ok_and(|index| index < self.models.len()))
                    && stage.model_indices.windows(2).all(|pair| pair.first() < pair.get(1)),
                "root cost stage or model table is not canonical"
            );
            let mut count = 0_u64;
            for (prefix, suffixes) in &stage.attempts {
                ensure!(
                    !suffixes.is_empty() && suffixes.windows(2).all(|pair| pair.first() < pair.get(1)),
                    "root cost attempt group is not canonical"
                );
                for suffix in suffixes {
                    let id = format!("{prefix}{suffix}");
                    ensure!(
                        !id.is_empty() && attempts.insert(id),
                        "root cost repeats an admitted attempt"
                    );
                    count = count.checked_add(1).context("root attempt count overflow")?;
                }
            }
            let usage = stage.resources();
            ensure!(
                usage.calls == count
                    && ((count == 0 && stage.model_indices.is_empty())
                        || (count > 0 && !stage.model_indices.is_empty())),
                "root cost does not cover its actual attempts/models"
            );
            total = total.checked_add(usage)?;
        }
        ensure!(
            total == self.usage,
            "root cost six-component sum differs from its stages"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::skills::lifecycle) struct AdoptionStageCostEvidence {
    pub stage: String,
    pub physical_scope_id: String,
    /// Lossless order: calls, tokens, micro_usd, tool_calls, output_bytes, scratch_bytes.
    pub usage: [u64; 6],
    /// Original IDs reconstruct as prefix + suffix. Shared purpose prefixes
    /// appear once; no attempt is truncated, hashed away or duplicated.
    pub attempts: std::collections::BTreeMap<String, Vec<String>>,
    /// Indices into the root's one canonical model table.
    pub model_indices: Vec<u32>,
}
impl AdoptionStageCostEvidence {
    pub(in crate::skills::lifecycle) const fn resources(&self) -> ExperimentResources {
        let [calls, tokens, micro_usd, tool_calls, output_bytes, scratch_bytes] = self.usage;
        ExperimentResources {
            calls,
            tokens,
            micro_usd,
            tool_calls,
            output_bytes,
            scratch_bytes,
        }
    }
}

impl SkillLifecycleStore {
    /// The existing proposal owner keeps source and retention stable. Only this
    /// module constructs VerifiedAdoptionRecord; the service cannot mint it.
    pub(in crate::skills::lifecycle) fn record_committed_adoption_locked(
        &self,
        owner: &ProposalLock,
        request: &RecordAdoptionRequest,
        journal: &ExperimentJournal,
        history: &VerifiedGateReviewHistory,
        compatibility: &LessonCompatibility,
        config: &crate::config::Config,
    ) -> Result<LessonRef> {
        // Completeness is derived here from the actual original ledger. No
        // public/internal request can supply a self-labelled complete body.
        let cost = self.adoption_root_cost_evidence(config, journal, history)?;
        let root_cost = cost.as_ref();
        let first = self.prove_committed_adoption(owner, request, journal, history, compatibility, root_cost)?;
        // Re-read the exact publication, revisions, source review and projection
        // immediately before annotation. No recovery or authority is inferred.
        let current_journal = self.read_experiment(&request.experiment()?, &request.experiment_sha256)?;
        ensure!(
            current_journal.head_sha256 == journal.head_sha256,
            "adoption experiment changed during proof"
        );
        let second =
            self.prove_committed_adoption(owner, request, &current_journal, history, compatibility, root_cost)?;
        ensure!(first.0 == second.0, "adoption source facts changed during proof");
        let cost_again = self.adoption_root_cost_evidence(config, &current_journal, history)?;
        ensure!(
            cost == cost_again,
            "root physical evidence changed during adoption proof"
        );
        Ok(self.persist_verified_adoption_locked(owner, &second)?)
    }

    fn prove_committed_adoption(
        &self,
        owner: &ProposalLock,
        request: &RecordAdoptionRequest,
        journal: &ExperimentJournal,
        history: &VerifiedGateReviewHistory,
        compatibility: &LessonCompatibility,
        root_cost: Option<&AdoptionRootCostEvidence>,
    ) -> Result<VerifiedAdoptionRecord> {
        let pin = request.gate_pin()?;
        let experiment_id = request.experiment()?;
        let transaction = TransactionId::parse(&request.publication_tx)?;
        let subject = history.subject();
        let origin = journal.plan.spec();
        ensure!(
            journal.state.qualified(&journal.plan)
                && origin.owner_scope == *self.owner_scope()
                && origin.id == experiment_id
                && journal.plan.digest() == request.experiment_sha256
                && history.binding().owner_scope == *self.owner_scope()
                && history.binding().experiment.experiment_sha256 == journal.plan.digest()
                && history.pin() == pin
                && subject.proposal_id == request.proposal_id
                && subject.skill_id == origin.pins.skill_id
                && subject.baseline_revision.as_ref() == Some(&origin.pins.baseline_revision)
                && subject.suite_digest == origin.pins.suite_digest
                && subject.expected_generation == origin.pins.baseline_generation,
            "adoption does not name its exact owner/experiment/gate subject"
        );
        ensure!(
            compatibility.model_sha256 == origin.pins.model.digest()
                && compatibility.policy_sha256 == origin.pins.policy_sha256
                && compatibility.source_profile_sha256 == origin.pins.binary_sha256,
            "adoption compatibility is not the actual original source/model/policy"
        );
        if let Some(cost) = root_cost {
            cost.validate()?;
            ensure!(
                cost.schema_version == 1
                    && cost.owner_scope == *self.owner_scope()
                    && cost.experiment_id == request.experiment_id
                    && cost.experiment_sha256 == request.experiment_sha256
                    && cost.experiment_head_sha256 == journal.head_sha256
                    && cost.stages.len() == origin.stages.len()
                    && cost.usage.within(origin.resources),
                "complete root cost evidence belongs to another frozen execution"
            );
        }
        let proposal = self
            .read_proposal(&request.proposal_id)?
            .context("adoption proposal is missing")?;
        ensure!(
            proposal.owner_scope == *self.owner_scope()
                && proposal.status == ProposalStatus::Committed
                && proposal.operation == ProposalOperation::Revise
                && proposal.operation == subject.operation
                && proposal.skill_id == subject.skill_id
                && proposal.baseline_revision == subject.baseline_revision
                && proposal.candidate_revision.as_ref() == Some(&subject.candidate_revision)
                && proposal.expected_generation == subject.expected_generation,
            "adoption requires the exact finalized committed revision proposal"
        );
        // Current mutable evaluation_epoch/status are never used to derive a
        // historical gate. Its immutable epoch is already proved by D.
        let auth = proposal
            .authorization
            .as_ref()
            .context("committed proposal has no authorization")?;
        let publication = proposal
            .publication
            .as_ref()
            .context("committed proposal has no publication")?;
        ensure!(
            (1..=MAX_PUBLICATION_ATTEMPTS).contains(&auth.attempt)
                && publication_tx(&proposal.proposal_id, auth.attempt)? == transaction
                && auth.gate_id == pin.gate_id
                && auth.gate_digest == pin.gate_sha256
                && publication.transaction_id == request.publication_tx
                && publication.revision_id == subject.candidate_revision,
            "publication authorization/transaction differs from the exact gate"
        );
        let entry = self
            .journal_entry(&transaction)?
            .context("publication journal is missing")?;
        let finished = entry
            .finished_at
            .context("committed journal has no historical completion time")?;
        let baseline_id = subject
            .baseline_revision
            .as_ref()
            .context("new-skill adoption is unsupported")?;
        let baseline_digest = entry
            .predecessor_record_digest
            .as_ref()
            .context("publication has no pinned predecessor record")?;
        let next_generation = proposal
            .expected_generation
            .checked_add(1)
            .context("publication generation overflow")?;
        ensure!(
            entry.owner_scope == *self.owner_scope()
                && entry.schema_version == crate::skills::lifecycle::SCHEMA_VERSION
                && entry.kind == TransactionKind::Publish
                && entry.state == TransactionState::Committed
                && entry.enabled
                && entry.transaction_id == transaction
                && entry.skill_id == subject.skill_id
                && entry.predecessor_revision_id.as_ref() == Some(baseline_id)
                && baseline_digest == &origin.pins.baseline_record_digest
                && entry.candidate_revision_id == subject.candidate_revision
                && entry.candidate_record_digest == auth.candidate_record_digest
                && entry.authorization_digest.as_deref()
                    == Some(authorization_pin(&proposal.proposal_id, auth).as_str())
                && entry.expected_generation == proposal.expected_generation
                && entry.new_generation == next_generation
                && publication.generation == next_generation
                && publication.committed_at == finished,
            "publication is legacy, unfinalized or bound to different immutable inputs"
        );
        let candidate = self.read_revision(
            &subject.skill_id,
            &subject.candidate_revision,
            Some(&auth.candidate_record_digest),
        )?;
        let baseline = self.read_revision(&subject.skill_id, baseline_id, Some(baseline_digest))?;
        ensure!(
            candidate
                .revision
                .proposal_id
                .as_ref()
                .is_none_or(|id| id == &proposal.proposal_id),
            "candidate revision belongs to another proposal"
        );
        let review = self.require_source_review(&proposal)?;
        ensure!(!review.skill_moved, "source review recorded a moved skill");
        let decided = history.gate_decided_at();
        let expires = history.gate_expires_at();
        let checked = history.verification_window();
        if review.reviewed_at > decided
            || decided > auth.authorized_at
            || checked.finished_at > auth.authorized_at
            || auth.authorized_at > entry.prepared_at
            || entry.prepared_at > finished
            || auth.authorized_at > expires
            || entry.prepared_at > expires
        {
            return Err(GateError::InsufficientTemporalEvidence(
                "publication authorization/first preparation has no consistent original gate window".into(),
            )
            .into());
        }
        ensure!(
            request.expires_at > finished,
            "adoption eligibility deadline must follow publication"
        );
        let suite = self.read_suite(&origin.pins.suite_digest)?;
        ensure!(
            !request.task_family.is_empty()
                && request.task_family.len() <= 128
                && !request.task_family.chars().any(char::is_control)
                && suite.suite().owner_scope == *self.owner_scope()
                && suite.suite().skill_id == subject.skill_id
                && suite
                    .suite()
                    .cases
                    .iter()
                    .any(|case| case.task_family == request.task_family),
            "adoption family is not part of the actual registered origin suite"
        );
        let diff = proposal
            .diff
            .as_ref()
            .context("adoption has no verified instruction diff")?;
        let original = baseline.bundle.file("SKILL.md").context("origin has no SKILL.md")?;
        let reapplied = crate::skills::lifecycle::propose::reapply(
            crate::skills::lifecycle::propose::Mode::Revise,
            std::str::from_utf8(original.bytes()).context("origin instructions are not UTF-8")?,
            &baseline.revision.description,
            diff,
        )
        .map_err(anyhow::Error::msg)?;
        ensure!(
            candidate.bundle.files().len() == baseline.bundle.files().len()
                && candidate
                    .bundle
                    .files()
                    .iter()
                    .zip(baseline.bundle.files())
                    .all(|(new, old)| {
                        new.path() == old.path()
                            && new.is_executable() == old.is_executable()
                            && if new.path() == "SKILL.md" {
                                new.bytes() == reapplied.skill_md.as_bytes()
                            } else {
                                new.bytes() == old.bytes()
                            }
                    })
                && reapplied.changed_tokens == diff.changed_tokens
                && reapplied.baseline_tokens == diff.baseline_tokens
                && reapplied.allowed_tokens == diff.allowed_tokens
                && reapplied.changed_tokens <= reapplied.allowed_tokens
                && reapplied.changed_bytes <= reapplied.allowed_bytes,
            "adopted guidance is not the original bounded instruction-only edit program"
        );
        let source = SupportedGuidance {
            edits: diff
                .edits
                .iter()
                .map(|edit| LessonEdit {
                    op: match edit.op {
                        EditOp::Replace => "replace",
                        EditOp::Append => "append",
                    }
                    .into(),
                    old: edit.old.clone(),
                    new: edit.new.clone(),
                })
                .collect(),
            description: diff.description_change.as_ref().map(|change| change.new.clone()),
        };
        source.render()?;
        let excluded_roots = suite.lesson_root_digests();
        ensure!(
            !excluded_roots.is_empty(),
            "origin task/source provenance is unavailable"
        );
        let bundle_bytes = candidate
            .bundle
            .files()
            .iter()
            .try_fold(0_u64, |sum, file| {
                sum.checked_add(u64::try_from(file.bytes().len()).ok()?)
            })
            .context("adopted bundle size overflow")?;
        // This observation is current read-only evidence, not a field that
        // changes the immutable annotation when a later publication supersedes it.
        self.observe_committed_publication_locked(owner, &entry)?;
        let evidence = serde_json::json!({
            "domain":"prx-committed-adoption-evidence-v1", "owner":self.owner_scope(),
            "proposal_id":proposal.proposal_id, "authorization":auth, "publication":publication,
            "journal":entry, "source_review_facts":review, "origin_experiment":origin.id,
            "experiment_sha256":journal.plan.digest(), "experiment_head":journal.head_sha256,
            "gate_review":history.pin(), "subject":subject, "physical_gate":history.physical(),
            "verification_window":checked, "gate_decided_at":decided, "gate_expires_at":expires,
            "compatibility":compatibility, "excluded_roots":excluded_roots, "guidance":source,
            "complete_root_cost":root_cost,
            "complete_root_cost_unknown":root_cost.is_none(),
        });
        let identity = serde_json::json!([
            "prx-adoption-identity-v1",
            self.owner_scope(),
            proposal.proposal_id,
            request.publication_tx,
            request.task_family
        ]);
        Ok(VerifiedAdoptionRecord(AdoptionRecord {
            schema_version: 1,
            owner_scope: self.owner_scope().clone(),
            adoption_id: format!("adopt-{}", digest(&serde_json::to_vec(&identity)?)),
            skill_id: subject.skill_id.clone(),
            task_family: request.task_family.clone(),
            proposal_id: proposal.proposal_id,
            publication_tx: request.publication_tx.clone(),
            origin_baseline: baseline_id.clone(),
            adopted_revision: subject.candidate_revision.clone(),
            adopted_record_sha256: candidate.record_digest,
            adopted_bundle_bytes: bundle_bytes,
            experiment_id: request.experiment_id.clone(),
            experiment_sha256: request.experiment_sha256.clone(),
            gate_review_pin: serde_json::to_value(history.pin())?,
            evidence_pin_sha256: digest(&serde_json::to_vec(&evidence)?),
            compatibility: compatibility.clone(),
            excluded_roots,
            source,
            root_cost_evidence_sha256: root_cost.map(AdoptionRootCostEvidence::digest).transpose()?,
            root_cost_evidence: root_cost.cloned(),
            committed_at: finished,
            expires_at: request.expires_at,
        }))
    }
}

impl SkillLifecycleStore {
    /// Pure existing-owner snapshot. Missing old evidence remains Unknown;
    /// contradictions, partial liabilities and overrun never become completeness.
    fn adoption_root_cost_evidence(
        &self,
        config: &crate::config::Config,
        journal: &crate::skills::lifecycle::experiment::ExperimentJournal,
        history: &crate::skills::lifecycle::VerifiedGateReviewHistory,
    ) -> Result<Option<AdoptionRootCostEvidence>> {
        use crate::skills::lifecycle::experiment::{ExperimentResources, ExperimentStageKind, StageState};
        use anyhow::Context as _;
        use std::collections::{BTreeMap, BTreeSet};
        ensure!(
            journal.state.qualified(&journal.plan),
            "root execution is not fully settled and cleaned"
        );
        let workspace = self
            .root()
            .parent()
            .and_then(std::path::Path::parent)
            .context("adoption store has no owner workspace")?;
        ensure!(
            workspace.canonicalize()? == config.workspace_dir.canonicalize()?,
            "cost snapshot uses another owner workspace"
        );
        // load_existing does not mint a key or reinterpret missing history as a
        // fresh zero-cost root. Actual current evidence retains the original key.
        let key = crate::decision::DecisionKey::load_existing(workspace)?;
        let scopes: Vec<_> = journal
            .plan
            .spec()
            .stages
            .iter()
            .map(|stage| {
                hex::encode(
                    crate::runtime::experiment_stage::ExperimentStageRuntime::physical_identity(
                        &key,
                        journal.plan.digest(),
                        &stage.id,
                    ),
                )
            })
            .collect();
        let Some(physical) = crate::cost::tracker::physical_scopes_read_only(workspace, &scopes)? else {
            return Ok(None);
        };
        let model_set: BTreeSet<_> = physical
            .values()
            .flat_map(|scope| scope.models.iter().cloned())
            .collect();
        let models: Vec<_> = model_set.into_iter().collect();
        let mut seen = BTreeSet::new();
        let mut total = ExperimentResources::default();
        let mut stages = Vec::new();
        let mut missing_historical_scope = false;
        for (declared, scope) in journal.plan.spec().stages.iter().zip(&scopes) {
            let Some(StageState::Settled {
                usage,
                successful: true,
                cleanup_confirmed: true,
                ..
            }) = journal.state.stages.get(&declared.id)
            else {
                anyhow::bail!("root stage is unresolved or uncleaned");
            };
            ensure!(
                usage.within(declared.allocation),
                "root stage exceeded its original allocation"
            );
            let actual = physical
                .get(scope)
                .context("physical snapshot omitted a requested scope")?;
            ensure!(
                !actual.unresolved && actual.calls == actual.settled_calls,
                "root still has unknown liability"
            );
            let model_stage = matches!(
                declared.kind,
                ExperimentStageKind::ModelProposal | ExperimentStageKind::SolverTrial | ExperimentStageKind::GateReview
            );
            if actual.calls == 0 && usage.calls > 0 {
                // Archived/pruned legacy rows are Unknown, never valid zero.
                missing_historical_scope = true;
                continue;
            }
            ensure!(
                actual.calls == usage.calls
                    && actual.tokens == usage.tokens
                    && actual.micro_usd == usage.micro_usd
                    && actual.attempt_ids.len() == usize::try_from(actual.calls)?
                    && ((model_stage && actual.calls > 0 && !actual.models.is_empty())
                        || (!model_stage && actual.calls == 0 && actual.models.is_empty())),
                "sealed stage usage differs from its original actual physical owner"
            );
            if declared.id == history.binding().experiment.stage {
                let gate = history.physical();
                ensure!(
                    history.binding().physical_scope_id == *scope
                        && gate.calls == actual.calls
                        && gate.settled_calls == actual.settled_calls
                        && gate.tokens == actual.tokens
                        && gate.micro_usd == actual.micro_usd
                        && gate.attempt_ids.iter().cloned().collect::<BTreeSet<_>>() == actual.attempt_ids
                        && gate.models == actual.models,
                    "root gate scope is not its actual sealed GateReview owner"
                );
            }
            let mut attempts: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for id in &actual.attempt_ids {
                ensure!(seen.insert(id.clone()), "root repeats a parent/child physical attempt");
                let (prefix, suffix) = id.rsplit_once(':').map_or_else(
                    || (String::new(), id.clone()),
                    |(prefix, suffix)| (format!("{prefix}:"), suffix.to_owned()),
                );
                attempts.entry(prefix).or_default().push(suffix);
            }
            for ids in attempts.values_mut() {
                ids.sort();
            }
            let mut model_indices = Vec::new();
            for model in &actual.models {
                let index = models
                    .binary_search(model)
                    .map_err(|_| anyhow::anyhow!("physical model table is inconsistent"))?;
                model_indices.push(u32::try_from(index)?);
            }
            total = total.checked_add(*usage)?;
            stages.push(AdoptionStageCostEvidence {
                stage: declared.id.clone(),
                physical_scope_id: scope.clone(),
                usage: [
                    usage.calls,
                    usage.tokens,
                    usage.micro_usd,
                    usage.tool_calls,
                    usage.output_bytes,
                    usage.scratch_bytes,
                ],
                attempts,
                model_indices,
            });
        }
        if missing_historical_scope {
            return Ok(None);
        }
        ensure!(
            total.within(journal.plan.spec().resources) && seen.len() == usize::try_from(total.calls)?,
            "root physical coverage exceeds or double-counts its original allowance"
        );
        Ok(Some(AdoptionRootCostEvidence {
            schema_version: 1,
            owner_scope: self.owner_scope().clone(),
            experiment_id: journal.plan.spec().id.as_str().into(),
            experiment_sha256: journal.plan.digest().into(),
            experiment_head_sha256: journal.head_sha256.clone(),
            usage: total,
            models,
            stages,
        }))
    }
}
