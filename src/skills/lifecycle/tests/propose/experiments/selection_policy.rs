//! Selector boundaries over the actual service and pinned development evidence.
//! Proposals use loopback HTTP. The existing probe adapter judges deterministic
//! case observations; these policy fixtures are not native/live-model evidence.
use super::*;
use crate::skills::lifecycle::tests::evaluate::{Scripted, good};

type Outcomes = Vec<Vec<bool>>;
struct PolicyVector {
    name: &'static str,
    arms: [Outcomes; 3],
    min_gain: u32,
    allowed_regressions: u32,
    critical_roots: Vec<usize>,
    expected: &'static str,
}
impl PolicyVector {
    fn new(name: &'static str, arms: [Outcomes; 3], expected: &'static str) -> Self {
        Self {
            name,
            arms,
            min_gain: 1,
            allowed_regressions: 0,
            critical_roots: vec![],
            expected,
        }
    }
}
fn once(values: [bool; 3]) -> Outcomes {
    values.into_iter().map(|value| vec![value]).collect()
}
fn twice(values: [[bool; 2]; 3]) -> Outcomes {
    values.into_iter().map(Vec::from).collect()
}

fn append(f: &Fixture, old: &ExperimentJournal, id: &str, event: ExperimentEvent) -> Result<ExperimentJournal> {
    Ok(f.store.append_experiment(
        &old.plan.spec().id,
        old.plan.digest(),
        old.next_sequence(),
        &old.head_sha256,
        id,
        event,
        Utc::now(),
    )?)
}

/// Admit a scripted probe through the same immutable stage/state contracts,
/// generate its case receipts with run_development_probe, then pin its digest.
/// Zero tokens/money reflect that this executor invokes no model or transport.
async fn record_probe(
    f: &Fixture,
    config: &Config,
    plan: &FrozenExperiment,
    stage: &str,
    bundle: &Bundle,
    outcomes: &Outcomes,
) -> Result<()> {
    let proposal_stage = stage.strip_suffix("-probe").map(str::to_owned);
    let mut journal = f.store.read_experiment(&plan.spec().id, plan.digest())?;
    journal = append(
        f,
        &journal,
        &format!("{stage}-reserve"),
        ExperimentEvent::Reserved { stage: stage.into() },
    )?;
    journal = append(
        f,
        &journal,
        &format!("{stage}-dispatch"),
        ExperimentEvent::Dispatched { stage: stage.into() },
    )?;
    let expected: Vec<_> = outcomes.iter().flatten().copied().collect();
    let scripted = expected.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let executor = Scripted::new(move |request| {
        let index = observed.fetch_add(1, Ordering::SeqCst);
        let mut run = good(request);
        run.response = if scripted[index] {
            "MIGRATION-PASS"
        } else {
            "BASELINE-FAIL"
        }
        .into();
        run.tokens = Some(0);
        run.cost_micros = Some(0);
        Ok(run)
    });
    let cancel = CancellationToken::new();
    let receipt = CleanupScope::new(None, cancel.clone())?
        .run(async {
            let runtime = crate::runtime::experiment_stage::ExperimentStageRuntime::new(
                config,
                plan,
                stage,
                cancel.clone(),
                f.store.clone(),
            )?;
            let receipt = runtime
                .scope(
                    f.store
                        .run_development_probe(&journal, stage, proposal_stage, bundle, &executor, &cancel),
                )
                .await?;
            runtime.flush().await?;
            Ok::<_, anyhow::Error>(receipt)
        })
        .await?;
    assert_eq!(calls.load(Ordering::SeqCst), expected.len());
    assert_eq!(receipt.cases.len(), expected.len());
    assert!(
        receipt.complete(),
        "fixture probe must contain valid semantic pass/fail evidence"
    );
    for (case, pass) in receipt.cases.iter().zip(expected) {
        assert_eq!(case.verdict == crate::skills::lifecycle::CaseVerdict::Pass, pass);
    }
    let digest = f.store.write_development_probe(&journal, &receipt)?;
    journal = append(
        f,
        &journal,
        &format!("{stage}-settle"),
        ExperimentEvent::Settled {
            stage: stage.into(),
            usage: ExperimentResources::default(),
            evidence_sha256: digest.clone(),
            reference: Some(format!("probe:{stage}")),
            successful: true,
        },
    )?;
    journal = append(
        f,
        &journal,
        &format!("{stage}-cleanup"),
        ExperimentEvent::CleanupConfirmed { stage: stage.into() },
    )?;
    assert!(journal.state.stages[stage].successful());
    assert_eq!(
        f.store
            .read_development_probe(&plan.spec().id, stage, &digest)?
            .digest()?,
        digest
    );
    Ok(())
}

