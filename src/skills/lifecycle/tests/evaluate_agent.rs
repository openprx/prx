#![cfg(test)]
#![allow(clippy::indexing_slicing)]

//! SL06 through the real agent entry (`crate::agent::run`) against a
//! deterministic loopback model on 127.0.0.1. No network, no real credential:
//! the only key in play is a sentinel literal.

use super::{activate, entry, owner};
use crate::config::Config;
use crate::skills::lifecycle::{
    AccountingOwner, AgentCaseExecutor, Arm, Bundle, CaseVerdict, EvalCase, EvalSplit, EvalSuite, EvaluationOutcome,
    EvaluationRequest, InvalidReason, NewRevision, RevisionId, SideEffects, SkillId, SkillLifecycleStore, SourceKind,
    SuiteBudget, Validator,
};
use parking_lot::Mutex;
use std::net::SocketAddr;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const OWNER: &str = "owner:eval:cli:alice";
const MODEL: &str = "eval-loopback-model";
/// What the "model" says after it has read a skill that contains `ANSWER=<x>`.
const FINAL_PREFIX: &str = "FINAL:";

fn skill() -> SkillId {
    SkillId::parse("release-notes").expect("test: skill id")
}

/// A scripted model. It decides from the request alone:
/// * the skill text `ANSWER=<token>` is present in the conversation: `FINAL:<token>`;
/// * the user asked for the release-notes skill: it calls `skill_read`;
/// * `ZEROBODY` in the prompt: an empty reply;
/// * anything else: `plain answer`.
struct Loopback {
    addr: SocketAddr,
    bodies: Arc<Mutex<Vec<String>>>,
    server: JoinHandle<()>,
}

