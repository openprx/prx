//! Genuine Committed -> bounded lookup -> prepared editor -> owned HTTP path.
//! All model behavior is deterministic loopback; no Prepared or success record
//! constructor, publication bypass or production profile exception is used.
use super::*;
use crate::skills::lifecycle::adoption::{LessonLimits, LessonQuery, LessonSnapshot, LessonSnapshotEntry};
use crate::skills::lifecycle::{LessonRef, RecordAdoptionRequest};
use sha2::Digest as _;
use std::collections::BTreeSet;

fn auto_service(
    f: &Fixture,
    suite: &crate::skills::lifecycle::FrozenSuite,
    config: &Config,
) -> Result<Arc<SkillsLifecycleService>> {
    let reviewed = gate_review::reviewed_service(f, suite, config, None)?;
    let mut settings = reviewed.settings().clone();
    settings.mode = LifecycleMode::Auto;
    assert!(settings.publication_block().is_none());
    let service = Arc::new(SkillsLifecycleService::new(f.store.clone(), settings)?);
    service.install_operator_suite(suite)?;
    Ok(service)
}
fn binding(config: &Config) -> ModelBinding {
    ModelBinding {
        provider_label: config.default_provider.clone().unwrap(),
        model: "experiment-loopback".into(),
        temperature_milli: 100,
    }
}
async fn donor(
    f: &Fixture,
    config: &Config,
) -> Result<(
    Arc<SkillsLifecycleService>,
    InstructionCycleDraft,
    ExperimentJournal,
    RecordAdoptionRequest,
)> {
    // Two actual origin families isolate the orphan-fault identity from the
    // positive adoption, with the same original cases/calls/root allocation.
    let mut origin_suite = suite(f).suite().clone();
    origin_suite
        .cases
        .first_mut()
        .context("origin suite is empty")?
        .task_family = "deploy-fault".into();
    let service = auto_service(f, &origin_suite.freeze()?, config)?;
    let pattern = usable_pattern(f);
    let draft = gate_review::draft(f, &service, config, pattern.pattern_id)?;
    let plan = service.plan_instruction_cycle(draft.clone(), config)?;
    service.admit_experiment(plan.spec.clone(), config)?;
    let cancel = CancellationToken::new();
    let journal = CleanupScope::new(None, cancel.clone())?
        .run(service.run_instruction_plan(config, &plan, &cancel))
        .await?;
    assert!(service.experiment_qualified(&journal)?);
    let witness = gate_review::witness(f, &journal)?;
    let request = RecordAdoptionRequest {
        proposal_id: witness.subject.proposal_id.clone(),
        publication_tx: crate::skills::lifecycle::authorize::publication_tx(&witness.subject.proposal_id, 1)?
            .to_string(),
        experiment_id: journal.plan.spec().id.as_str().into(),
        experiment_sha256: journal.plan.digest().into(),
        gate_stage: "gate-review".into(),
        gate_witness_sha256: witness.digest()?,
        gate_id: witness.gate_id,
        gate_sha256: witness.gate_digest,
        task_family: "deploy".into(),
        expires_at: Utc::now() + chrono::Duration::hours(1),
    };
    Ok((service, draft, journal, request))
}
fn fresh_suite(f: &Fixture) -> crate::skills::lifecycle::FrozenSuite {
    let mut next = suite(f).suite().clone();
    next.suite_id = "release-preflight-suite".into();
    for (number, case) in next.cases.iter_mut().enumerate() {
        case.case_id = format!("new-release-{number}");
        case.prompt = format!(
            "New release preflight task {number}: Use skill_read to read Deploy helper, check release preflight guidance, and report the migration result."
        );
    }
    next.freeze().unwrap()
}
fn query(
    f: &Fixture,
    config: &Config,
    journal: &ExperimentJournal,
    suite: &crate::skills::lifecycle::FrozenSuite,
) -> Result<LessonQuery> {
    let pins = &journal.plan.spec().pins;
    let active = f
        .store
        .pin_active(&f.pin.skill_id)?
        .context("committed baseline missing")?;
    let now = Utc::now();
    Ok(LessonQuery {
        owner_scope: f.store.owner_scope().clone(),
        task_family: "deploy".into(),
        skill_id: f.pin.skill_id.clone(),
        baseline_revision: active.revision_id,
        baseline_record_sha256: active.record_digest,
        compatibility: f.store.lesson_profile_compatibility(
            &binding(config),
            &pins.policy_sha256,
            &pins.binary_sha256,
        )?,
        excluded_roots: suite.lesson_root_digests(),
        limits: LessonLimits::default(),
        planned_at: now,
        expires_at: now + chrono::Duration::minutes(5),
    })
}
fn record_path(f: &Fixture, reference: &LessonRef) -> std::path::PathBuf {
    f.store
        .root()
        .join("adoptions/records")
        .join(format!("{}.json", reference.adoption_id))
}
fn confirm(f: &Fixture, snapshot: &LessonSnapshot, q: &LessonQuery, now: chrono::DateTime<Utc>) -> Result<()> {
    let _guard = f.store.lock_store(true, Duration::from_secs(5))?;
    Ok(f.store.validate_lesson_snapshot_locked(
        snapshot,
        &q.skill_id,
        &q.baseline_revision,
        &q.baseline_record_sha256,
        &q.compatibility,
        &q.excluded_roots,
        now,
    )?)
}
fn next_pattern(f: &Fixture) -> KnowledgePattern {
    let a = failing(f, "new-preflight-failure-1");
    let b = failing(f, "new-preflight-failure-2");
    consolidate(
        f,
        "The deploy skill omits the release preflight step.",
        FailureClass::MissingGuidance,
        &[&a, &b],
    )
}
fn delivery_records(f: &Fixture) -> Result<Vec<serde_json::Value>> {
    let directory = f.store.root().join("lesson-deliveries");
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut paths = std::fs::read_dir(directory)?
        .map(|entry| Ok(entry?.path()))
        .collect::<Result<Vec<_>>>()?;
    paths.sort();
    paths
        .into_iter()
        .map(|path| Ok(serde_json::from_slice(&std::fs::read(path)?)?))
        .collect()
}

