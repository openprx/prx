//! Actual runtime-builder -> HTTP -> proposal -> isolated agent -> verifier.
//! The model is a deterministic loopback fixture, never a live-provider claim.
use super::*;
use crate::config::Config;
use crate::runtime::turn_cleanup::CleanupScope;
use crate::skills::lifecycle::experiment::*;
use crate::skills::lifecycle::{
    EvalCase, EvalSplit, EvalSuite, LifecycleMode, SideEffects, SkillsLifecycleService, SkillsLifecycleSettings,
    SuiteBudget, SuiteRegistry, Validator,
};
use anyhow::{Context, Result};
use chrono::Utc;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
mod admission;
mod adoption;
mod adoption_contract;
mod development_probes;
mod gate_review;
mod holdout;
mod instruction_cycle;
mod live_fixture;
mod refinement;
mod selection_policy;
mod v01_study;

struct Server {
    url: String,
    count: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<serde_json::Value>>>,
    oversized: Arc<AtomicBool>,
    h2_mode: Arc<AtomicUsize>,
    judge_mode: Arc<AtomicUsize>,
    judge_count: Arc<AtomicUsize>,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<Result<()>>,
}
impl Server {
    async fn start(hang: bool) -> Result<Self> {
        Self::start_mode(hang, None).await
    }
    async fn start_mode(hang: bool, artifact_wrong: Option<bool>) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/v1", listener.local_addr()?);
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        let count = Arc::new(AtomicUsize::new(0));
        let counter = count.clone();
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let captured = bodies.clone();
        let oversized = Arc::new(AtomicBool::new(false));
        let large = oversized.clone();
        let h2_mode = Arc::new(AtomicUsize::new(0));
        let h2_behavior = h2_mode.clone();
        let judge_mode = Arc::new(AtomicUsize::new(0));
        let judge_behavior = judge_mode.clone();
        let judge_count = Arc::new(AtomicUsize::new(0));
        let judges = judge_count.clone();
        let editor_barrier = Arc::new(tokio::sync::Barrier::new(2));
        let editor_count = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    biased;
                    () = stop.cancelled() => { connections.abort_all(); while connections.join_next().await.is_some() {} return Ok(()); },
                    accepted = listener.accept() => {
                        let (mut socket, _) = accepted?; let count = counter.clone(); let captured = captured.clone(); let large = large.clone(); let h2_behavior = h2_behavior.clone(); let editor_barrier = editor_barrier.clone(); let editor_count = editor_count.clone(); let judge_behavior = judge_behavior.clone(); let judges = judges.clone();
                        connections.spawn(async move {
                            let mut bytes = Vec::new(); let mut chunk = [0_u8; 4096];
                            let body = loop {
                                let size = socket.read(&mut chunk).await?;
                                anyhow::ensure!(size > 0, "incomplete HTTP request"); bytes.extend_from_slice(&chunk[..size]);
                                anyhow::ensure!(bytes.len() <= 2 * 1024 * 1024, "HTTP fixture byte cap");
                                if let Some(end) = bytes.windows(4).position(|value| value == b"\r\n\r\n") {
                                    let headers = std::str::from_utf8(&bytes[..end])?;
                                    let length: usize = headers.lines().find_map(|line| line.split_once(':').filter(|(key, _)| key.eq_ignore_ascii_case("content-length")).map(|(_, value)| value.trim())).context("missing content length")?.parse()?;
                                    anyhow::ensure!(length <= 2 * 1024 * 1024, "HTTP body byte cap");
                                    if bytes.len() >= end + 4 + length { break serde_json::from_slice::<serde_json::Value>(&bytes[end + 4..end + 4 + length])?; }
                                }
                            };
                            count.fetch_add(1, Ordering::SeqCst);
                            { let mut retained = captured.lock(); if retained.len() < 256 { retained.push(body.clone()); } }
                            if body["tools"].as_array().is_some_and(|tools| tools.iter().any(|tool| {
                                tool["function"]["name"].as_str().is_none_or(|name| name.is_empty() || name.len()>128
                                    || !name.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte,b'_'|b'-')))
                            })) {
                                let error = r#"{"error":{"message":"invalid wire tool name"}}"#;
                                socket.write_all(format!("HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{error}",error.len()).as_bytes()).await?;
                                return Ok::<_, anyhow::Error>(());
                            }
                            if hang { std::future::pending::<()>().await; }
                            let messages = body["messages"].as_array().context("missing messages")?;
                            let is_gate = messages.iter().any(|message| message["role"] == "system"
                                && message["content"].as_str().is_some_and(|text|text.starts_with("You review one proposed change to an instruction skill")));
                            if is_gate {
                                let number = judges.fetch_add(1, Ordering::SeqCst);
                                if judge_behavior.load(Ordering::SeqCst) == 2 { std::future::pending::<()>().await; }
                                if judge_behavior.load(Ordering::SeqCst) == 4 && number == 0 {
                                    let error = r#"{"error":{"message":"fixture judge transient unavailable"}}"#;
                                    socket.write_all(format!("HTTP/1.1 503 Service Unavailable\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{error}",error.len()).as_bytes()).await?;
                                    return Ok::<_, anyhow::Error>(());
                                }
                            }
                            let assessment_response = if is_gate {
                                Some(match judge_behavior.load(Ordering::SeqCst) {
                                    1 => r#"{"verdict":"concern","reason":"fixture procedural concern"}"#,
                                    3 => "malformed fixture judge response",
                                    _ => r#"{"verdict":"approve"}"#,
                                })
                            } else if messages.iter().any(|message| message["role"] == "system"
                                && message["content"].as_str().is_some_and(|text| text.starts_with("Classify the execution requirement of current_request"))) {
                                Some(r#"{"requirement":"execution_required"}"#)
                            } else if messages.iter().any(|message| message["role"] == "system"
                                && message["content"].as_str().is_some_and(|text| text.starts_with("Review the proposed final reply against"))) {
                                Some(r#"{"decision":"accept","reason":"Scripted native results are present","constraints":[]}"#)
                            } else { None };
                            let is_editor = messages.iter().any(|message| message["content"].as_str().is_some_and(|value| value.contains("You revise one instruction skill")));
                            let editor_number = if is_editor { Some(editor_count.fetch_add(1, Ordering::SeqCst)) } else { None };
                            if is_editor {
                                match h2_behavior.load(Ordering::SeqCst) {
                                    3 => { tokio::time::timeout(Duration::from_secs(20), editor_barrier.wait()).await?; },
                                    4 => { std::future::pending::<()>().await; },
                                    5 => {
                                        tokio::time::timeout(Duration::from_secs(20), editor_barrier.wait()).await?;
                                        if editor_number != Some(0) { std::future::pending::<()>().await; }
                                    },
                                    _ => {},
                                }
                            }
                            let text = assessment_response.map_or_else(|| {
                            if messages.iter().any(|message| message["content"].as_str().is_some_and(|value| value.contains("You revise one instruction skill"))) {
                                let system = messages.iter().filter(|message| message["role"] == "system").map(|message| message["content"].to_string()).collect::<Vec<_>>().join(" ");
                                let mode = h2_behavior.load(Ordering::SeqCst);
                                if mode == 9 {
                                    let role = if system.contains("H2 role: general.") { "general" }
                                        else if system.contains("H2 role: domain.") { "domain" } else { "composition" };
                                    return json!({"summary":"Add release preflight guidance", "edits":[{
                                        "file":"SKILL.md", "op":"replace", "old":"the check number 4 before",
                                        "new":format!("the release preflight and {role} check before")
                                    }]}).to_string();
                                }
                                if (6..=8).contains(&mode) {
                                    let development = messages.iter().filter(|message| message["role"] == "user")
                                        .filter_map(|message| message["content"].as_str())
                                        .find_map(|text| text.split_once("H2 observed development data:\n").map(|(_, data)| data))
                                        .expect("ordered-parent fixture needs actual development JSON");
                                    let context: serde_json::Value = serde_json::from_str(development).expect("valid development JSON");
                                    if let Some(parent) = context.get("selected_parent") {
                                        let patch = parent["baseline_patch"].as_object().expect("parent must provide its replayable baseline patch");
                                        if mode == 7 {
                                            let previous = parent["baseline_patch"]["edits"].as_array().unwrap().last().unwrap()["new"].as_str().unwrap();
                                            return json!({"summary":"Incorrectly assume the parent is installed", "edits":[{"file":"SKILL.md", "op":"replace", "old":previous, "new":"the migration step and another check before"}]}).to_string();
                                        }
                                        let mut replay = serde_json::Value::Object(patch.clone());
                                        if mode == 8 {
                                            let remaining = parent["diff"]["allowed_tokens"].as_u64().unwrap() - parent["diff"]["changed_tokens"].as_u64().unwrap();
                                            replay["edits"].as_array_mut().unwrap().push(json!({"file":"SKILL.md", "op":"append", "new":"x ".repeat(usize::try_from(remaining + 1).unwrap())}));
                                        }
                                        return replay.to_string();
                                    }
                                    if system.contains("H2 role: general.") { return ordered_parent_patch().to_string(); }
                                }
                                let qualifier = if system.contains("H2 role: general.") { "general" }
                                    else if system.contains("H2 role: domain.") { "domain" }
                                    else if system.contains("H2 role: compose.") { "composition" } else { "" };
                                if qualifier.is_empty() { good_patch() }
                                else { good_patch().replace("migration step before continuing", &format!("migration step and the {qualifier} check before continuing")) }
                            } else {
                                let hydrated: Vec<_> = messages.iter().filter(|message| message["role"] == "tool" || message["content"].as_str().is_some_and(|text| text.contains("<tool_result name=\"skill_read\">"))).map(|message| message["content"].to_string()).collect();
                                if hydrated.iter().any(|text| text.contains("Step 3:")) {
                                    let mode = h2_behavior.load(Ordering::SeqCst);
                                    if mode == 9 { return if hydrated.iter().any(|text| text.contains("release preflight")) { "MIGRATION-PASS".into() } else { "BASELINE-FAIL".into() }; }
                                    let harmful = mode == 2 && hydrated.iter().any(|text| text.contains("composition check"));
                                    if !harmful && (mode == 1 || hydrated.iter().any(|text| text.contains("migration step"))) { "MIGRATION-PASS".into() } else { "BASELINE-FAIL".into() }
                                } else {
                                    "<tool_call>\n{\"name\":\"skill_read\",\"arguments\":{\"name\":\"Deploy helper\"}}\n</tool_call>".into()
                                }
                            }}, str::to_string);
                            let hydrated: Vec<_> = messages.iter().filter(|message| message["role"] == "tool"
                                || message["content"].as_str().is_some_and(|text| text.contains("<tool_result name=")))
                                .map(|message| message["content"].to_string()).collect();
                            let artifact_call = artifact_wrong.filter(|_| assessment_response.is_none()).and_then(|wrong| {
                                let candidate = hydrated.iter().any(|text| text.contains("migration step"));
                                let called = |name: &str| messages.iter().filter_map(|message| message["tool_calls"].as_array())
                                    .flatten().any(|call| call["function"]["name"] == name);
                                if hydrated.is_empty() { None }
                                else if !called("file_write") { Some(("file_write", json!({"path":"artifact.txt", "content":if candidate {"PASS-TEMP\n"} else {"BASELINE\n"}}))) }
                                else if !called("file_edit") { Some(("file_edit", json!({"path":"artifact.txt", "old_string":if candidate {"PASS-TEMP"} else {"BASELINE"}, "new_string": if candidate && !wrong {"VERIFIED"} else {"WRONG"}}))) }
                                else if !called("file_read") { Some(("file_read", json!({"path":"artifact.txt"}))) }
                                else { None }
                            });
                            let message = if let Some((mut name, mut arguments)) = artifact_call {
                                let tools = body["tools"].as_array().context("missing tools")?;
                                if !tools.iter().any(|tool| tool["function"]["name"] == name) {
                                    arguments = json!({"query":name,"limit":5}); name = "capability_search";
                                }
                                anyhow::ensure!(tools.iter().any(|tool| tool["function"]["name"] == name), "artifact tool or discovery not offered");
                                json!({"role":"assistant","content":null,"tool_calls":[{"id":format!("artifact-{}",count.load(Ordering::SeqCst)),"type":"function","function":{"name":name,"arguments":arguments.to_string()}}]})
                            } else if text.starts_with("<tool_call>") {
                                let offered = body["tools"].as_array().context("solver has no offered tools")?;
                                let name = offered.iter().find_map(|tool| {
                                    (tool["function"]["parameters"]["properties"]["name"]["const"].as_str() == Some("Deploy helper"))
                                        .then(|| tool["function"]["name"].as_str()).flatten()
                                }).context("solver has no offered instruction-read alias")?;
                                json!({"role":"assistant","content":null,"tool_calls":[{"id":format!("fixture-read-{}",count.load(Ordering::SeqCst)),"type":"function","function":{"name":name,"arguments":"{\"name\":\"Deploy helper\"}"}}]})
                            } else { json!({"role":"assistant","content":text}) };
                            let message = if large.load(Ordering::SeqCst) || (h2_behavior.load(Ordering::SeqCst) == 5 && editor_number == Some(0)) { json!({"role":"assistant","content":"x".repeat(256*1024)}) } else { message };
                            let body = json!({"choices":[{"message":message,"finish_reason":"stop"}],"usage":{"prompt_tokens":120,"completion_tokens":30,"total_tokens":150}}).to_string();
                            let reply = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                            socket.write_all(reply.as_bytes()).await?; socket.shutdown().await?; Ok::<_, anyhow::Error>(())
                        });
                    },
                    result = connections.join_next(), if !connections.is_empty() => {
                        // A cancelled client is expected in cancellation cases.
                        if let Some(Err(error)) = result { anyhow::ensure!(error.is_cancelled(), "fixture task panicked"); }
                    }
                }
            }
        });
        Ok(Self {
            url,
            count,
            bodies,
            oversized,
            h2_mode,
            judge_mode,
            judge_count,
            cancel,
            task,
        })
    }
    async fn finish(self) -> Result<()> {
        self.cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), self.task).await??
    }
}
/// The second replacement deliberately targets text made by the first. The
/// final native candidate also changes the allowed description scalar.
fn ordered_parent_patch() -> serde_json::Value {
    json!({
        "summary":"Retain ordered migration guidance",
        "description":"Helps with migration-aware deploys",
        "edits":[
            {"file":"SKILL.md", "op":"replace", "old":"the check number 2 before", "new":"the migration step before"},
            {"file":"SKILL.md", "op":"replace", "old":"the migration step before", "new":"the migration step and general check before"},
        ]
    })
}