fn decide(body: &str) -> String {
    let json: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let last_user = json
        .get("messages")
        .and_then(|m| m.as_array())
        .and_then(|messages| {
            messages
                .iter()
                .rev()
                .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
        })
        .map(|m| m.get("content").map(ToString::to_string).unwrap_or_default())
        .unwrap_or_default();
    if let Some(at) = body.find("ANSWER=") {
        let token: String = body[at + "ANSWER=".len()..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        return format!("{FINAL_PREFIX}{token}");
    }
    if last_user.contains("ZEROBODY") {
        return String::new();
    }
    if last_user.contains("release notes") {
        return "<tool_call>\n{\"name\":\"skill_read\",\"arguments\":{\"name\":\"release-notes\"}}\n</tool_call>"
            .to_string();
    }
    "plain answer".to_string()
}

impl Loopback {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test: bind loopback");
        let addr = listener.local_addr().expect("test: address");
        let bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&bodies);
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let sink = Arc::clone(&sink);
                tokio::spawn(async move {
                    let mut buffer: Vec<u8> = Vec::new();
                    let mut chunk = [0_u8; 8192];
                    let head_end = loop {
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                        }
                        if let Some(p) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                            break p + 4;
                        }
                    };
                    let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
                    let path = head.split_whitespace().nth(1).unwrap_or_default().to_string();
                    let length: usize = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    while buffer.len() < head_end + length {
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let body = String::from_utf8_lossy(&buffer[head_end..]).to_string();
                    let payload = if path.contains("chat/completions") {
                        let reply = decide(&body);
                        sink.lock().push(body);
                        serde_json::json!({
                            "id": "chatcmpl-eval", "object": "chat.completion", "created": 1, "model": MODEL,
                            "choices": [{"index": 0, "message": {"role": "assistant", "content": reply}, "finish_reason": "stop"}],
                            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                        })
                        .to_string()
                    } else {
                        serde_json::json!({"object": "list", "data": [{"id": MODEL, "object": "model", "created": 1, "owned_by": "eval"}]}).to_string()
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                        payload.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.flush().await;
                });
            }
        });
        Self { addr, bodies, server }
    }

    fn provider(&self) -> String {
        format!("custom:http://{}/v1", self.addr)
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn template(root: &std::path::Path, loopback: &Loopback) -> Config {
    let mut config = Config::default();
    config.workspace_dir = root.join("workspace");
    config.config_path = root.join("config.toml");
    config.default_provider = Some(loopback.provider());
    config.default_model = Some(MODEL.to_string());
    config.api_key = Some("EVAL-LOOPBACK-SENTINEL".to_string());
    config.api_url = None;
    config.model_routes = Vec::new();
    config.reliability.fallback_providers = Vec::new();
    config.reliability.api_keys = Vec::new();
    config.reliability.provider_retries = 0;
    config.reliability.provider_backoff_ms = 0;
    config.reliability.scheduler_retries = 0;
    config.auth.codex_auth_json_auto_import = false;
    config.auth.codex_auth_json_path = root.join("no-codex-auth.json");
    config.memory.backend = "sqlite".to_string();
    let provider = loopback.provider();
    config.cost.prices.insert(
        format!("{provider}/{MODEL}"),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    config.providers.models.push(
        toml::from_str(&format!(
            "provider = {provider:?}\nmodel = {MODEL:?}\noperations = [\"chat\"]\nnative_tools = true\n"
        ))
        .expect("test: model declaration"),
    );
    config
}

fn write_revision(store: &SkillLifecycleStore, body: &str) -> (RevisionId, String) {
    let bundle = Bundle::from_entries(vec![entry(
        "SKILL.md",
        format!("---\nname: release-notes\ndescription: Writes release notes\n---\n\n{body}\n").as_bytes(),
    )])
    .expect("test: bundle");
    let written = store
        .write_revision(&NewRevision::new(skill(), SourceKind::Operator), &bundle)
        .expect("test: write revision");
    (written.revision.revision_id, written.record_digest)
}

fn case(id: &str, prompt: &str, validator: Validator, expect_read: bool) -> EvalCase {
    EvalCase {
        case_id: id.to_string(),
        split: EvalSplit::Validation,
        task_family: "notes".to_string(),
        prompt: prompt.to_string(),
        fixtures: Vec::new(),
        validators: vec![validator],
        expect_skill_read: expect_read,
        critical: false,
        max_elapsed_ms: 120_000,
    }
}

fn suite(cases: Vec<EvalCase>, min_cases: u32) -> crate::skills::lifecycle::FrozenSuite {
    EvalSuite {
        schema_version: 1,
        suite_id: "entry-suite".to_string(),
        owner_scope: owner(OWNER),
        skill_id: skill(),
        trials: 1,
        min_cases,
        allowed_regressions: 0,
        deterministic: true,
        min_gain_cases: 1,
        validation_attempts: 3,
        holdout_attempts: 1,
        required_tools: Vec::new(),
        side_effects: SideEffects::None,
        budget: SuiteBudget {
            max_runs: 32,
            max_total_elapsed_ms: 900_000,
            max_total_tokens: 1_000_000,
            max_total_cost_micros: 1_000_000,
        },
        cases,
    }
    .freeze()
    .expect("test: freeze")
}

#[tokio::test]
async fn the_real_agent_entry_separates_a_better_skill_revision_from_the_baseline() {
    let tmp = TempDir::new().expect("test: tempdir");
    let loopback = Loopback::start().await;
    std::fs::create_dir_all(tmp.path().join("owner-ws")).expect("test: owner workspace");
    let store = SkillLifecycleStore::open(&tmp.path().join("owner-ws"), owner(OWNER)).expect("test: store");
    let a = write_revision(&store, "ANSWER=draft");
    let b = write_revision(&store, "ANSWER=report-v2");
    activate(&store, &skill(), &a, 0, true);

    let oracle = format!("{FINAL_PREFIX}report-v2");
    let suite = suite(
        vec![
            case(
                "pos-0",
                "Write the release notes for build one.",
                Validator::ResponseContains { text: oracle.clone() },
                true,
            ),
            case(
                "pos-1",
                "Write the release notes for build two.",
                Validator::ResponseContains { text: oracle.clone() },
                true,
            ),
            case(
                "neg-0",
                "What is two plus two?",
                Validator::ResponseContains {
                    text: "plain answer".to_string(),
                },
                false,
            ),
        ],
        2,
    );
    let scratch = tmp.path().join("scratch");
    let accounting_dir = tmp.path().join("operator-ws");
    std::fs::create_dir_all(&accounting_dir).expect("test: operator ws");
    let executor = AgentCaseExecutor::new(
        template(tmp.path(), &loopback),
        scratch.clone(),
        0.0,
        AccountingOwner::new(accounting_dir.clone()),
    );
    let receipt = store
        .evaluate(
            &EvaluationRequest {
                suite: &suite,
                split: EvalSplit::Validation,
                baseline: Some(a.0.clone()),
                candidate: b.0.clone(),
                observed_generation: 1,
            },
            &executor,
            &CancellationToken::new(),
        )
        .await
        .expect("test: evaluate");

    let verified = store.verify_evaluation(&receipt.record_id, None).expect("test: verify");
    assert_eq!(verified.outcome, EvaluationOutcome::Improved, "{:#?}", verified.cases);
    assert_eq!(verified.summary.candidate_passed, 3);
    assert_eq!(
        verified.summary.baseline_passed, 1,
        "only the negative-trigger case passes without the new revision"
    );
    assert_eq!(verified.cases.len(), 6);

    for case in &verified.cases {
        let arm_revision = if case.arm == Arm::Candidate { &b.0 } else { &a.0 };
        let installed = case.run.installed.as_ref().expect("test: a skill was installed");
        assert_eq!(&installed.revision_id, arm_revision);
        assert!(
            case.run.tokens.is_some_and(|t| t > 0),
            "usage comes from the run's own ledger"
        );
        assert!(case.run.cost_micros.is_some_and(|c| c > 0));
        assert_eq!(case.run.models_used.len(), 1, "{:?}", case.run.models_used);
        if case.case_id.starts_with("pos") {
            // The real runtime served the pinned revision of this arm and joined a terminal.
            assert_eq!(case.run.hydrations.len(), 1, "{}: {:?}", case.case_id, case.verdict);
            assert_eq!(&case.run.hydrations[0].revision_id, arm_revision);
            assert!(case.run.experience_terminal.is_some());
        } else {
            assert!(
                case.run.hydrations.is_empty(),
                "the unrelated task must not load the skill"
            );
        }
    }
    let bodies = loopback.bodies.lock();
    assert!(bodies.iter().any(|b| b.contains("ANSWER=draft")));
    assert!(bodies.iter().any(|b| b.contains("ANSWER=report-v2")));
    assert!(
        bodies.iter().all(|b| !b.contains(&oracle)),
        "the expected answer never reached the model"
    );
    drop(bodies);
    // The surface the model was actually offered on the wire.
    let forbidden = [
        "web_search_tool",
        "skills_manage",
        "skill_execute",
        "shell",
        "mcp_call",
        "cron",
        "delegate",
        "sessions_spawn",
        "http_request",
        "file_write",
        "file_edit",
        "file_read",
        "xin",
        "pushover",
        "proxy_config",
        "document_ingest",
        "message_send",
    ];
    let mut offered_somewhere = false;
    for body in loopback.bodies.lock().iter() {
        let json: serde_json::Value = serde_json::from_str(body).expect("test: request json");
        let names: Vec<String> = json
            .get("tools")
            .and_then(|t| t.as_array())
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|t| t.pointer("/function/name").or_else(|| t.get("name")))
                    .filter_map(|n| n.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if names.iter().any(|n| n == "skill_read") {
            offered_somewhere = true;
        }
        for name in forbidden {
            assert!(
                !names.iter().any(|n| n == name),
                "{name} was offered on the wire: {names:?}"
            );
        }
    }
    assert!(
        offered_somewhere,
        "skill_read must be offered through the explicit allowlist"
    );
    // The run was charged to the operator's ledger, not only to its scratch one.
    let ledger = std::fs::read_to_string(accounting_dir.join("state/costs.jsonl")).unwrap_or_default();
    assert!(ledger.lines().count() >= 6, "{ledger}");
    let leftovers: Vec<_> = std::fs::read_dir(&scratch)
        .map(|d| d.flatten().collect())
        .unwrap_or_default();
    assert!(leftovers.is_empty(), "every task workspace was removed: {leftovers:?}");
}

#[tokio::test]
async fn an_empty_model_reply_through_the_real_entry_can_never_pass() {
    let tmp = TempDir::new().expect("test: tempdir");
    let loopback = Loopback::start().await;
    std::fs::create_dir_all(tmp.path().join("owner-ws")).expect("test: owner workspace");
    let store = SkillLifecycleStore::open(&tmp.path().join("owner-ws"), owner(OWNER)).expect("test: store");
    let a = write_revision(&store, "ANSWER=draft");
    let b = write_revision(&store, "ANSWER=report-v2");
    activate(&store, &skill(), &a, 0, true);
    // Both arms get an empty reply, which also satisfies an "excludes" validator.
    let suite = suite(
        ["empty-0", "empty-1"]
            .into_iter()
            .map(|id| {
                case(
                    id,
                    &format!("ZEROBODY please answer {id}."),
                    Validator::ResponseExcludes {
                        text: "NEVER-PRESENT".to_string(),
                    },
                    false,
                )
            })
            .collect(),
        2,
    );
    let accounting_dir = tmp.path().join("operator-ws");
    std::fs::create_dir_all(&accounting_dir).expect("test: operator ws");
    let executor = AgentCaseExecutor::new(
        template(tmp.path(), &loopback),
        tmp.path().join("scratch"),
        0.0,
        AccountingOwner::new(accounting_dir),
    );
    let receipt = store
        .evaluate(
            &EvaluationRequest {
                suite: &suite,
                split: EvalSplit::Validation,
                baseline: Some(a.0),
                candidate: b.0,
                observed_generation: 1,
            },
            &executor,
            &CancellationToken::new(),
        )
        .await
        .expect("test: evaluate");
    assert!(!receipt.outcome.is_pass(), "{:?}", receipt.outcome);
    let verified = store.verify_evaluation(&receipt.record_id, None).expect("test: verify");
    assert!(
        verified.cases.iter().all(|c| matches!(
            &c.verdict,
            CaseVerdict::Invalid {
                reason: InvalidReason::ZeroBody | InvalidReason::Partial,
                ..
            }
        )),
        "{:#?}",
        verified.cases.iter().map(|c| &c.verdict).collect::<Vec<_>>()
    );
    assert!(matches!(verified.outcome, EvaluationOutcome::Incomplete { .. }));
}

fn dummy_request<'a>(
    denied: &'a [std::path::PathBuf],
    tools: &'a [String],
    effects: SideEffects,
) -> crate::skills::lifecycle::ExecutionRequest<'a> {
    crate::skills::lifecycle::ExecutionRequest {
        run_id: "evalrun-x",
        prompt: "p",
        fixtures: &[],
        collect_artifacts: &[],
        skill: None,
        deadline: std::time::Duration::from_secs(1),
        required_tools: tools,
        side_effects: effects,
        denied_paths: denied,
    }
}

