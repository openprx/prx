//! Bounded k2 schedules through real runtime builders and scripted loopback HTTP.
//! A rendezvous in the parallel fixture requires actual simultaneous editor sends.
use super::instruction_cycle::cycle_with_schedule;
use super::*;

fn editors(server: &Server) -> Vec<serde_json::Value> {
    server
        .bodies
        .lock()
        .iter()
        .filter(|body| {
            body["messages"].as_array().is_some_and(|messages| {
                messages.iter().any(|message| {
                    message["role"] == "system"
                        && message["content"]
                            .as_str()
                            .is_some_and(|text| text.contains("You revise one instruction skill"))
                })
            })
        })
        .cloned()
        .collect()
}
fn choice(journal: &ExperimentJournal, name: &str) -> Option<String> {
    journal.entries.iter().find_map(|entry| match &entry.event {
        ExperimentEvent::Settled { stage, reference, .. } if stage == name => reference.clone(),
        _ => None,
    })
}

#[tokio::test]
async fn refinement_sequential_carries_exact_parent_and_retains_it_on_tie() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    let result = async {
        let config = config(&f, &server);
        let suite = suite(&f);
        let service = service(&f, &suite);
        let plan = cycle_with_schedule(
            &f,
            &service,
            &config,
            pattern.pattern_id,
            InstructionSchedule::Sequential2,
        )?;
        assert_eq!(plan.spec.stages.len(), 17);
        assert_eq!(plan.spec.max_concurrent, 1);
        service.admit_experiment(plan.spec.clone(), &config)?;
        let cancel = CancellationToken::new();
        let journal = CleanupScope::new(None, cancel.clone())?
            .run(service.run_instruction_plan(&config, &plan, &cancel))
            .await?;
        assert!(service.experiment_qualified(&journal)?);
        assert_eq!(choice(&journal, "select-round1").as_deref(), Some("selected:general"));
        assert_eq!(
            choice(&journal, "select"),
            choice(&journal, "select-round1"),
            "an equal second round must retain its parent"
        );
        let probes = service.experiment_development_probes(&journal)?;
        assert_eq!(probes.as_object().unwrap().len(), 7);
        assert_eq!(
            probes
                .as_object()
                .unwrap()
                .values()
                .map(|probe| probe["cases"].as_array().unwrap().len())
                .sum::<usize>(),
            21
        );
        assert_eq!(
            f.store.list_evaluation_ids()?.len(),
            1,
            "only final search result may use independent validation"
        );
        let editors = editors(&server);
        assert_eq!(editors.len(), 6);
        let parent_id = choice(&journal, "general").unwrap();
        let parent = f.store.read_proposal(&parent_id)?.unwrap();
        let bundle = f.store.read_candidate_bundle(&parent.proposal_id)?;
        for editor in editors.iter().skip(3) {
            let messages = editor["messages"].as_array().unwrap();
            let text = messages
                .iter()
                .filter_map(|message| message["content"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let context: serde_json::Value =
                serde_json::from_str(text.split("H2 observed development data:\n").nth(1).unwrap())?;
            assert_eq!(
                context["selected_parent"]["revision"],
                serde_json::to_value(&parent.candidate_revision)?
            );
            assert_eq!(context["selected_parent"]["diff"], serde_json::to_value(&parent.diff)?);
            assert_eq!(
                context["selected_parent"]["development_evidence"]["probe_stage"],
                "general-probe"
            );
            assert_eq!(
                context["selected_parent"]["development_evidence"]["cases"]
                    .as_array()
                    .unwrap()
                    .len(),
                3
            );
            assert_eq!(
                context["selected_parent"]["files"],
                json!([{"path":"SKILL.md", "content":std::str::from_utf8(bundle.file("SKILL.md").unwrap().bytes())?}]),
                "only the editable UTF-8 instructions enter parent context"
            );
            assert!(!text.contains("validation-0") && !text.contains("holdout-0"));
            assert!(text.contains("patch against the original baseline"));
        }
        let sends = server.count.load(Ordering::SeqCst);
        let repeated = CleanupScope::new(None, cancel.clone())?
            .run(service.run_instruction_plan(&config, &plan, &cancel))
            .await?;
        assert_eq!(repeated.head_sha256, journal.head_sha256);
        assert_eq!(server.count.load(Ordering::SeqCst), sends);
        assert_eq!(
            f.store.active_binding(&f.pin.skill_id)?.unwrap().generation,
            f.pin.generation
        );
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn refinement_round_two_replays_ordered_parent_patch_and_description_exactly() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    server.h2_mode.store(6, Ordering::SeqCst);
    let result = async {
        let config = config(&f, &server);
        let suite = suite(&f);
        let service = service(&f, &suite);
        let plan = cycle_with_schedule(
            &f,
            &service,
            &config,
            pattern.pattern_id,
            InstructionSchedule::Sequential2,
        )?;
        service.admit_experiment(plan.spec.clone(), &config)?;
        let cancel = CancellationToken::new();
        let journal = CleanupScope::new(None, cancel.clone())?
            .run(service.run_instruction_plan(&config, &plan, &cancel))
            .await?;
        assert!(
            service.experiment_qualified(&journal)?,
            "ordered parent replay must remain independently verifiable"
        );
        assert_eq!(choice(&journal, "select-round1").as_deref(), Some("selected:general"));
        assert_eq!(
            choice(&journal, "select"),
            choice(&journal, "select-round1"),
            "equal replay retains the first selected parent"
        );
        assert_eq!(
            f.store.list_evaluation_ids()?.len(),
            1,
            "only one final independent validation"
        );
        let probes = service.experiment_development_probes(&journal)?;
        assert_eq!(probes.as_object().unwrap().len(), 7);
        assert_eq!(
            probes
                .as_object()
                .unwrap()
                .values()
                .map(|probe| probe["cases"].as_array().unwrap().len())
                .sum::<usize>(),
            21
        );
        let editors = editors(&server);
        assert_eq!(editors.len(), 6);
        let parent_id = choice(&journal, "general").unwrap();
        let parent = f.store.read_proposal(&parent_id)?.unwrap();
        let parent_bundle = f.store.read_candidate_bundle(&parent.proposal_id)?;
        let baseline = f.store.read_revision(&f.pin.skill_id, &f.pin.revision_id, None)?.bundle;
        let expected = ordered_parent_patch();
        let intermediate = expected["edits"][1]["old"].as_str().unwrap();
        assert!(
            !std::str::from_utf8(baseline.file("SKILL.md").unwrap().bytes())?.contains(intermediate),
            "later old text deliberately does not exist in untouched baseline"
        );
        assert_eq!(expected["edits"][0]["new"], expected["edits"][1]["old"]);
        assert!(
            std::str::from_utf8(parent_bundle.file("SKILL.md").unwrap().bytes())?
                .contains("description: Helps with migration-aware deploys")
        );
        assert_eq!(parent.diff.as_ref().unwrap().edits.len(), 2);
        assert_eq!(
            parent.diff.as_ref().unwrap().description_change.as_ref().unwrap().new,
            "Helps with migration-aware deploys"
        );
        for (editor, stage) in editors
            .iter()
            .skip(3)
            .zip(["round2-general", "round2-domain", "round2-compose"])
        {
            let content = editor["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|message| message["content"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let context: serde_json::Value =
                serde_json::from_str(content.split_once("H2 observed development data:\n").unwrap().1)?;
            assert_eq!(
                context["selected_parent"]["baseline_patch"], expected,
                "the editor receives the actual complete first-round program"
            );
            assert_eq!(
                context["selected_parent"]["revision"],
                serde_json::to_value(&parent.candidate_revision)?
            );
            assert_eq!(context["selected_parent"]["diff"], serde_json::to_value(&parent.diff)?);
            assert_eq!(
                context["selected_parent"]["development_evidence"]["probe_stage"],
                "general-probe"
            );
            assert!(!content.contains("validation-0") && !content.contains("holdout-0"));
            assert!(content.contains("a later old string may match content produced by an earlier edit"));
            let replay_id = choice(&journal, stage).unwrap();
            let replay = f.store.read_proposal(&replay_id)?.unwrap();
            assert_eq!(
                replay.candidate_revision, parent.candidate_revision,
                "provided patch recreates exact parent identity"
            );
            let replay_bundle = f.store.read_candidate_bundle(&replay.proposal_id)?;
            assert_eq!(replay_bundle.files().len(), parent_bundle.files().len());
            for file in parent_bundle.files() {
                assert_eq!(
                    replay_bundle.file(file.path()).unwrap().bytes(),
                    file.bytes(),
                    "exact bytes for {}",
                    file.path()
                );
            }
        }
        assert_eq!(
            f.store.active_binding(&f.pin.skill_id)?.unwrap().generation,
            f.pin.generation,
            "private search does not publish"
        );
        let sends = server.count.load(Ordering::SeqCst);
        let repeated = CleanupScope::new(None, cancel.clone())?
            .run(service.run_instruction_plan(&config, &plan, &cancel))
            .await?;
        assert_eq!(repeated.head_sha256, journal.head_sha256);
        assert_eq!(
            server.count.load(Ordering::SeqCst),
            sends,
            "whole-plan replay sends nothing"
        );
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn refinement_parent_context_omits_assets_while_replay_preserves_the_complete_bundle() -> Result<()> {
    const SCRIPT: &[u8] = b"#!/bin/sh\n# C04-SCRIPT-CONTENT-MARKER\nexit 0\n";
    const REFERENCE: &[u8] = b"# C04-REFERENCE-CONTENT-MARKER\n";
    const BINARY: &[u8] = b"\xff\x00C04-BINARY-CONTENT-MARKER\xfe";
    let f = fixture_with_entries(vec![
        entry("SKILL.md", skill_md_with(20).as_bytes()),
        exec_entry("scripts/run.sh", SCRIPT),
        entry("references/guide.md", REFERENCE),
        entry("assets/opaque.bin", BINARY),
    ]);
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    server.h2_mode.store(6, Ordering::SeqCst);
    let result = async {
        let config = config(&f, &server);
        let suite = suite(&f);
        let service = service(&f, &suite);
        let plan = cycle_with_schedule(
            &f,
            &service,
            &config,
            pattern.pattern_id,
            InstructionSchedule::Sequential2,
        )?;
        service.admit_experiment(plan.spec.clone(), &config)?;
        let cancel = CancellationToken::new();
        let journal = CleanupScope::new(None, cancel.clone())?
            .run(service.run_instruction_plan(&config, &plan, &cancel))
            .await?;
        assert!(service.experiment_qualified(&journal)?);
        assert_eq!(choice(&journal, "select"), choice(&journal, "select-round1"));
        let parent = f.store.read_proposal(&choice(&journal, "general").unwrap())?.unwrap();
        let parent_bundle = f.store.read_candidate_bundle(&parent.proposal_id)?;
        assert_eq!(parent_bundle.files().len(), 4);
        for (path, bytes) in [
            ("scripts/run.sh", SCRIPT),
            ("references/guide.md", REFERENCE),
            ("assets/opaque.bin", BINARY),
        ] {
            assert_eq!(parent_bundle.file(path).unwrap().bytes(), bytes);
        }
        assert!(parent_bundle.file("scripts/run.sh").unwrap().is_executable());
        let requests = editors(&server);
        assert_eq!(requests.len(), 6);
        for editor in &requests {
            let wire = serde_json::to_string(editor)?;
            for marker in [
                "C04-SCRIPT-CONTENT-MARKER",
                "C04-REFERENCE-CONTENT-MARKER",
                "C04-BINARY-CONTENT-MARKER",
            ] {
                assert!(
                    !wire.contains(marker),
                    "non-instruction asset must not enter editor wire: {marker}"
                );
            }
        }
        for (editor, stage) in requests
            .iter()
            .skip(3)
            .zip(["round2-general", "round2-domain", "round2-compose"])
        {
            let content = editor["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|message| message["content"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let context: serde_json::Value =
                serde_json::from_str(content.split_once("H2 observed development data:\n").unwrap().1)?;
            assert_eq!(
                context["selected_parent"]["files"],
                json!([{
                    "path":"SKILL.md", "content":std::str::from_utf8(parent_bundle.file("SKILL.md").unwrap().bytes())?
                }])
            );
            assert_eq!(context["selected_parent"]["baseline_patch"], ordered_parent_patch());
            assert_eq!(
                context["selected_parent"]["revision"],
                serde_json::to_value(&parent.candidate_revision)?
            );
            assert_eq!(context["selected_parent"]["diff"], serde_json::to_value(&parent.diff)?);
            assert_eq!(
                context["selected_parent"]["development_evidence"]["probe_stage"],
                "general-probe"
            );
            assert!(!content.contains("validation-0") && !content.contains("holdout-0"));
            let replay = f.store.read_proposal(&choice(&journal, stage).unwrap())?.unwrap();
            assert_eq!(replay.candidate_revision, parent.candidate_revision);
            let bundle = f.store.read_candidate_bundle(&replay.proposal_id)?;
            assert_eq!(bundle.files().len(), parent_bundle.files().len());
            for file in parent_bundle.files() {
                let replay_file = bundle.file(file.path()).unwrap();
                assert_eq!(
                    replay_file.bytes(),
                    file.bytes(),
                    "complete asset bytes: {}",
                    file.path()
                );
                assert_eq!(replay_file.is_executable(), file.is_executable());
            }
        }
        assert_eq!(f.store.list_evaluation_ids()?.len(), 1);
        assert_eq!(
            f.store.active_binding(&f.pin.skill_id)?.unwrap().generation,
            f.pin.generation
        );
        let sends = server.count.load(Ordering::SeqCst);
        let repeated = CleanupScope::new(None, cancel.clone())?
            .run(service.run_instruction_plan(&config, &plan, &cancel))
            .await?;
        assert_eq!(repeated.head_sha256, journal.head_sha256);
        assert_eq!(server.count.load(Ordering::SeqCst), sends);
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn refinement_parent_relative_and_aggregate_over_budget_programs_remain_refused() -> Result<()> {
    for (mode, failure) in [
        (7, ProposalFailureKind::PatchNotApplicable),
        (8, ProposalFailureKind::EditBudgetExceeded),
    ] {
        let f = fixture();
        let pattern = usable_pattern(&f);
        let server = Server::start(false).await?;
        server.h2_mode.store(mode, Ordering::SeqCst);
        let result: Result<()> = async {
            let config = config(&f, &server);
            let suite = suite(&f);
            let service = service(&f, &suite);
            let plan = cycle_with_schedule(
                &f,
                &service,
                &config,
                pattern.pattern_id,
                InstructionSchedule::Sequential2,
            )?;
            service.admit_experiment(plan.spec.clone(), &config)?;
            let cancel = CancellationToken::new();
            let journal = CleanupScope::new(None, cancel.clone())?
                .run(service.run_instruction_plan(&config, &plan, &cancel))
                .await?;
            assert_eq!(choice(&journal, "select-round1").as_deref(), Some("selected:general"));
            assert!(!journal.state.stages["round2-general"].successful());
            let failed = f
                .store
                .read_proposal(&choice(&journal, "round2-general").unwrap())?
                .unwrap();
            assert_eq!(failed.failure.as_ref().unwrap().kind, failure);
            assert!(
                failed.candidate_revision.is_none(),
                "invalid program cannot create a candidate"
            );
            assert!(!service.experiment_qualified(&journal)?);
            assert!(
                f.store.list_evaluation_ids()?.is_empty(),
                "failed development cannot reach independent cases"
            );
            assert_eq!(
                service
                    .experiment_development_probes(&journal)?
                    .as_object()
                    .unwrap()
                    .len(),
                4
            );
            assert_eq!(editors(&server).len(), 4);
            assert!(matches!(journal.state.stages["round2-domain"], StageState::Planned));
            assert_eq!(
                f.store.active_binding(&f.pin.skill_id)?.unwrap().generation,
                f.pin.generation
            );
            let sends = server.count.load(Ordering::SeqCst);
            let repeated = CleanupScope::new(None, cancel.clone())?
                .run(service.run_instruction_plan(&config, &plan, &cancel))
                .await?;
            assert_eq!(repeated.head_sha256, journal.head_sha256);
            assert_eq!(
                server.count.load(Ordering::SeqCst),
                sends,
                "failed-program replay sends nothing"
            );
            Ok(())
        }
        .await;
        server.finish().await?;
        result?;
    }
    Ok(())
}

#[tokio::test]
async fn refinement_neutral_first_round_stops_without_round_two_or_validation() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    server.h2_mode.store(1, Ordering::SeqCst);
    let result = async {
        let config = config(&f, &server);
        let suite = suite(&f);
        let service = service(&f, &suite);
        let plan = cycle_with_schedule(
            &f,
            &service,
            &config,
            pattern.pattern_id,
            InstructionSchedule::Sequential2,
        )?;
        service.admit_experiment(plan.spec.clone(), &config)?;
        let cancel = CancellationToken::new();
        let journal = CleanupScope::new(None, cancel.clone())?
            .run(service.run_instruction_plan(&config, &plan, &cancel))
            .await?;
        assert_eq!(choice(&journal, "select-round1").as_deref(), Some("baseline"));
        assert_eq!(editors(&server).len(), 3);
        assert_eq!(
            service
                .experiment_development_probes(&journal)?
                .as_object()
                .unwrap()
                .len(),
            4
        );
        assert_eq!(f.store.list_evaluation_ids()?.len(), 0);
        assert!(!service.experiment_qualified(&journal)?);
        assert!(matches!(journal.state.stages["round2-general"], StageState::Planned));
        let sends = server.count.load(Ordering::SeqCst);
        let repeated = CleanupScope::new(None, cancel.clone())?
            .run(service.run_instruction_plan(&config, &plan, &cancel))
            .await?;
        assert_eq!(repeated.head_sha256, journal.head_sha256);
        assert_eq!(server.count.load(Ordering::SeqCst), sends);
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn refinement_parallel_runs_two_owned_branches_with_shared_baseline_and_one_final_validation() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    server.h2_mode.store(3, Ordering::SeqCst);
    let result = async {
        let config = config(&f, &server);
        let suite = suite(&f);
        let service = service(&f, &suite);
        let plan = cycle_with_schedule(
            &f,
            &service,
            &config,
            pattern.pattern_id,
            InstructionSchedule::Parallel2,
        )?;
        assert_eq!(plan.spec.stages.len(), 18);
        assert_eq!(plan.spec.max_concurrent, 2);
        service.admit_experiment(plan.spec.clone(), &config)?;
        let cancel = CancellationToken::new();
        let journal = CleanupScope::new(None, cancel.clone())?
            .run(service.run_instruction_plan(&config, &plan, &cancel))
            .await?;
        assert!(service.experiment_qualified(&journal)?);
        assert_eq!(editors(&server).len(), 6);
        assert_eq!(choice(&journal, "select").as_deref(), Some("selected:branch-a-general"));
        let probes = service.experiment_development_probes(&journal)?;
        assert_eq!(probes.as_object().unwrap().len(), 7);
        assert_eq!(
            probes
                .as_object()
                .unwrap()
                .values()
                .filter(|probe| probe["proposal_stage"].is_null())
                .count(),
            1
        );
        assert_eq!(f.store.list_evaluation_ids()?.len(), 1);
        for editor in editors(&server) {
            let text = editor["messages"].to_string();
            assert!(!text.contains("selected_parent") && !text.contains("validation-0") && !text.contains("holdout-0"));
            assert!(
                !(text.contains("branch-a-general-probe") && text.contains("branch-b-general-probe")),
                "branches cannot see sibling context"
            );
        }
        let mut active = std::collections::BTreeSet::new();
        let mut peak = 0;
        for entry in &journal.entries {
            match &entry.event {
                ExperimentEvent::Reserved { stage } => {
                    assert!(active.insert(stage));
                    peak = peak.max(active.len());
                    assert!(active.len() <= 2);
                }
                ExperimentEvent::CleanupConfirmed { stage } => {
                    assert!(active.remove(stage));
                }
                _ => {}
            }
        }
        assert_eq!(peak, 2);
        assert!(active.is_empty());
        let frozen = plan.spec.clone().freeze()?;
        assert!(journal.state.committed(&frozen)?.within(plan.spec.resources));
        let sends = server.count.load(Ordering::SeqCst);
        let recovered = service.recover_experiment(&plan.spec.id, frozen.digest())?;
        assert_eq!(recovered.head_sha256, journal.head_sha256);
        let repeated = CleanupScope::new(None, cancel.clone())?
            .run(service.run_instruction_plan(&config, &plan, &cancel))
            .await?;
        assert_eq!(repeated.head_sha256, journal.head_sha256);
        assert_eq!(server.count.load(Ordering::SeqCst), sends);
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn refinement_parallel_cancel_drains_both_owners_and_recovery_sends_nothing() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    server.h2_mode.store(4, Ordering::SeqCst);
    let result = async {
        let config = config(&f, &server);
        let suite = suite(&f);
        let service = service(&f, &suite);
        let plan = cycle_with_schedule(
            &f,
            &service,
            &config,
            pattern.pattern_id,
            InstructionSchedule::Parallel2,
        )?;
        service.admit_experiment(plan.spec.clone(), &config)?;
        let cancel = CancellationToken::new();
        let mut running = Box::pin(
            CleanupScope::new(None, cancel.clone())?.run(service.run_instruction_plan(&config, &plan, &cancel)),
        );
        tokio::time::timeout(Duration::from_secs(30),async {
            loop { tokio::select! {
                result=&mut running=>anyhow::bail!("parallel cycle ended before both editors dispatched: {result:?}"),
                ()=tokio::time::sleep(Duration::from_millis(10))=>if editors(&server).len()==2 {break;},
            }} Ok::<(),anyhow::Error>(())
        }).await??;
        service.cancel_experiment(&plan.spec.id, plan.spec.clone().freeze()?.digest())?;
        assert!(
            tokio::time::timeout(Duration::from_secs(30), &mut running)
                .await?
                .is_err()
        );
        let sends = server.count.load(Ordering::SeqCst);
        let recovered = service.recover_experiment(&plan.spec.id, plan.spec.clone().freeze()?.digest())?;
        assert!(recovered.state.cancelled);
        assert!(!service.experiment_qualified(&recovered)?);
        for name in ["branch-a-general", "branch-b-general"] {
            assert!(matches!(recovered.state.stages[name], StageState::DispatchUnknown));
        }
        assert!(
            CleanupScope::new(None, cancel.clone())?
                .run(service.run_instruction_plan(&config, &plan, &cancel))
                .await
                .is_err()
        );
        assert_eq!(server.count.load(Ordering::SeqCst), sends);
        assert_eq!(editors(&server).len(), 2);
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn refinement_parallel_branch_failure_cancels_and_joins_sibling_without_retry() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    server.h2_mode.store(5, Ordering::SeqCst);
    let result = async {
        let config = config(&f, &server);
        let suite = suite(&f);
        let service = service(&f, &suite);
        let plan = cycle_with_schedule(
            &f,
            &service,
            &config,
            pattern.pattern_id,
            InstructionSchedule::Parallel2,
        )?;
        service.admit_experiment(plan.spec.clone(), &config)?;
        let cancel = CancellationToken::new();
        assert!(
            tokio::time::timeout(
                Duration::from_secs(40),
                CleanupScope::new(None, cancel.clone())?.run(service.run_instruction_plan(&config, &plan, &cancel))
            )
            .await?
            .is_err()
        );
        assert_eq!(editors(&server).len(), 2);
        let frozen = plan.spec.clone().freeze()?;
        let journal = service.inspect_experiment(&plan.spec.id, frozen.digest())?;
        for name in ["branch-a-general", "branch-b-general"] {
            assert!(matches!(journal.state.stages[name], StageState::DispatchUnknown));
        }
        let sends = server.count.load(Ordering::SeqCst);
        service.recover_experiment(&plan.spec.id, frozen.digest())?;
        assert!(
            CleanupScope::new(None, cancel.clone())?
                .run(service.run_instruction_plan(&config, &plan, &cancel))
                .await
                .is_err()
        );
        assert_eq!(server.count.load(Ordering::SeqCst), sends);
        assert_eq!(f.store.list_evaluation_ids()?.len(), 0);
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn refinement_dropped_plan_waiter_preserves_both_owned_unknown_receipts() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    server.h2_mode.store(4, Ordering::SeqCst);
    let result = async {
        let config = config(&f, &server);
        let suite = suite(&f);
        let service = service(&f, &suite);
        let plan = cycle_with_schedule(
            &f,
            &service,
            &config,
            pattern.pattern_id,
            InstructionSchedule::Parallel2,
        )?;
        service.admit_experiment(plan.spec.clone(), &config)?;
        let cancel = CancellationToken::new();
        let scope = CleanupScope::new(None, cancel.clone())?;
        scope
            .run(async {
                let mut running = Box::pin(service.run_instruction_plan(&config, &plan, &cancel));
                tokio::time::timeout(Duration::from_secs(30), async {
                    loop {
                        tokio::select! {
                            result=&mut running=>anyhow::bail!("cycle ended before drop: {result:?}"),
                            ()=tokio::time::sleep(Duration::from_millis(10))=>if editors(&server).len()==2 {break;},
                        }
                    }
                    Ok::<(), anyhow::Error>(())
                })
                .await??;
                drop(running);
                Ok::<(), anyhow::Error>(())
            })
            .await?;
        let frozen = plan.spec.clone().freeze()?;
        let journal = service.inspect_experiment(&plan.spec.id, frozen.digest())?;
        for name in ["branch-a-general", "branch-b-general"] {
            assert!(matches!(journal.state.stages[name], StageState::DispatchUnknown));
        }
        let sends = server.count.load(Ordering::SeqCst);
        service.recover_experiment(&plan.spec.id, frozen.digest())?;
        assert!(
            CleanupScope::new(None, cancel.clone())?
                .run(service.run_instruction_plan(&config, &plan, &cancel))
                .await
                .is_err()
        );
        assert_eq!(server.count.load(Ordering::SeqCst), sends);
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn refinement_planning_checks_each_aggregate_resource_and_legacy_operation_identity() -> Result<()> {
    let server = Server::start(false).await?;
    let result=async {
        for schedule in [InstructionSchedule::Sequential2,InstructionSchedule::Parallel2] {
            let f=fixture(); let pattern=usable_pattern(&f);
            let config=config(&f,&server); let suite=suite(&f); let service=service(&f,&suite);
            let plan=cycle_with_schedule(&f,&service,&config,pattern.pattern_id.clone(),schedule)?;
            let mut resources=ExperimentResources::default();
            for stage in &plan.spec.stages {resources=resources.checked_add(stage.allocation)?;}
            for component in 0..6 {
                let mut reduced=resources;
                match component {0=>reduced.calls-=1,1=>reduced.tokens-=1,2=>reduced.micro_usd-=1,3=>reduced.tool_calls-=1,4=>reduced.output_bytes-=1,_=>reduced.scratch_bytes-=1}
                let draft=InstructionCycleDraft {id:plan.spec.id.clone(),skill_id:f.pin.skill_id.clone(),patterns:vec![pattern.pattern_id.clone()],schedule,
                    verification_split:EvalSplit::Validation, holdout_allocation:None, resources:reduced, proposal_allocation:plan.spec.stages[1].allocation,
                    probe_allocation:plan.spec.stages[0].allocation,evaluation_allocation:plan.spec.stages.iter().find(|stage|stage.id=="final-evaluation").unwrap().allocation,
                    elapsed_ms:300_000,max_input_bytes:65536,max_output_bytes:16384,proposal_timeout_ms:120_000};
                assert!(matches!(draft.compile(),Err(ExperimentError::BudgetExceeded)),"component {component} must fail before sending");
            }
        }
        let legacy=r#"{"operation":"development_selection","probe_stages":["baseline","general-probe"]}"#;
        let operation:ExperimentOperation=serde_json::from_str(legacy)?;
        assert_eq!(serde_json::to_string(&operation)?,legacy,"empty new ancestry must preserve old serialized operation identity");
        let legacy_proposal=r#"{"operation":"development_proposal","role":"general","patterns":[],"probe_stages":["baseline"],"max_input_bytes":1024,"max_output_bytes":512,"timeout_ms":1000}"#;
        let operation:ExperimentOperation=serde_json::from_str(legacy_proposal)?;
        assert_eq!(serde_json::to_string(&operation)?,legacy_proposal);
        assert_eq!(server.count.load(Ordering::SeqCst),0,"planning and compatibility checks must be read-only");
        Ok(())
    }.await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn refinement_rejects_changed_selected_parent_before_round_two_dispatch() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    let result = async {
        let config = config(&f, &server);
        let suite = suite(&f);
        let service = service(&f, &suite);
        let plan = cycle_with_schedule(
            &f,
            &service,
            &config,
            pattern.pattern_id,
            InstructionSchedule::Sequential2,
        )?;
        let admitted = service.admit_experiment(plan.spec.clone(), &config)?;
        for stage in plan.spec.stages.iter().take_while(|stage| stage.id != "round2-general") {
            let journal = run_stage(
                &service,
                &config,
                &admitted.plan,
                &stage.id,
                &plan.operations[&stage.id],
            )
            .await?;
            assert!(journal.state.stages[&stage.id].successful());
        }
        let path = f
            .store
            .root()
            .join("experiment-evidence")
            .join(plan.spec.id.as_str())
            .join("general-probe.json");
        let mut altered: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        altered["executor"] = json!("forged-parent-executor");
        std::fs::write(path, serde_json::to_vec(&altered)?)?;
        let sends = server.count.load(Ordering::SeqCst);
        let cancel = CancellationToken::new();
        assert!(
            CleanupScope::new(None, cancel.clone())?
                .run(service.run_instruction_plan(&config, &plan, &cancel))
                .await
                .is_err()
        );
        assert_eq!(
            server.count.load(Ordering::SeqCst),
            sends,
            "changed parent evidence must fail before a second-round send"
        );
        assert_eq!(f.store.list_evaluation_ids()?.len(), 0);
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}