fn config(f: &Fixture, server: &Server) -> Config {
    let mut config = Config {
        workspace_dir: f.tmp.path().into(),
        default_provider: Some(format!("custom:{}", server.url)),
        default_model: Some("experiment-loopback".into()),
        api_key: Some("sentinel".into()),
        ..Config::default()
    };
    config.config_path = f.tmp.path().join("config.toml");
    config.cost.prices.insert(
        format!("{}/experiment-loopback", config.default_provider.as_deref().unwrap()),
        crate::config::schema::ModelPricing {
            input: 0.0,
            output: 0.0,
            cache_write: 0.0,
            cache_read: 0.0,
        },
    );
    config.providers.models.push(
        toml::from_str(&format!(
            "provider = {:?}\nmodel = \"experiment-loopback\"\noperations = [\"chat\"]\nnative_tools = true\n",
            config.default_provider.as_deref().unwrap()
        ))
        .expect("test model declaration"),
    );
    config.self_system.evolution_model_max_tokens = 1_000_000;
    config.self_system.evolution_model_max_micro_usd = 1_000_000;
    config.self_system.evolution_judge_timeout_secs = 120;
    config.reliability.provider_retries = 0;
    config.memory.embedding_provider = "none".into();
    config.memory.embedding_provider_explicit = true;
    config
}
fn suite(f: &Fixture) -> crate::skills::lifecycle::FrozenSuite {
    suite_mode(f, false)
}
fn suite_mode(f: &Fixture, artifacts: bool) -> crate::skills::lifecycle::FrozenSuite {
    let cases = [EvalSplit::Development, EvalSplit::Validation, EvalSplit::Holdout].into_iter().flat_map(|split| (0..3).map(move |index| EvalCase {
        case_id: format!("{}-{index}", (match split { EvalSplit::Development => "development", EvalSplit::Validation => "validation", EvalSplit::Holdout => "holdout" })), split, task_family: "deploy".into(),
        prompt: if artifacts { format!("Task {split:?}-{index}: Use skill_read to read Deploy helper. Use file_write to create artifact.txt, file_edit to correct its migration marker, then file_read to verify artifact.txt. Follow the skill and report the migration result.") } else { format!("Task {split:?}-{index}: Use skill_read to read Deploy helper and follow its instructions. Then report the migration result.") },
        fixtures: vec![], validators: if artifacts { vec![Validator::ResponseContains { text: "MIGRATION-PASS".into() }, Validator::ArtifactContains { path:"artifact.txt".into(), text:"VERIFIED".into() }] } else { vec![Validator::ResponseContains { text: "MIGRATION-PASS".into() }] }, expect_skill_read: true, critical: true, max_elapsed_ms: 30_000,
    })).collect();
    EvalSuite {
        schema_version: 1,
        suite_id: "experiment-suite".into(),
        owner_scope: f.store.owner_scope().clone(),
        skill_id: f.pin.skill_id.clone(),
        trials: 1,
        min_cases: 3,
        allowed_regressions: 0,
        deterministic: true,
        min_gain_cases: 1,
        validation_attempts: 2,
        holdout_attempts: 1,
        required_tools: if artifacts {
            vec!["file_write".into(), "file_edit".into(), "file_read".into()]
        } else {
            vec![]
        },
        side_effects: if artifacts {
            SideEffects::WorkspaceWrite
        } else {
            SideEffects::None
        },
        budget: SuiteBudget {
            max_runs: 64,
            max_total_elapsed_ms: 300_000,
            max_total_tokens: 1_000_000,
            max_total_cost_micros: 1_000_000,
        },
        cases,
    }
    .freeze()
    .unwrap()
}
fn service(f: &Fixture, suite: &crate::skills::lifecycle::FrozenSuite) -> Arc<SkillsLifecycleService> {
    let service = Arc::new(
        SkillsLifecycleService::new(
            f.store.clone(),
            SkillsLifecycleSettings {
                enabled: true,
                mode: LifecycleMode::Shadow,
                suites: SuiteRegistry::new([(f.pin.skill_id.clone(), suite.digest().into())]).unwrap(),
                ..Default::default()
            },
        )
        .unwrap(),
    );
    service.install_operator_suite(suite).unwrap();
    service
}
fn plan(
    f: &Fixture,
    service: &SkillsLifecycleService,
    config: &Config,
    patterns: Vec<PatternId>,
) -> Result<(FrozenExperiment, BTreeMap<String, ExperimentOperation>)> {
    let mut spec = crate::skills::lifecycle::experiment::test_specification();
    spec.id = ExperimentId::parse("experiment-integration")?;
    spec.pins.skill_id = f.pin.skill_id.clone();
    spec.created_at = Utc::now();
    spec.elapsed_ms = 300_000;
    spec.expires_at = spec.created_at + chrono::Duration::milliseconds(300_000);
    spec.resources = ExperimentResources {
        calls: 128,
        tokens: 1_000_000,
        micro_usd: 1_000_000,
        tool_calls: 128,
        output_bytes: 1_000_000,
        scratch_bytes: 64 * 1024 * 1024,
    };
    let inputs: BTreeMap<String, ExperimentOperation> = BTreeMap::from([
        (
            "proposal".into(),
            ExperimentOperation::Proposal {
                patterns,
                max_input_bytes: 64 * 1024,
                max_output_bytes: 16 * 1024,
                timeout_ms: 120_000,
            },
        ),
        (
            "evaluation".into(),
            ExperimentOperation::Evaluation {
                proposal_stage: "proposal".into(),
                split: EvalSplit::Validation,
            },
        ),
        (
            "verify".into(),
            ExperimentOperation::Verify {
                evaluation_stage: "evaluation".into(),
            },
        ),
    ]);
    spec.stages = inputs
        .iter()
        .map(|(id, input)| ExperimentStage {
            id: id.clone(),
            kind: input.kind(),
            input_sha256: input.digest().unwrap(),
            dependencies: match id.as_str() {
                "evaluation" => vec!["proposal".into()],
                "verify" => vec!["evaluation".into()],
                _ => vec![],
            },
            allocation: match input {
                ExperimentOperation::Proposal { .. }
                | ExperimentOperation::DevelopmentProposal { .. }
                | ExperimentOperation::GateReview { .. } => ExperimentResources {
                    calls: 4,
                    tokens: 131_072,
                    micro_usd: 100_000,
                    tool_calls: 0,
                    output_bytes: 131_072,
                    scratch_bytes: 8 * 1024 * 1024,
                },
                ExperimentOperation::Evaluation { .. }
                | ExperimentOperation::StudyTrial { .. }
                | ExperimentOperation::DevelopmentProbe { .. }
                | ExperimentOperation::SelectedEvaluation { .. } => ExperimentResources {
                    calls: 64,
                    tokens: 500_000,
                    micro_usd: 500_000,
                    tool_calls: 64,
                    output_bytes: 500_000,
                    scratch_bytes: 32 * 1024 * 1024,
                },
                ExperimentOperation::Verify { .. } | ExperimentOperation::DevelopmentSelection { .. } => {
                    ExperimentResources::default()
                }
            },
        })
        .collect();
    // The map is lexicographic; the immutable plan must be topological.
    spec.stages.sort_by_key(|stage| match stage.id.as_str() {
        "proposal" => 0,
        "evaluation" => 1,
        _ => 2,
    });
    let frozen = service.plan_experiment(spec, config)?;
    service.admit_experiment(frozen.spec().clone(), config)?;
    Ok((frozen, inputs))
}
async fn run_stage(
    service: &Arc<SkillsLifecycleService>,
    config: &Config,
    plan: &FrozenExperiment,
    id: &str,
    input: &ExperimentOperation,
) -> Result<ExperimentJournal> {
    let cancel = CancellationToken::new();
    CleanupScope::new(None, cancel.clone())?
        .run(service.run_experiment_stage(
            config,
            plan.spec().id.clone(),
            plan.digest().into(),
            id.into(),
            input.clone(),
            &cancel,
        ))
        .await
}