#[tokio::test]
async fn the_evaluation_config_is_built_from_an_allowlist_and_cannot_reach_the_store() {
    let tmp = TempDir::new().expect("test: tempdir");
    let loopback = Loopback::start().await;
    let mut hostile = template(tmp.path(), &loopback);
    hostile.memory.backend = "postgres".to_string();
    hostile.memory.auto_save = true;
    hostile.security.workspace_hooks = crate::config::schema::WorkspaceHooks::On;
    hostile.observability.backend = "otel".to_string();
    hostile.autonomy.level = crate::security::AutonomyLevel::Full;
    hostile.autonomy.workspace_only = false;
    hostile.tool_tiering.always_include = vec!["shell".to_string()];
    hostile.http_request.timeout_secs = 1;
    hostile.web_search.max_results = 99;
    hostile.browser.allowed_domains.push("evil.example".to_string());
    let executor = AgentCaseExecutor::new(
        hostile,
        tmp.path().join("scratch"),
        0.0,
        AccountingOwner::new(tmp.path().to_path_buf()),
    );
    let ws = tmp.path().join("run-ws");
    std::fs::create_dir_all(&ws).expect("test: ws");
    let store_dir = tmp.path().join("owner-ws");
    std::fs::create_dir_all(&store_dir).expect("test: owner ws");
    let store = SkillLifecycleStore::open(&store_dir, owner(OWNER)).expect("test: store");
    let denied = [store.root().to_path_buf()];

    let none: Vec<String> = Vec::new();
    let config = executor
        .run_config(&ws, &dummy_request(&denied, &none, SideEffects::None))
        .expect("test: config");
    let default = Config::default();
    let json = |value: &dyn erased::Ser| value.to_json();
    assert_eq!(config.memory.backend, "sqlite");
    assert!(!config.memory.auto_save);
    assert_eq!(config.memory.embedding_provider, "none");
    assert_eq!(
        config.security.workspace_hooks,
        crate::config::schema::WorkspaceHooks::Off
    );
    assert_eq!(config.observability.backend, "none");
    assert_eq!(config.autonomy.level, crate::security::AutonomyLevel::ReadOnly);
    assert!(config.autonomy.workspace_only);
    assert_eq!(config.workspace_dir, ws);
    assert!(config.config_path.starts_with(&ws));
    assert_eq!(config.tool_tiering.always_include, ["skill_read"]);
    for name in [
        "shell",
        "http_request",
        "web_search_tool",
        "skills_manage",
        "skill_execute",
        "mcp_call",
        "cron",
        "delegate",
        "sessions_spawn",
        "file_read",
        "file_write",
        "file_edit",
        "xin",
        "pushover",
        "proxy_config",
        "document_ingest",
        "message_send",
    ] {
        assert!(
            config.tool_tiering.always_exclude.iter().any(|n| n == name),
            "{name} must be withheld"
        );
    }
    assert_eq!(json(&config.http_request), json(&default.http_request));
    assert_eq!(json(&config.web_search), json(&default.web_search));
    assert_eq!(json(&config.browser), json(&default.browser));
    // The tool surface the turn publishes: nothing outside the allowlist is direct or discoverable.
    let policy = crate::runtime::turn_publication::surface_policy(
        crate::causal_tree::state::TurnSurfaceKind::AgentLoop,
        &config.tool_tiering,
    );
    assert!(policy.direct_names.contains("skill_read"));
    for name in crate::tools::STATIC_PUBLIC_TOOL_NAMES {
        if matches!(*name, "skill_read" | "capability_search") {
            assert!(!policy.excluded_names.contains(*name), "{name}");
        } else {
            assert!(
                policy.excluded_names.contains(*name),
                "{name} must be excluded from the surface"
            );
            assert!(!policy.direct_names.contains(*name), "{name} must not be a direct tool");
        }
    }
    assert_eq!(json(&config.mcp), json(&default.mcp));
    assert_eq!(json(&config.channels_config), json(&default.channels_config));
    assert_eq!(json(&config.agents), json(&default.agents));
    assert_eq!(json(&config.cron), json(&default.cron));
    assert_eq!(json(&config.sessions_spawn), json(&default.sessions_spawn));

    // The file tools run under this policy and cannot read the suite oracle.
    let secret = store.root().join("suites").join("oracle.json");
    std::fs::create_dir_all(secret.parent().expect("test: parent")).expect("test: suites dir");
    std::fs::write(&secret, b"ORACLE").expect("test: oracle");
    let policy = std::sync::Arc::new(crate::security::SecurityPolicy::from_config(&config.autonomy, &ws));
    assert!(!policy.is_path_allowed(&secret.to_string_lossy()));
    assert!(
        !policy.is_path_allowed(&ws.join("state/costs.jsonl").to_string_lossy()),
        "the run ledger is not writable"
    );
    let reader = crate::tools::FileReadTool::new(policy, false);
    let read = crate::tools::Tool::execute(&reader, serde_json::json!({"path": secret.to_string_lossy()}))
        .await
        .expect("test: tool result");
    assert!(!read.success && !read.output.contains("ORACLE"), "{read:?}");

    // Declared tools are offered, and only those take settings from the template.
    let tools = vec!["web_search_tool".to_string(), "file_write".to_string()];
    let config = executor
        .run_config(&ws, &dummy_request(&denied, &tools, SideEffects::WorkspaceWrite))
        .expect("test: config");
    assert_eq!(
        config.tool_tiering.always_include,
        ["skill_read", "web_search_tool", "file_write"]
    );
    assert!(
        !config
            .tool_tiering
            .always_exclude
            .iter()
            .any(|n| n == "web_search_tool" || n == "file_write")
    );
    assert!(
        config
            .tool_tiering
            .always_exclude
            .iter()
            .any(|n| n == "shell" || n == "skill_execute")
    );
    assert_eq!(config.web_search.max_results, 99);
    assert_eq!(config.autonomy.level, crate::security::AutonomyLevel::Full);
    assert!(config.autonomy.workspace_only, "writes stay inside the task workspace");
}

