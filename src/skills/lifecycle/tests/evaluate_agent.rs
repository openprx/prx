#![cfg(test)]
#![allow(clippy::indexing_slicing)]

//! SL06 through the real agent entry (`crate::agent::run`) against a
//! deterministic loopback model on 127.0.0.1. No network, no real credential:
//! the only key in play is a sentinel literal.

use super::{activate, entry, owner};
use crate::config::Config;
use crate::skills::lifecycle::{
    AgentCaseExecutor, Arm, Bundle, CaseVerdict, EvalCase, EvalSplit, EvalSuite, EvaluationOutcome, EvaluationRequest,
    InvalidReason, NewRevision, RevisionId, SkillId, SkillLifecycleStore, SourceKind, SuiteBudget, Validator,
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
    // The decision plane has no model backend here, so the skill reader is offered
    // directly instead of through a discovery hop.
    config.tool_tiering.always_include.push("skill_read".to_string());
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
        validation_attempts: 3,
        holdout_attempts: 1,
        budget: SuiteBudget {
            max_runs: 32,
            max_total_elapsed_ms: 900_000,
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
    let executor = AgentCaseExecutor::new(template(tmp.path(), &loopback), scratch.clone(), 0.0);
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
        vec![case(
            "empty",
            "ZEROBODY please answer.",
            Validator::ResponseExcludes {
                text: "NEVER-PRESENT".to_string(),
            },
            false,
        )],
        1,
    );
    let executor = AgentCaseExecutor::new(template(tmp.path(), &loopback), tmp.path().join("scratch"), 0.0);
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