#[tokio::test]
async fn experiment_production_builder_runs_one_proposal_and_refuses_replay() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    let result = async {
        let config = config(&f, &server);
        let suite = suite(&f);
        let service = service(&f, &suite);
        let (plan, inputs) = plan(&f, &service, &config, vec![pattern.pattern_id])?;
        assert_eq!(
            server.count.load(Ordering::SeqCst),
            0,
            "planning and admission must not send"
        );
        let journal = run_stage(&service, &config, &plan, "proposal", &inputs["proposal"]).await?;
        assert!(journal.state.stages["proposal"].successful());
        assert!(!service.experiment_qualified(&journal)?);
        assert_eq!(server.count.load(Ordering::SeqCst), 1);
        assert!(
            run_stage(&service, &config, &plan, "proposal", &inputs["proposal"])
                .await
                .is_err()
        );
        assert_eq!(server.count.load(Ordering::SeqCst), 1, "replay must not send again");
        let binding = f.store.active_binding(&f.pin.skill_id)?.unwrap();
        assert_eq!(binding.generation, f.pin.generation);
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn experiment_solver_and_independent_verifier_use_the_same_admitted_plan() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    let result = async {
        let config = config(&f, &server);
        let suite = suite(&f);
        let service = service(&f, &suite);
        let (plan, inputs) = plan(&f, &service, &config, vec![pattern.pattern_id])?;
        for stage in ["proposal", "evaluation", "verify"] {
            let journal = run_stage(&service, &config, &plan, stage, &inputs[stage])
                .await
                .with_context(|| format!("stage {stage}"))?;
            if stage == "verify" {
                if !service.experiment_qualified(&journal)? {
                    let evaluations: Vec<_> = f
                        .store
                        .list_evaluation_ids()?
                        .iter()
                        .map(|id| {
                            let verified = f.store.verify_evaluation(id, None).unwrap();
                            json!({"receipt":verified.receipt,"cases":verified.cases,"summary":verified.summary})
                        })
                        .collect();
                    let path = format!("/tmp/prx-m1-solver-failure-{}.json", uuid::Uuid::new_v4());
                    std::fs::write(
                        &path,
                        serde_json::to_vec_pretty(
                            &json!({"evaluations":evaluations,"requests":*server.bodies.lock()}),
                        )?,
                    )?;
                    anyhow::bail!(
                        "independent verification did not qualify; evidence retained at {path}; state={:?}",
                        journal.state
                    );
                }
            }
        }
        let journal = service.inspect_experiment(&plan.spec().id, plan.digest())?;
        assert!(journal.state.committed(&plan)?.within(plan.spec().resources));
        // Completed receipts cannot be transplanted into another experiment
        // or another frozen stage input, even with the same candidate.
        for change_input in [false, true] {
            let mut transplanted = journal.clone();
            let mut spec = transplanted.plan.spec().clone();
            if change_input {
                spec.stages[1].input_sha256 = "f".repeat(64);
            } else {
                spec.id = ExperimentId::parse("another-experiment")?;
            }
            transplanted.plan = spec.freeze()?;
            assert!(!service.experiment_qualified(&transplanted)?);
        }
        let mut wrong_reference = journal;
        for entry in &mut wrong_reference.entries {
            if let ExperimentEvent::Settled { stage, reference, .. } = &mut entry.event {
                if stage == "verify" {
                    *reference = Some("not-an-evaluation".into());
                }
            }
        }
        assert!(!service.experiment_qualified(&wrong_reference)?);
        assert_eq!(
            server.count.load(Ordering::SeqCst),
            13,
            "one proposer and twelve baseline/candidate physical calls"
        );
        assert_eq!(
            f.store.active_binding(&f.pin.skill_id)?.unwrap().generation,
            f.pin.generation,
            "Shadow must not publish"
        );
        assert!(
            f.tmp
                .path()
                .read_dir()?
                .filter_map(std::result::Result::ok)
                .all(|entry| !entry.file_name().to_string_lossy().starts_with("prx-experiment-eval-"))
        );
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn experiment_durable_cancel_reaches_active_provider_and_recovery_never_retries() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(true).await?;
    let result = async {
        let config = config(&f, &server); let suite = suite(&f); let service = service(&f, &suite);
        let (plan, inputs) = plan(&f, &service, &config, vec![pattern.pattern_id])?;
        let mut running = Box::pin(run_stage(&service, &config, &plan, "proposal", &inputs["proposal"]));
        tokio::time::timeout(Duration::from_secs(20), async {
            loop { tokio::select! { result = &mut running => anyhow::bail!("provider ended before cancellation: {result:?}"), () = tokio::time::sleep(Duration::from_millis(20)) => { if server.count.load(Ordering::SeqCst) == 1 { break Ok::<(), anyhow::Error>(()); } } } }
        }).await??;
        service.cancel_experiment(&plan.spec().id, plan.digest())?;
        assert!(tokio::time::timeout(Duration::from_secs(20), running).await?.is_err());
        let recovered = service.recover_experiment(&plan.spec().id, plan.digest())?;
        assert!(matches!(recovered.state.stages["proposal"], StageState::DispatchUnknown));
        assert!(recovered.state.cancelled); assert!(!service.experiment_qualified(&recovered)?);
        assert_eq!(recovered.state.committed(&plan)?, plan.spec().stages[0].allocation);
        let again = service.recover_experiment(&plan.spec().id, plan.digest())?; assert_eq!(again.head_sha256, recovered.head_sha256);
        assert!(run_stage(&service, &config, &plan, "proposal", &inputs["proposal"]).await.is_err());
        assert_eq!(server.count.load(Ordering::SeqCst), 1);
        let proposals = f.store.list_proposals()?; assert_eq!(proposals.len(), 1); assert!(!proposals[0].status.is_live());
        Ok(())
    }.await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn experiment_native_write_edit_read_requires_independently_correct_artifacts() -> Result<()> {
    for wrong in [false, true] {
        let f = fixture();
        let pattern = usable_pattern(&f);
        let server = Server::start_mode(false, Some(wrong)).await?;
        let mut result: Result<()> = async {
            let config = config(&f, &server);
            let suite = suite_mode(&f, true);
            let service = service(&f, &suite);
            let (plan, inputs) = plan(&f, &service, &config, vec![pattern.pattern_id])?;
            for stage in ["proposal", "evaluation", "verify"] {
                run_stage(&service, &config, &plan, stage, &inputs[stage])
                    .await
                    .with_context(|| format!("artifact stage {stage}, wrong={wrong}"))?;
            }
            let journal = service.inspect_experiment(&plan.spec().id, plan.digest())?;
            anyhow::ensure!(
                service.experiment_qualified(&journal)? != wrong,
                "completed model output must not override the independent artifact verdict; wrong={wrong}"
            );
            let calls = server.count.load(Ordering::SeqCst);
            assert!(
                (31..=49).contains(&calls),
                "six paired native trials plus admitted progressive discovery: {calls}"
            );
            assert_eq!(journal.state.committed(&plan)?.calls, u64::try_from(calls)?);
            assert_eq!(
                f.store.active_binding(&f.pin.skill_id)?.unwrap().generation,
                f.pin.generation
            );
            for id in f.store.list_evaluation_ids()? {
                let verified = f.store.verify_evaluation(&id, None)?;
                assert_eq!(verified.outcome.is_pass(), !wrong);
            }
            assert!(
                f.tmp
                    .path()
                    .read_dir()?
                    .filter_map(std::result::Result::ok)
                    .all(|entry| !entry.file_name().to_string_lossy().starts_with("prx-experiment-eval-"))
            );
            Ok(())
        }
        .await;
        if let Err(error) = &result {
            let evaluations: Vec<_> = f
                .store
                .list_evaluation_ids()?
                .iter()
                .map(|id| match f.store.verify_evaluation(id, None) {
                    Ok(value) => json!({"receipt":value.receipt,"cases":value.cases,"summary":value.summary}),
                    Err(error) => json!({"error":error.to_string()}),
                })
                .collect();
            let path = format!("/tmp/prx-m1-artifact-failure-{}.json", uuid::Uuid::new_v4());
            std::fs::write(
                &path,
                serde_json::to_vec_pretty(
                    &json!({"error":error.to_string(),"requests":*server.bodies.lock(),"evaluations":evaluations}),
                )?,
            )?;
            result = result.map_err(|error| error.context(format!("artifact failure evidence: {path}")));
        }
        server.finish().await?;
        result?;
    }
    Ok(())
}

#[tokio::test]
async fn experiment_changed_operation_or_configuration_refuses_before_model_send() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    let result = async {
        let mut config = config(&f, &server);
        let suite = suite(&f);
        let service = service(&f, &suite);
        let (plan, inputs) = plan(&f, &service, &config, vec![pattern.pattern_id])?;
        let mut altered = inputs["proposal"].clone();
        if let ExperimentOperation::Proposal { max_input_bytes, .. } = &mut altered {
            *max_input_bytes -= 1;
        }
        assert!(run_stage(&service, &config, &plan, "proposal", &altered).await.is_err());
        let mut no_storage = plan.spec().clone();
        no_storage.id = ExperimentId::parse("experiment-no-storage")?;
        no_storage.stages[0].allocation.scratch_bytes = 0;
        let no_storage = no_storage.freeze()?;
        service.admit_experiment(no_storage.spec().clone(), &config)?;
        assert!(
            run_stage(&service, &config, &no_storage, "proposal", &inputs["proposal"])
                .await
                .is_err()
        );
        assert_eq!(
            service
                .inspect_experiment(&no_storage.spec().id, no_storage.digest())?
                .entries
                .len(),
            0
        );
        config.cost.daily_limit_usd += 1.0;
        assert!(
            run_stage(&service, &config, &plan, "proposal", &inputs["proposal"])
                .await
                .is_err()
        );
        assert_eq!(server.count.load(Ordering::SeqCst), 0);
        assert_eq!(
            service
                .inspect_experiment(&plan.spec().id, plan.digest())?
                .entries
                .len(),
            0
        );
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn experiment_dropped_waiter_retains_owned_cancel_and_unknown_receipt() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(true).await?;
    let result = async {
        let config = config(&f, &server); let suite = suite(&f); let service = service(&f, &suite);
        let (plan, inputs) = plan(&f, &service, &config, vec![pattern.pattern_id])?;
        let mut running = Box::pin(run_stage(&service, &config, &plan, "proposal", &inputs["proposal"]));
        tokio::time::timeout(Duration::from_secs(20), async {
            loop { tokio::select! {
                result = &mut running => anyhow::bail!("provider ended before waiter drop: {result:?}"),
                () = tokio::time::sleep(Duration::from_millis(20)) => if server.count.load(Ordering::SeqCst) == 1 { break; },
            } }
            Ok::<(), anyhow::Error>(())
        }).await??;
        drop(running);
        let recovered = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let journal = service.inspect_experiment(&plan.spec().id, plan.digest())?;
                if matches!(journal.state.stages["proposal"], StageState::DispatchUnknown) { break Ok::<_, anyhow::Error>(journal); }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await??;
        assert!(!service.experiment_qualified(&recovered)?);
        let before = recovered.head_sha256.clone();
        let observation = service.experiment_physical_observations(&recovered, &config)?;
        assert_eq!(observation["proposal"]["needs_reconciliation"], true);
        assert_eq!(observation["proposal"]["physical"]["calls"], 1);
        assert_eq!(observation["proposal"]["physical"]["unresolved"], true);
        assert_eq!(before, service.inspect_experiment(&plan.spec().id, plan.digest())?.head_sha256);
        assert!(run_stage(&service, &config, &plan, "proposal", &inputs["proposal"]).await.is_err());
        assert_eq!(server.count.load(Ordering::SeqCst), 1);
        Ok(())
    }.await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn experiment_oversized_http_body_keeps_liability_and_refuses_another_send() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    server.oversized.store(true, Ordering::SeqCst);
    let result = async {
        let config = config(&f, &server);
        let suite = suite(&f);
        let service = service(&f, &suite);
        let (plan, inputs) = plan(&f, &service, &config, vec![pattern.pattern_id])?;
        assert!(
            run_stage(&service, &config, &plan, "proposal", &inputs["proposal"])
                .await
                .is_err()
        );
        let journal = service.inspect_experiment(&plan.spec().id, plan.digest())?;
        assert!(matches!(journal.state.stages["proposal"], StageState::DispatchUnknown));
        assert!(!service.experiment_qualified(&journal)?);
        let observed = service.experiment_physical_observations(&journal, &config)?;
        assert_eq!(observed["proposal"]["physical"]["calls"], 1);
        assert_eq!(observed["proposal"]["physical"]["unresolved"], true);
        assert!(
            run_stage(&service, &config, &plan, "proposal", &inputs["proposal"])
                .await
                .is_err()
        );
        assert_eq!(server.count.load(Ordering::SeqCst), 1);
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn experiment_competing_service_callers_dispatch_once() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    let result = async {
        let config = config(&f, &server);
        let suite = suite(&f);
        let service = service(&f, &suite);
        let (plan, inputs) = plan(&f, &service, &config, vec![pattern.pattern_id])?;
        let (left, right) = tokio::join!(
            run_stage(&service, &config, &plan, "proposal", &inputs["proposal"]),
            run_stage(&service, &config, &plan, "proposal", &inputs["proposal"])
        );
        assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
        assert_eq!(server.count.load(Ordering::SeqCst), 1);
        let journal = service.recover_experiment(&plan.spec().id, plan.digest())?;
        assert!(journal.state.stages["proposal"].successful());
        assert_eq!(journal.state.committed(&plan)?.calls, 1);
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

mod product;