/// Tiny helper so sections of different types compare as JSON.
mod erased {
    pub trait Ser {
        fn to_json(&self) -> serde_json::Value;
    }
    impl<T: serde::Serialize> Ser for T {
        fn to_json(&self) -> serde_json::Value {
            serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
        }
    }
}

#[test]
fn usage_is_read_from_the_run_ledger_and_the_environment_binds_the_models_that_served() {
    let tmp = TempDir::new().expect("test: tempdir");
    assert!(
        super::super::agent_executor_read_usage_for_test(tmp.path())
            .tokens
            .is_none(),
        "no ledger means unmeasured, not free"
    );
    // Rows are written by the real ledger, so their settlement fingerprints verify.
    let tracker = crate::cost::tracker::CostTracker::new(crate::config::schema::CostConfig::default(), tmp.path())
        .expect("test: tracker");
    let settle = |id: &str, model: &str, tokens: u64, cost: f64| {
        tracker
            .settle_metered(&crate::llm::route_decision::MeteredTokenUsageRecord {
                settlement_id: Some(id.to_string()),
                provider: "p".to_string(),
                model: model.to_string(),
                prompt_tokens: tokens,
                completion_tokens: 0,
                total_tokens: tokens,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                source: crate::llm::route_decision::TokenUsageSource::Reported,
                cost_usd: Some(cost),
            })
            .expect("test: settle");
    };
    settle("s-1", "m-a", 7, 0.000_001);
    settle("s-2", "m-b", 5, 0.000_002_5);
    let usage = super::super::agent_executor_read_usage_for_test(tmp.path());
    assert_eq!((usage.tokens, usage.cost_micros), (Some(12), Some(4)));
    assert_eq!(usage.models, ["p/m-a", "p/m-b"]);

    // A forged zero-cost row, an unsettled row and a row with a wrong fingerprint are ignored.
    let ledger = tmp.path().join("state/costs.jsonl");
    let forged = serde_json::json!({
        "id": "f", "settlement_id": "x", "settlement_sha256": "0".repeat(64), "session_id": "s",
        "usage": {"model": "p/m-a", "input_tokens": 900, "output_tokens": 0, "total_tokens": 900, "cost_usd": 0.0,
                  "timestamp": "2026-10-17T00:00:00Z"}
    });
    let unsettled = serde_json::json!({
        "id": "u", "session_id": "s",
        "usage": {"model": "p/m-c", "input_tokens": 99, "output_tokens": 0, "total_tokens": 99, "cost_usd": 9.0,
                  "timestamp": "2026-10-17T00:00:00Z"}
    });
    let mut text = std::fs::read_to_string(&ledger).expect("test: ledger");
    text.push_str(&format!("{forged}\n{unsettled}\n"));
    std::fs::write(&ledger, text).expect("test: ledger");
    let usage = super::super::agent_executor_read_usage_for_test(tmp.path());
    assert_eq!(
        (usage.tokens, usage.cost_micros),
        (Some(12), Some(4)),
        "forged rows add nothing"
    );
    // A ledger holding only forged rows measures nothing.
    std::fs::write(&ledger, format!("{forged}\n")).expect("test: ledger");
    assert!(
        super::super::agent_executor_read_usage_for_test(tmp.path())
            .tokens
            .is_none()
    );

    let tools: Vec<String> = Vec::new();
    let denied: Vec<std::path::PathBuf> = Vec::new();
    let request = dummy_request(&denied, &tools, SideEffects::None);
    let config = Config::default();
    let one = AgentCaseExecutor::environment_hash_for_test(&config, &request, &["m-a".to_string()]);
    let other = AgentCaseExecutor::environment_hash_for_test(&config, &request, &["m-b".to_string()]);
    assert_ne!(
        one, other,
        "a run served by a fallback model has a different environment"
    );
    assert_eq!(
        one,
        AgentCaseExecutor::environment_hash_for_test(&config, &request, &["m-a".to_string()])
    );
}