// Explicit synthetic capture input, associated with an already executed run
// only for retention. Unknown/default facts assert no new execution success.
fn retention_capture(f: &Fixture, task: &str, run: Option<&str>) -> Result<String> {
    let correlation = Correlation {
        run_id: run.map(str::to_owned),
        ..Default::default()
    };
    f.store.record_selection(task, &f.pin, &correlation)?;
    f.store.join_terminal_with(
        task,
        "retention-fixture-terminal",
        ExperienceOutcome::Unknown,
        &TerminalFacts::default(),
    )?;
    Ok(crate::skills::lifecycle::experience_id(f.store.owner_scope(), task))
}

#[tokio::test]
async fn committed_origin_cost_query_and_real_editor_delivery_survive_replay_and_retirement() -> Result<()> {
    let mut f = fixture();
    let server = Server::start(false).await?;
    let result = async {
        let mut config = config(&f, &server);
        let price=config.cost.prices.get_mut(&format!("{}/experiment-loopback",config.default_provider.as_deref().unwrap()))
            .context("fixture model pricing missing")?;
        price.input=1.0;price.output=1.0;
        let _profile = f.store.register_lesson_fixture_profile(&binding(&config))?;
        let (origin, draft, journal, mut request) = donor(&f, &config).await?;
        let before_publish = server.count.load(Ordering::SeqCst);
        assert!(
            origin.record_adoption(&request, &config).is_err(),
            "eligible/Shadow qualification cannot mint adopted guidance"
        );
        assert!(!f.store.root().join("adoptions").exists());
        // A real PREPARED crash is not adoption. Annotation must leave that
        // exact journal untouched; only an explicit publisher retry may settle it.
        crate::skills::lifecycle::publish::crash_hooks::arm("after_prepared");
        let interrupted = origin.publish(&request.proposal_id);
        crate::skills::lifecycle::publish::crash_hooks::disarm();
        assert!(interrupted.is_err());
        let first_tx = TransactionId::parse(&request.publication_tx)?;
        let prepared = f
            .store
            .journal_entry(&first_tx)?
            .context("real Prepared publication missing")?;
        assert_eq!(prepared.state, crate::skills::lifecycle::TransactionState::Prepared);
        let frozen_prepared = serde_json::to_vec(&prepared)?;
        assert!(origin.record_adoption(&request, &config).is_err());
        assert_eq!(
            serde_json::to_vec(&f.store.journal_entry(&first_tx)?.unwrap())?,
            frozen_prepared
        );
        assert!(!f.store.root().join("adoptions").exists());
        assert!(
            origin.publish(&request.proposal_id).is_err(),
            "explicit retry closes the unswapped attempt as aborted"
        );
        assert_eq!(
            f.store.journal_entry(&first_tx)?.unwrap().state,
            crate::skills::lifecycle::TransactionState::Aborted
        );
        let publication = origin.publish(&request.proposal_id)?;
        request.publication_tx = publication
            .proposal
            .publication
            .as_ref()
            .unwrap()
            .transaction_id
            .clone();
        let review = f.store.proposal_file(&request.proposal_id, "source_review.json");
        let detached = review.with_file_name("source_review.temporarily-absent");
        std::fs::rename(&review, &detached)?;
        let absent = origin.record_adoption(&request, &config);
        std::fs::rename(&detached, &review)?;
        assert!(
            absent.is_err(),
            "missing immutable source review must not be silently reconstructed"
        );
        assert!(!f.store.root().join("adoptions").exists());
        assert_eq!(publication.proposal.status, Status::Committed);
        assert_eq!(
            publication.proposal.publication.as_ref().unwrap().transaction_id,
            request.publication_tx
        );
        let reference = retention_barrier_faults_then_record(&f, &origin, &request, &config)?;
        assert_eq!(origin.record_adoption(&request, &config)?, reference);
        assert_eq!(
            server.count.load(Ordering::SeqCst),
            before_publish,
            "annotation and idempotent retry cannot judge again"
        );
        let saved = std::fs::read(record_path(&f, &reference))?;
        let record: serde_json::Value = serde_json::from_slice(&saved)?;
        let cost = &record["value"]["root_cost_evidence"];
        assert!(
            cost.is_object(),
            "a new fully observed root must not be labelled Unknown"
        );
        let usage: ExperimentResources = serde_json::from_value(cost["usage"].clone())?;
        assert_eq!(usage, journal.state.committed(&journal.plan)?);
        assert_eq!(usage.calls, u64::try_from(before_publish)?);
        assert!(usage.micro_usd>0,"priced real requests must contribute actual fees under the original allowance");
        assert_ne!(cost["models"].as_array().unwrap().as_slice(), &[] as &[serde_json::Value]);
        let mut ids = BTreeSet::new();
        let mut scopes = Vec::new();
        for stage in cost["stages"].as_array().unwrap() {
            scopes.push(stage["physical_scope_id"].as_str().unwrap().to_owned());
            for (prefix, suffixes) in stage["attempts"].as_object().unwrap() {
                for suffix in suffixes.as_array().unwrap() {
                    assert!(ids.insert(format!("{prefix}{}", suffix.as_str().unwrap())));
                }
            }
        }
        assert_eq!(
            u64::try_from(ids.len())?,
            usage.calls,
            "parent and child attempts must be counted once"
        );
        let actual = crate::cost::tracker::physical_scopes_read_only(f.tmp.path(), &scopes)?
            .context("new root ledger disappeared")?;
        assert_eq!(actual.values().map(|row| row.calls).sum::<u64>(), usage.calls);
        assert_eq!(actual.values().map(|row| row.tokens).sum::<u64>(), usage.tokens);
        assert_eq!(actual.values().map(|row| row.micro_usd).sum::<u64>(), usage.micro_usd);
        assert!(
            actual
                .values()
                .all(|row| !row.unresolved && row.calls == row.settled_calls)
        );
        let reservations = config.workspace_dir.join("state/costs.reservations.jsonl");
        let original_ledger = std::fs::read(&reservations)?;
        std::fs::write(&reservations, b"corrupt reservation evidence\n")?;
        let corrupt = origin.record_adoption(&request, &config);
        assert_eq!(
            std::fs::read(&reservations)?,
            b"corrupt reservation evidence\n",
            "reader must not repair malformed evidence"
        );
        std::fs::write(&reservations, &original_ledger)?;
        assert!(corrupt.is_err());
        assert_eq!(std::fs::read(record_path(&f, &reference))?, saved);
        let detached = reservations.with_file_name("costs.reservations.temporarily-absent");
        std::fs::rename(&reservations, &detached)?;
        let missing = crate::cost::tracker::physical_scopes_read_only(f.tmp.path(), &scopes);
        std::fs::rename(&detached, &reservations)?;
        assert!(missing?.is_none(), "missing old evidence is Unknown, not complete zero");
        let mut conflict = request.clone();
        conflict.expires_at += chrono::Duration::seconds(1);
        assert!(origin.record_adoption(&conflict, &config).is_err());
        let mut wrong = request.clone();
        wrong.gate_sha256 = "0".repeat(64);
        assert!(origin.record_adoption(&wrong, &config).is_err());
        assert_eq!(std::fs::read(record_path(&f, &reference))?, saved);
        assert!(
            origin.experiment_qualified(&journal)?,
            "Committed/current-baseline changes do not revoke sealed history"
        );
        assert!(
            f.store
                .verify_gate(
                    &request.gate_id,
                    &request.gate_sha256,
                    &origin.settings().gate_policy,
                    &journal.plan.spec().pins.suite_digest
                )
                .is_err(),
            "current publication authority is separate"
        );
        let next_suite = fresh_suite(&f);
        let q = query(&f, &config, &journal, &next_suite)?;
        let snapshot = f.store.query_adopted_lessons(&q)?;
        assert_eq!(snapshot.entries.len(), 1);
        assert_eq!(snapshot.entries[0].reference, reference);
        confirm(&f, &snapshot, &q, Utc::now())?;
        let block = crate::skills::lifecycle::adoption::format_lesson_block(&snapshot, "general")?;
        assert_eq!(
            block,
            crate::skills::lifecycle::adoption::format_lesson_block(&snapshot, "general")?
        );
        assert!(block.is_ascii() && block.len() + 320 <= 1536);
        let mut overlap = q.clone();
        overlap.excluded_roots = suite(&f).lesson_root_digests();
        assert_eq!(f.store.query_adopted_lessons(&overlap)?.entries.as_slice(), &[] as &[LessonSnapshotEntry]);
        let mut family = q.clone();
        family.task_family = "unrelated-family".into();
        assert_eq!(f.store.query_adopted_lessons(&family)?.entries.as_slice(), &[] as &[LessonSnapshotEntry]);
        let mut old = q.clone();
        old.baseline_revision = f.pin.revision_id.clone();
        old.baseline_record_sha256 = f.pin.record_digest.clone();
        assert!(
            f.store.query_adopted_lessons(&old)?.entries.is_empty(),
            "origin baseline is not the adopted target baseline"
        );
        for component in 0..3 {
            let mut capped = q.clone();
            match component {
                0 => capped.limits.index_bytes = 1,
                1 => capped.limits.record_bytes = 1,
                _ => capped.limits.decoded_bytes = 1,
            };
            assert!(
                f.store.query_adopted_lessons(&capped).is_err(),
                "cap exhaustion cannot return a partial usable snapshot"
            );
        }
        let mut forged = snapshot.clone();
        forged.entries[0].sanitized_guidance.push_str(" forged");
        forged.entries[0].guidance_sha256 =
            hex::encode(sha2::Sha256::digest(forged.entries[0].sanitized_guidance.as_bytes()));
        assert!(
            confirm(&f, &forged, &q, Utc::now()).is_err(),
            "self-consistent text/hash is not signed source authority"
        );
        let mut pin = snapshot.clone();
        pin.entries[0].reference.adoption_sha256 = "0".repeat(64);
        assert!(confirm(&f, &pin, &q, Utc::now()).is_err());
        assert!(confirm(&f, &snapshot, &q, snapshot.expires_at + chrono::Duration::seconds(1)).is_err());
        f.pin = f.store.pin_active(&f.pin.skill_id)?.unwrap();
        let pattern = next_pattern(&f);
        let next = auto_service(&f, &next_suite, &config)?;
        let mut next_draft = draft;
        next_draft.id = ExperimentId::parse("lesson-new-task-cycle")?;
        next_draft.patterns = vec![pattern.pattern_id];
        next_draft.lesson_query = Some(LessonQueryIntent {
            task_family: "deploy".into(),
        });
        let owned_draft = next_draft.clone();
        let next_plan = next.plan_instruction_cycle(next_draft, &config)?;
        assert_eq!(
            next_plan.spec.pins.lesson_snapshot.as_ref().unwrap().entries[0].reference,
            reference
        );
        server.h2_mode.store(9, Ordering::SeqCst);
        server.bodies.lock().clear();
        next.admit_experiment(next_plan.spec.clone(), &config)?;
        let cancel = CancellationToken::new();
        let completed = CleanupScope::new(None, cancel.clone())?
            .run(next.run_instruction_plan(&config, &next_plan, &cancel))
            .await?;
        assert!(next.experiment_qualified(&completed)?);
        let bodies = server.bodies.lock().clone();
        let mut editors = 0;
        for body in &bodies {
            let messages = body["messages"].as_array().unwrap();
            let editor = messages.iter().any(|m| {
                m["role"] == "system"
                    && m["content"]
                        .as_str()
                        .is_some_and(|s| s.contains("You revise one instruction skill"))
            });
            let text = messages
                .iter()
                .filter_map(|m| m["content"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if editor {
                editors += 1;
                assert_eq!(text.matches("BEGIN_PRX_LESSON").count(), 1);
                assert_eq!(text.matches("END_PRX_LESSON").count(), 1);
                for case in next_suite.suite().cases.iter().filter(|case| case.split != EvalSplit::Development) {
                    assert!(!text.contains(&case.case_id) && !text.contains(&case.prompt), "independent task evidence cannot enter an editor");
                }
            } else {
                assert!(
                    !text.contains("BEGIN_PRX_LESSON"),
                    "solver/independent split/judge cannot inherit editor guidance"
                );
                for user in messages.iter().filter(|message|message["role"]=="user").filter_map(|message|message["content"].as_str()) {
                    for entry in &next_plan.spec.pins.lesson_snapshot.as_ref().unwrap().entries {
                        assert!(!user.contains(&entry.sanitized_guidance), "non-editor logical user slots cannot carry the C05 projection; normal candidate SKILL/tool content remains allowed");
                    }
                }
            }
        }
        assert_eq!(editors, 3);
        let deliveries = delivery_records(&f)?;
        assert_eq!(deliveries.len(), 3);
        let mut attempts = BTreeSet::new();
        let mut bindings = BTreeSet::new();
        for delivery in &deliveries {
            let p = &delivery["payload"];
            assert_eq!(p["state"], "PossibleHandoffUnknown");
            assert_eq!(
                p["experiment"]["experiment_sha256"],
                next_plan.spec.clone().freeze()?.digest()
            );
            assert_eq!(p["refs"][0], serde_json::to_value(&reference)?);
            let stage=p["experiment"]["stage"].as_str().unwrap();
            let expected_block=crate::skills::lifecycle::adoption::format_lesson_block(next_plan.spec.pins.lesson_snapshot.as_ref().unwrap(),stage)?;
            let wire=bodies.iter().find(|body|body["messages"].as_array().unwrap().iter().any(|message|
                message["role"]=="user"&&message["content"].as_str().is_some_and(|text|text.contains(&expected_block))))
                .context("delivery has no matching actual native HTTP user slot")?;
            let users=wire["messages"].as_array().unwrap().iter().filter(|message|message["role"]=="user").collect::<Vec<_>>();
            assert_eq!(users.len(),1);
            let actual_user=users[0]["content"].as_str().context("user slot is not literal text")?;
            assert_eq!(p["final_context_sha256"],hex::encode(sha2::Sha256::digest(actual_user.as_bytes())));
            assert_eq!(p["block_sha256"],hex::encode(sha2::Sha256::digest(expected_block.as_bytes())));
            assert_eq!(wire["model"],"experiment-loopback");

            assert!(attempts.insert(p["attempt_id"].as_str().unwrap().to_owned()));
            assert!(bindings.insert(p["body_binding_sha256"].as_str().unwrap().to_owned()));
        }
        let bytes = std::fs::read(config.workspace_dir.join("state/costs.reservations.jsonl"))?;
        let sends = server.count.load(Ordering::SeqCst);
        let replay = CleanupScope::new(None, cancel.clone())?
            .run(next.run_instruction_plan(&config, &next_plan, &cancel))
            .await?;
        assert_eq!(replay.head_sha256, completed.head_sha256);
        assert_eq!(
            next.recover_experiment(&next_plan.spec.id, completed.plan.digest())?
                .head_sha256,
            completed.head_sha256
        );
        assert_eq!(server.count.load(Ordering::SeqCst), sends);
        assert_eq!(
            std::fs::read(config.workspace_dir.join("state/costs.reservations.jsonl"))?,
            bytes
        );
        {
            let owner = f.store.lock_store(false, Duration::from_secs(5))?;
            let refs = f.store.delivery_retention_refs_locked(
                &owner,
                crate::skills::lifecycle::lesson_delivery::DeliveryRetentionLimits {
                    max_records: 64,
                    max_decoded_bytes: 1024 * 1024,
                    deadline: std::time::Instant::now() + Duration::from_secs(5),
                },
            )?;
            assert_eq!(refs.refs, vec![reference.clone()]);
            assert_eq!(refs.examined_records, 3);
        }
        let probe_digest = completed.entries.iter().find_map(|entry| match &entry.event {
            ExperimentEvent::Settled { stage, reference: Some(reference), evidence_sha256, .. }
                if stage == "baseline" && reference == "probe:baseline" => Some(evidence_sha256),
            _ => None,
        }).context("retained root has no actual baseline probe pin")?;
        let probe = f.store.read_development_probe(&completed.plan.spec().id, "baseline", probe_digest)?;
        let probe_run = &probe.cases.first().context("retained probe has no executed case")?.run.run_id;
        let evaluation = holdout::evaluation(&f, &completed, "final-evaluation")?;
        let evaluation_run = &evaluation.cases.first().context("retained evaluation has no executed case")?.run.run_id;
        let proposals = f.store.list_proposals()?;
        assert!(proposals.iter().any(|proposal| proposal.bound_evaluation_attempts.contains(&evaluation.receipt.record_id)),
            "the selected real evaluation must have an existing C owner-local reference");
        let retained_captures = [
            retention_capture(&f, "retention-associated-real-probe", Some(probe_run))?,
            retention_capture(&f, "retention-associated-real-evaluation", Some(evaluation_run))?,
        ];
        let control = retention_capture(&f, "retention-unreferenced-terminal-control", None)?;
        let legacy_patterns = f.store.retained_pattern_experiences()?;
        for id in retained_captures.iter().chain(std::iter::once(&control)) {
            assert!(!legacy_patterns.contains(id) && proposals.iter().all(|proposal| !proposal.evidence_ids.contains(id)),
                "these captures must not be retained by a legacy pattern/proposal chain");
            assert!(f.store.read_experience_by_id(id)?.is_some());
        }
        let revision_before = f.store.read_revision(
            &f.pin.skill_id,
            &reference.adopted_revision,
            Some(&reference.adopted_record_sha256),
        )?;
        f.store.prune_unreferenced_experiences(
            &crate::skills::lifecycle::ExperienceRetention {
                terminal_max_age: chrono::Duration::zero(),
                incomplete_max_age: chrono::Duration::zero(),
            },
            Utc::now() + chrono::Duration::days(1),
        )?;
        assert_eq!(
            f.store
                .read_revision(
                    &f.pin.skill_id,
                    &reference.adopted_revision,
                    Some(&reference.adopted_record_sha256)
                )?
                .record_digest,
            revision_before.record_digest
        );
        for id in &retained_captures {
            assert!(f.store.read_experience_by_id(id)?.is_some(),
                "actual retained probe/evaluation run references must protect expired terminal captures");
        }
        assert!(f.store.read_experience_by_id(&control)?.is_none(),
            "same-API expired terminal capture with no retained run reference must be deleted");
        let next_witness = gate_review::witness(&f, &completed)?;
        let published = next.publish(&next_witness.subject.proposal_id)?;
        let next_digest = next_witness.digest()?;
        let next_request = RecordAdoptionRequest {
            proposal_id: next_witness.subject.proposal_id,
            publication_tx: published.proposal.publication.as_ref().unwrap().transaction_id.clone(),
            experiment_id: completed.plan.spec().id.as_str().into(),
            experiment_sha256: completed.plan.digest().into(),
            gate_stage: "gate-review".into(),
            gate_witness_sha256: next_digest,
            gate_id: next_witness.gate_id,
            gate_sha256: next_witness.gate_digest,
            task_family: "deploy".into(),
            expires_at: request.expires_at,
        };
        let second = next.record_adoption(&next_request, &config)?;
        assert_ne!(second.adopted_revision, reference.adopted_revision);
        let mut third = fresh_suite(&f).suite().clone();
        third.suite_id = "independent-owner-task-population".into();
        for (number, case) in third.cases.iter_mut().enumerate() {
            case.prompt = format!("Post-release integrity task {number}: Use skill_read to read Deploy helper and check package signing and rollback readiness, then report the migration result.");
        }
        let third = third.freeze()?;
        // Whichever genuine identity sorts later must survive an incompatible
        // earlier record. Output limiting cannot spend its slot on that record.
        let eligible = if reference.adoption_id > second.adoption_id {
            &reference
        } else {
            &second
        };
        let mut limited = query(&f, &config, &journal, &third)?;
        limited.baseline_revision = eligible.adopted_revision.clone();
        limited.baseline_record_sha256 = eligible.adopted_record_sha256.clone();
        limited.limits.lessons = 1;
        let selected = f.store.query_adopted_lessons(&limited)?;
        assert_eq!(selected.entries.len(), 1);
        assert_eq!(selected.entries[0].reference, *eligible);
        assert!(
            selected.stats.examined >= 2 && selected.stats.excluded >= 1,
            "eligibility must precede the one-result limit"
        );
        f.pin = f.store.pin_active(&f.pin.skill_id)?.unwrap();
        let owner_service = auto_service(&f, &third, &config)?;
        owned_attempts_then_retire(&f, &server, &config, &next, &owner_service, owned_draft, &second).await?;
        origin.set_adoption_status(&reference, true, false)?;
        let sends = server.count.load(Ordering::SeqCst);
        assert_eq!(f.store.query_adopted_lessons(&q)?.entries.as_slice(), &[] as &[LessonSnapshotEntry]);
        assert!(
            confirm(&f, &snapshot, &q, Utc::now()).is_err(),
            "retirement between planning and handoff must revoke the snapshot"
        );
        origin.set_adoption_status(&reference, false, false)?;
        assert!(
            f.store.query_adopted_lessons(&q)?.entries.is_empty(),
            "false cannot revive a retired origin"
        );
        assert_eq!(server.count.load(Ordering::SeqCst), sends);
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

async fn owned_attempts_then_retire(
    f: &Fixture,
    server: &Server,
    config: &Config,
    origin: &Arc<SkillsLifecycleService>,
    target: &Arc<SkillsLifecycleService>,
    mut draft: InstructionCycleDraft,
    reference: &LessonRef,
) -> Result<()> {
    draft.id = ExperimentId::parse("prepared-owned-attempts")?;
    let plan = target.plan_instruction_cycle(draft, config)?;
    let frozen = plan.spec.clone().freeze()?;
    server.h2_mode.store(9, Ordering::SeqCst);
    target.admit_experiment(plan.spec.clone(), config)?;
    let mut journal = run_stage(&target, config, &frozen, "baseline", &plan.operations["baseline"]).await?;
    for (name, event) in [
        (
            "reserve",
            ExperimentEvent::Reserved {
                stage: "general".into(),
            },
        ),
        (
            "dispatch",
            ExperimentEvent::Dispatched {
                stage: "general".into(),
            },
        ),
    ] {
        journal = f.store.append_experiment(
            &plan.spec.id,
            frozen.digest(),
            journal.next_sequence(),
            &journal.head_sha256,
            &format!("owned-editor-{name}"),
            event,
            Utc::now(),
        )?;
    }
    let cancel = CancellationToken::new();
    let cleanup = CleanupScope::new(None, cancel.clone())?;
    let runtime = cleanup
        .scope_admission(async {
            crate::runtime::experiment_stage::ExperimentStageRuntime::new(
                config,
                &frozen,
                "general",
                cancel.clone(),
                f.store.clone(),
            )
        })
        .await?;
    let block = crate::skills::lifecycle::adoption::format_lesson_block(
        plan.spec.pins.lesson_snapshot.as_ref().unwrap(),
        "general",
    )?;
    let user = format!("Fixture release editor task.\n{block}");
    let model = crate::runtime::evolution_model::EvolutionModel::from_config(
        config,
        crate::runtime::evolution_model::EvolutionPurpose::Proposer,
    )?;
    let scope = runtime.scope_id();
    cleanup.run(runtime.scope(async {
            let prepared=target.prepare_lesson_context(&frozen,"general",&plan.operations["general"],&user)
                .await?.context("genuine factory returned no context")?;
            assert_eq!(prepared.block(),block);assert_eq!(prepared.refs(),std::slice::from_ref(reference));
            assert_eq!(prepared.expected_user_slot_bytes(),u64::try_from(user.len())?);
            let before=server.count.load(Ordering::SeqCst);
            for _ in 0..2 {
                let tally=Arc::new(TallyCounters::default());
                let reply=crate::runtime::lesson_delivery::scope(prepared.clone(),model.complete(
                    "You revise one instruction skill. H2 role: general.",&user,tally.clone())).await?;
                assert_eq!((tally.snapshot().physical_requests,tally.snapshot().settled_requests),(1,1));
                assert_eq!((reply.usage.physical_requests,reply.usage.settled_requests),(1,1));
            }
            assert_eq!(server.count.load(Ordering::SeqCst),before+2);
            let delivered=delivery_records(f)?.into_iter().filter(|record|record["payload"]["experiment"]["experiment_sha256"]==frozen.digest()).collect::<Vec<_>>();assert_eq!(delivered.len(),2);
            assert_ne!(delivered[0]["payload"]["attempt_id"],delivered[1]["payload"]["attempt_id"]);
            assert_ne!(delivered[0]["payload"]["body_binding_sha256"],delivered[1]["payload"]["body_binding_sha256"]);
            assert_eq!(delivered[0]["payload"]["final_context_sha256"],delivered[1]["payload"]["final_context_sha256"],
                "identical complete user bytes still require distinct physical attempt bindings");
            for item in &delivered {assert_eq!(item["payload"]["turn_id"],scope);assert_eq!(item["payload"]["refs"][0],serde_json::to_value(reference)?);}
            origin.set_adoption_status(reference,true,false)?;
            let tally=Arc::new(TallyCounters::default());
            let refused=crate::runtime::lesson_delivery::scope(prepared,model.complete(
                "You revise one instruction skill. H2 role: general.",&user,tally.clone())).await;
            assert!(matches!(refused,Err(ModelCallError::Failed{ref usage,..}) if usage.physical_requests==1&&usage.settled_requests==0));
            assert_eq!((tally.snapshot().physical_requests,tally.snapshot().settled_requests),(1,0));
            assert_eq!(server.count.load(Ordering::SeqCst),before+2,"retirement after preparation must refuse before wire");
            assert_eq!(delivery_records(f)?.into_iter().filter(|record|record["payload"]["experiment"]["experiment_sha256"]==frozen.digest()).count(),2,"refusal cannot fabricate a delivery witness");
            Ok(())
        })).await?;
    let physical = crate::cost::tracker::CostTracker::for_workspace(config.cost.clone(), f.tmp.path())?
        .physical_scope_usage(&scope)?;
    assert_eq!((physical.calls, physical.settled_calls), (3, 2));
    assert!(physical.unresolved && physical.tokens > 300 && physical.micro_usd > 0);
    assert!(crate::cost::tracker::physical_scopes_read_only(f.tmp.path(), &[scope]).is_err());
    journal = f.store.append_experiment(
        &plan.spec.id,
        frozen.digest(),
        journal.next_sequence(),
        &journal.head_sha256,
        "owned-editor-unknown",
        ExperimentEvent::DispatchUnknown {
            stage: "general".into(),
        },
        Utc::now(),
    )?;
    let raw = std::fs::read(config.workspace_dir.join("state/costs.reservations.jsonl"))?;
    let count = server.count.load(Ordering::SeqCst);
    assert_eq!(
        target.recover_experiment(&plan.spec.id, frozen.digest())?.head_sha256,
        journal.head_sha256
    );
    assert_eq!(
        std::fs::read(config.workspace_dir.join("state/costs.reservations.jsonl"))?,
        raw
    );
    assert_eq!(server.count.load(Ordering::SeqCst), count);
    assert!(
        f.store
            .prune_unreferenced_experiences(
                &crate::skills::lifecycle::ExperienceRetention {
                    terminal_max_age: chrono::Duration::zero(),
                    incomplete_max_age: chrono::Duration::zero()
                },
                Utc::now() + chrono::Duration::days(1)
            )
            .is_err(),
        "unresolved actual delivery root must stop unsafe pruning"
    );
    assert!(record_path(f, reference).is_file());
    Ok(())
}

// Exact schema copied from 4ac7efad's proposal.rs, with provenance retained in
// output/audits/prx-h2-m3-adoption-tests-20261030. This runs that old field-aware
// decoder; it does not claim execution of an old binary or a low-level pruner.
#[allow(dead_code)]
mod legacy_4ac {
    use crate::skills::lifecycle::proposal::{
        AttemptRecord, AuthorBinding, BoundedDiff, BudgetRecord, ProposalBasis, ProposalFailure,
        PublicationAuthorization, PublicationRecord,
    };
    use crate::skills::lifecycle::{OwnerScope, PatternId, ProposalOperation, ProposalStatus, RevisionId, SkillId};
    use chrono::{DateTime, Utc};
    use serde::{Deserialize, Serialize};
    include!("fixtures/skill_proposal_4ac.rs");
}

fn retention_barrier_faults_then_record(
    f: &Fixture,
    service: &SkillsLifecycleService,
    request: &RecordAdoptionRequest,
    config: &Config,
) -> Result<LessonRef> {
    let source = f.store.read_proposal(&request.proposal_id)?.unwrap();
    let unmarked = serde_json::to_vec(&source)?;
    assert!(serde_json::to_value(&source)?.get("c05_retention_format").is_none());
    let legacy: legacy_4ac::SkillProposal = serde_json::from_slice(&unmarked)?;
    assert_eq!(
        serde_json::to_vec(&legacy)?,
        unmarked,
        "None must preserve the exact pre-marker encoding"
    );
    crate::skills::lifecycle::publish::crash_hooks::arm("c05_before_retention_barrier");
    let before = service.record_adoption(request, config);
    crate::skills::lifecycle::publish::crash_hooks::disarm();
    assert!(before.is_err());
    assert_eq!(
        serde_json::to_vec(&f.store.read_proposal(&request.proposal_id)?.unwrap())?,
        unmarked
    );
    assert!(
        !f.store.root().join("adoptions").exists(),
        "reference cannot precede the downgrade barrier"
    );
    crate::skills::lifecycle::publish::crash_hooks::arm("c05_retention_barrier_confirmed");
    let interrupted = service.record_adoption(request, config);
    crate::skills::lifecycle::publish::crash_hooks::disarm();
    assert!(interrupted.is_err());
    let marked = f.store.read_proposal(&request.proposal_id)?.unwrap();
    assert_eq!(marked.c05_retention_format, Some(1));
    let mut visible = serde_json::to_value(&marked)?;
    visible.as_object_mut().unwrap().remove("c05_retention_format");
    assert_eq!(
        visible,
        serde_json::to_value(&source)?,
        "barrier must preserve auth/time/evidence and every historical subject field"
    );
    assert!(!f.store.root().join("adoptions").exists());
    let old = serde_json::from_slice::<legacy_4ac::SkillProposal>(&serde_json::to_vec(&marked)?);
    assert!(
        old.err()
            .context("old field-aware reader accepted a marked source")?
            .to_string()
            .contains("c05_retention_format")
    );
    {
        let owner = f.store.lock_proposals()?;
        for marker in [None, Some(2)] {
            let mut invalid = marked.clone();
            invalid.c05_retention_format = marker;
            assert!(
                f.store.write_proposal(&owner, &invalid).is_err(),
                "central writer cannot clear/change the durable barrier"
            );
        }
    }
    assert_eq!(f.store.read_proposal(&request.proposal_id)?.unwrap(), marked);
    // Execute the actual production collector refusal on an unsupported field
    // before it may delete a real expired capture. The exact old 4ac reader
    // above sees c05_retention_format itself as that unsupported field.
    let victim = failing(f, "retention-barrier-expired-victim");
    assert!(f.store.read_experience_by_id(&victim)?.is_some());
    let source_path = f.store.proposal_file(&request.proposal_id, "proposal.json");
    let retained_source = std::fs::read(&source_path)?;
    let mut unsupported: serde_json::Value = serde_json::from_slice(&retained_source)?;
    unsupported["unsupported_maintenance_format"] = json!(1);
    std::fs::write(&source_path, serde_json::to_vec(&unsupported)?)?;
    let refused = f.store.prune_unreferenced_experiences(
        &crate::skills::lifecycle::ExperienceRetention {
            terminal_max_age: chrono::Duration::zero(),
            incomplete_max_age: chrono::Duration::zero(),
        },
        Utc::now() + chrono::Duration::days(1),
    );
    std::fs::write(&source_path, &retained_source)?;
    assert!(
        refused.is_err(),
        "field-aware production collector must fail before deletion"
    );
    assert!(f.store.read_experience_by_id(&victim)?.is_some());

    let journal = f.store.read_experiment(
        &ExperimentId::parse(&request.experiment_id)?,
        &request.experiment_sha256,
    )?;
    assert!(
        service.experiment_qualified(&journal)?,
        "the added barrier cannot change sealed proposal/evaluation pins"
    );
    let mut fault_request = request.clone();
    fault_request.task_family = "deploy-fault".into();
    crate::skills::lifecycle::publish::crash_hooks::arm("c05_adoption_record_written");
    let orphan = service.record_adoption(&fault_request, config);
    crate::skills::lifecycle::publish::crash_hooks::disarm();
    assert!(orphan.is_err());
    assert_eq!(
        f.store
            .read_proposal(&request.proposal_id)?
            .unwrap()
            .c05_retention_format,
        Some(1)
    );
    let records = f.store.root().join("adoptions/records");
    let paths = std::fs::read_dir(&records)?
        .map(|entry| Ok(entry?.path()))
        .collect::<Result<Vec<_>>>()?;
    assert_eq!(
        paths.len(),
        1,
        "record may exist after its barrier, before its reference"
    );
    let orphan_bytes = std::fs::read(&paths[0])?;
    assert!(
        f.store
            .query_adopted_lessons(&query(f, config, &journal, &fresh_suite(f))?)?
            .entries
            .is_empty(),
        "an orphan record is retained evidence, not usable indexed guidance"
    );
    assert_eq!(std::fs::read(&paths[0])?, orphan_bytes);
    assert!(
        service.record_adoption(&fault_request, config).is_err(),
        "an existing orphan without its exact status index stays quarantined on retry"
    );
    assert_eq!(
        std::fs::read(&paths[0])?,
        orphan_bytes,
        "failed retry must preserve orphan evidence without resurrection"
    );
    // A separately registered real origin family gets its own adoption ID and
    // index. No orphan is deleted or fabricated into a successful record.
    let reference = service.record_adoption(request, config)?;
    assert_ne!(record_path(f, &reference), paths[0]);
    assert_eq!(std::fs::read(&paths[0])?, orphan_bytes);
    assert_eq!(service.record_adoption(request, config)?, reference);
    Ok(reference)
}