async fn check_selection(vector: PolicyVector) -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    let result = async {
        let config = config(&f, &server);
        let mut raw_suite = suite(&f).suite().clone();
        raw_suite.trials = u32::try_from(vector.arms[0][0].len())?;
        raw_suite.min_gain_cases = vector.min_gain;
        raw_suite.allowed_regressions = vector.allowed_regressions;
        for (index, case) in raw_suite
            .cases
            .iter_mut()
            .filter(|case| case.split == EvalSplit::Development)
            .enumerate()
        {
            case.critical = vector.critical_roots.contains(&index);
        }
        let suite = raw_suite.freeze()?;
        let service = service(&f, &suite);
        let mut plan = super::instruction_cycle::cycle(&f, &service, &config, pattern.pattern_id)?;
        plan.spec.id = ExperimentId::parse("selector-policy-matrix")?;
        plan.spec
            .stages
            .retain(|stage| !matches!(stage.id.as_str(), "compose" | "compose-probe"));
        plan.operations.remove("compose");
        plan.operations.remove("compose-probe");
        let probes = vec!["baseline".into(), "general-probe".into(), "domain-probe".into()];
        let selection = ExperimentOperation::DevelopmentSelection {
            probe_stages: probes.clone(),
        };
        let stage = plan.spec.stages.iter_mut().find(|stage| stage.id == "select").unwrap();
        stage.dependencies = probes;
        stage.input_sha256 = selection.digest()?;
        plan.operations.insert("select".into(), selection);
        let frozen = service.plan_experiment(plan.spec.clone(), &config)?;
        plan.spec = frozen.spec().clone();
        plan.validate()?;
        service.admit_experiment(plan.spec.clone(), &config)?;
        let baseline = f.store.read_revision(&f.pin.skill_id, &f.pin.revision_id, None)?.bundle;
        record_probe(&f, &config, &frozen, "baseline", &baseline, &vector.arms[0]).await?;
        for (index, proposal_stage) in ["general", "domain"].into_iter().enumerate() {
            let journal = run_stage(
                &service,
                &config,
                &frozen,
                proposal_stage,
                &plan.operations[proposal_stage],
            )
            .await?;
            assert!(
                journal.state.stages[proposal_stage].successful(),
                "{} proposal {proposal_stage}: {journal:?}",
                vector.name
            );
            let proposal_id = journal
                .entries
                .iter()
                .find_map(|entry| match &entry.event {
                    ExperimentEvent::Settled { stage, reference, .. } if stage == proposal_stage => reference.as_ref(),
                    _ => None,
                })
                .unwrap();
            let bundle = f.store.read_candidate_bundle(proposal_id)?;
            record_probe(
                &f,
                &config,
                &frozen,
                &format!("{proposal_stage}-probe"),
                &bundle,
                &vector.arms[index + 1],
            )
            .await?;
        }
        let journal = run_stage(&service, &config, &frozen, "select", &plan.operations["select"]).await?;
        assert!(journal.state.stages["select"].successful());
        let choice = journal
            .entries
            .iter()
            .find_map(|entry| match &entry.event {
                ExperimentEvent::Settled { stage, reference, .. } if stage == "select" => reference.as_deref(),
                _ => None,
            })
            .unwrap();
        assert_eq!(choice, vector.expected, "selector policy vector {}", vector.name);
        assert_eq!(
            server.count.load(Ordering::SeqCst),
            2,
            "only the two editing stages invoke loopback HTTP"
        );
        assert!(
            f.store.list_evaluation_ids()?.is_empty(),
            "development comparison cannot spend independent attempts"
        );
        let sends = server.count.load(Ordering::SeqCst);
        assert!(
            run_stage(&service, &config, &frozen, "select", &plan.operations["select"])
                .await
                .is_err()
        );
        assert_eq!(
            server.count.load(Ordering::SeqCst),
            sends,
            "selection replay sends nothing"
        );
        assert_eq!(
            f.store.active_binding(&f.pin.skill_id)?.unwrap().generation,
            f.pin.generation
        );
        assert!(
            !service.experiment_qualified(&journal)?,
            "selection alone is not independent qualification"
        );
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn selection_policy_chooses_the_good_role_in_both_directions_and_preserves_ties() -> Result<()> {
    let mut baseline_tie = PolicyVector::new(
        "baseline-tie-with-offsetting-gain-and-regression",
        [
            once([true, false, false]),
            once([false, true, false]),
            once([true, false, false]),
        ],
        "baseline",
    );
    baseline_tie.allowed_regressions = 1;
    for vector in [
        PolicyVector::new(
            "general-good-domain-bad",
            [
                once([false, false, false]),
                once([true, true, true]),
                once([false, false, false]),
            ],
            "selected:general",
        ),
        PolicyVector::new(
            "general-bad-domain-good",
            [
                once([false, false, false]),
                once([false, false, false]),
                once([true, true, true]),
            ],
            "selected:domain",
        ),
        baseline_tie,
        PolicyVector::new(
            "improving-candidate-tie",
            [
                once([false, false, false]),
                once([true, true, true]),
                once([true, true, true]),
            ],
            "selected:general",
        ),
    ] {
        check_selection(vector).await?;
    }
    Ok(())
}

#[tokio::test]
async fn selection_policy_critical_failure_vetoes_an_otherwise_eligible_higher_score() -> Result<()> {
    let mut vector = PolicyVector::new(
        "critical-veto",
        [
            once([true, false, false]),
            once([false, true, true]),
            once([true, false, false]),
        ],
        "baseline",
    );
    vector.allowed_regressions = 1;
    vector.critical_roots = vec![0];
    check_selection(vector).await
}

#[tokio::test]
async fn selection_policy_accepts_the_exact_regression_allowance_and_rejects_excess() -> Result<()> {
    for (allowed, expected) in [(0, "baseline"), (1, "selected:general")] {
        let mut vector = PolicyVector::new(
            "regression-boundary",
            [
                once([true, false, false]),
                once([false, true, true]),
                once([true, false, false]),
            ],
            expected,
        );
        vector.allowed_regressions = allowed;
        check_selection(vector).await?;
    }
    Ok(())
}

#[tokio::test]
async fn selection_policy_counts_root_gains_and_regressions_once_across_repeated_trials() -> Result<()> {
    let mut one_root = PolicyVector::new(
        "one-root-is-not-two-gains",
        [
            twice([[false, false]; 3]),
            twice([[true, true], [false, false], [false, false]]),
            twice([[false, false]; 3]),
        ],
        "baseline",
    );
    one_root.min_gain = 2;
    check_selection(one_root).await?;
    let mut partial = PolicyVector::new(
        "partial-repeat-is-not-a-pass",
        [
            twice([[false, false]; 3]),
            twice([[true, false], [true, true], [false, false]]),
            twice([[false, false]; 3]),
        ],
        "baseline",
    );
    partial.min_gain = 2;
    check_selection(partial).await?;
    let mut regression = PolicyVector::new(
        "one-regressed-root-is-not-two-regressions",
        [
            twice([[true, true], [false, false], [false, false]]),
            twice([[false, false], [true, true], [true, true]]),
            twice([[true, true], [false, false], [false, false]]),
        ],
        "selected:general",
    );
    regression.min_gain = 2;
    regression.allowed_regressions = 1;
    check_selection(regression).await
}