#[tokio::test]
async fn a_run_is_refused_when_the_operators_cost_limit_is_already_exceeded() {
    let tmp = TempDir::new().expect("test: tempdir");
    let loopback = Loopback::start().await;
    std::fs::create_dir_all(tmp.path().join("owner-ws")).expect("test: owner workspace");
    let store = SkillLifecycleStore::open(&tmp.path().join("owner-ws"), owner(OWNER)).expect("test: store");
    let a = write_revision(&store, "ANSWER=draft");
    let b = write_revision(&store, "ANSWER=report-v2");
    activate(&store, &skill(), &a, 0, true);

    let mut config = template(tmp.path(), &loopback);
    config.cost.daily_limit_usd = 0.5;
    let operator = tmp.path().join("operator-ws");
    std::fs::create_dir_all(&operator).expect("test: operator ws");
    // The operator already spent past the daily limit through the real ledger.
    let tracker =
        crate::cost::tracker::CostTracker::for_workspace(config.cost.clone(), &operator).expect("test: tracker");
    tracker
        .record_usage(crate::cost::types::TokenUsage {
            model: "earlier".to_string(),
            input_tokens: 1,
            output_tokens: 1,
            total_tokens: 2,
            cost_usd: 5.0,
            timestamp: chrono::Utc::now(),
        })
        .expect("test: spend");
    let suite = suite(
        ["neg-a", "neg-b"]
            .into_iter()
            .map(|id| {
                case(
                    id,
                    &format!("What is two plus {id}?"),
                    Validator::ResponseContains {
                        text: "plain answer".to_string(),
                    },
                    false,
                )
            })
            .collect(),
        2,
    );
    let executor = AgentCaseExecutor::new(config, tmp.path().join("scratch"), 0.0, AccountingOwner::new(operator));
    let receipt = store
        .evaluate(
            &EvaluationRequest {
                suite: &suite,
                split: EvalSplit::Validation,
                baseline: Some(a.0),
                candidate: b.0,
                observed_generation: 1,
            },
            &executor,
            &CancellationToken::new(),
        )
        .await
        .expect("test: evaluate");
    assert!(!receipt.outcome.is_pass(), "{:?}", receipt.outcome);
    assert!(
        loopback.bodies.lock().is_empty(),
        "no provider call was made past the limit"
    );
    let verified = store
        .verify_evaluation(&receipt.record_id, Some(&receipt.receipt_digest))
        .expect("test: verify");
    assert!(verified.cases.iter().all(|c| matches!(
        &c.verdict,
        CaseVerdict::Invalid {
            reason: InvalidReason::ExecutorError,
            ..
        }
    )));
}

#[tokio::test]
async fn the_relative_paths_that_decide_the_ledger_and_the_skill_are_refused_to_the_file_tools() {
    let tmp = TempDir::new().expect("test: tempdir");
    let loopback = Loopback::start().await;
    let executor = AgentCaseExecutor::new(
        template(tmp.path(), &loopback),
        tmp.path().join("scratch"),
        0.0,
        AccountingOwner::new(tmp.path().to_path_buf()),
    );
    let ws = tmp.path().join("run-ws").canonicalize().unwrap_or_else(|_| {
        std::fs::create_dir_all(tmp.path().join("run-ws")).expect("test: ws");
        tmp.path().join("run-ws").canonicalize().expect("test: canonical ws")
    });
    let denied: Vec<std::path::PathBuf> = Vec::new();
    let tools = vec!["file_write".to_string()];
    let config = executor
        .run_config(&ws, &dummy_request(&denied, &tools, SideEffects::WorkspaceWrite))
        .expect("test: config");
    assert_eq!(config.autonomy.level, crate::security::AutonomyLevel::Full);
    let policy = std::sync::Arc::new(crate::security::SecurityPolicy::from_config(&config.autonomy, &ws));
    let writer = crate::tools::FileWriteTool::new(policy);
    for path in [
        "state/costs.jsonl",
        "skills/release-notes/SKILL.md",
        "config.toml",
        "self/skill-lifecycle/x",
        "memory/brain.db",
    ] {
        let result = crate::tools::Tool::execute(&writer, serde_json::json!({"path": path, "content": "FORGED"}))
            .await
            .expect("test: tool result");
        assert!(!result.success, "{path} must be refused: {result:?}");
        assert!(!ws.join(path).exists(), "{path} must not have been written");
    }
    // An ordinary workspace file is still writable under workspace_write.
    let ok = crate::tools::Tool::execute(
        &writer,
        serde_json::json!({"path": "out/report.txt", "content": "fine"}),
    )
    .await
    .expect("test: tool result");
    assert!(ok.success, "{ok:?}");
}

#[tokio::test]
async fn web_fetch_inherits_a_non_empty_domain_allowlist_or_the_run_is_refused() {
    let tmp = TempDir::new().expect("test: tempdir");
    let loopback = Loopback::start().await;
    let mut with_domains = template(tmp.path(), &loopback);
    with_domains.browser.allowed_domains = vec!["docs.example".to_string()];
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("test: ws");
    let denied: Vec<std::path::PathBuf> = Vec::new();
    let tools = vec!["web_fetch".to_string()];
    let executor = AgentCaseExecutor::new(
        with_domains,
        tmp.path().join("s"),
        0.0,
        AccountingOwner::new(ws.clone()),
    );
    let config = executor
        .run_config(&ws, &dummy_request(&denied, &tools, SideEffects::None))
        .expect("test: config");
    assert_eq!(config.browser.allowed_domains, ["docs.example"]);

    let open = AgentCaseExecutor::new(
        template(tmp.path(), &loopback),
        tmp.path().join("s"),
        0.0,
        AccountingOwner::new(ws.clone()),
    );
    let error = open
        .run_config(&ws, &dummy_request(&denied, &tools, SideEffects::None))
        .map(|_| ())
        .expect_err("test: an empty allowlist is refused")
        .to_string();
    assert!(error.contains("allowed_domains"), "{error}");
}
