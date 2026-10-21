#![cfg(test)]

//! Shared fixtures of the SL10 acceptance scenarios.
//!
//! Everything runs on this machine: the "model" is a loopback HTTP server on
//! 127.0.0.1 that answers from the request alone, the proposer is a scripted
//! `ProposerModel`, and the only key in play is a sentinel literal. Nothing
//! here is a learning claim (invariant 11): the loopback model follows the
//! `RULE <kind> => <token>` lines of whatever skill text reached it, which
//! proves the mechanics of the lifecycle and nothing about a real model.

use crate::config::Config;
use crate::config::skills_lifecycle::{SkillSuiteRegistration, SkillsLifecycleConfig, SkillsLifecycleModeConfig};
use crate::skills::lifecycle::proposer_model::TallyCounters;
use crate::skills::lifecycle::{
    AccountingOwner, AgentCaseExecutor, ConsolidationRequest, EvalCase, EvalSplit, EvalSuite, FailureClass,
    FrozenSuite, KnowledgePattern, ModelBinding, ModelCallError, ModelReply, ObservedCallStatus, ObservedToolCall,
    OwnerScope, PatternScope, ProposerModel, ProviderEnd, RevisionId, SideEffects, SkillExperience, SkillId,
    SkillLifecycleStore, SkillsLifecycleService, SuiteBudget, TerminalKind, TerminalObservation, TerminalObservations,
    Validator, apply_migration, experience_id, plan_migration, settle_task_terminal,
};
use async_trait::async_trait;
use parking_lot::Mutex;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;

/// Chat requests one loopback serves before it starts failing.
const MAX_CHAT_REQUESTS: usize = 600;
pub(super) const MODEL: &str = "accept-loopback-model";
/// The directory name of the skill under test; its id is derived from it.
pub(super) const SKILL_DIR: &str = "release-notes";
/// A declared tool the skill requires. Following the skill means calling it.
pub(super) const REQUIRED_TOOL: &str = "filesystem:native:file_read";

// ---- the loopback model -------------------------------------------------------

/// What the loopback model does with a request.
enum Reply {
    Text(String),
    /// A provider outage: HTTP 500.
    Outage,
}

/// The model of this world. It decides from the request alone:
/// * no `TASK[<kind>]` marker: a plain answer, no tools;
/// * otherwise it reads the skill, then the declared input file, and answers
///   `FINAL:<token>` from the `RULE <kind> => <token>` line of the skill text
///   it was given, or a plain answer when the skill has no such line
///   (`OUTAGE` in the prompt makes the provider fail, `EMPTY-ON-MISSING` an
///   empty reply, `TWICE` a second read of the skill).
fn decide(body: &str) -> Reply {
    let Some(kind) = ["hotfix", "release", "legacy", "audit", "report"]
        .into_iter()
        .find(|kind| body.contains(&format!("TASK[{kind}]")))
    else {
        return Reply::Text("plain answer".to_string());
    };
    let call = |name: &str, arguments: &str| {
        Reply::Text(format!(
            "<tool_call>\n{{\"name\":\"{name}\",\"arguments\":{arguments}}}\n</tool_call>"
        ))
    };
    let skill_reads = body.matches("tool_result name=\\\"skill_read").count();
    if skill_reads == 0 {
        return call("skill_read", "{\"name\":\"release-notes\"}");
    }
    if body.contains("OUTAGE") {
        return Reply::Outage;
    }
    // The skill says to read the inputs first; the model does.
    if !body.contains("tool_result name=\\\"file_read") {
        return call("file_read", "{\"path\":\"inputs.txt\"}");
    }
    // A task that reads the skill again once the inputs are in.
    if body.contains("TWICE") && skill_reads < 2 {
        return call("skill_read", "{\"name\":\"release-notes\"}");
    }
    let marker = format!("RULE {kind} => ");
    let Some(at) = body.find(&marker) else {
        return Reply::Text(if body.contains("EMPTY-ON-MISSING") {
            String::new()
        } else {
            "plain answer".to_string()
        });
    };
    let token: String = body
        .get(at + marker.len()..)
        .unwrap_or_default()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    // A report task leaves its result in the workspace before it answers.
    if kind == "report" && !body.contains("tool_result name=\\\"file_write") {
        return call(
            "file_write",
            &format!("{{\"path\":\"report.txt\",\"content\":\"REPORT:{token}\"}}"),
        );
    }
    Reply::Text(format!("FINAL:{token}"))
}

/// Called with the 1-based number of the chat request, before it is answered.
pub(super) type Hook = Arc<dyn Fn(usize) + Send + Sync>;

pub(super) struct Loopback {
    addr: SocketAddr,
    pub bodies: Arc<Mutex<Vec<String>>>,
    hook: Arc<Mutex<Option<Hook>>>,
    server: JoinHandle<()>,
}

impl Loopback {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test: bind loopback");
        let addr = listener.local_addr().expect("test: address");
        let bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let hook: Arc<Mutex<Option<Hook>>> = Arc::new(Mutex::new(None));
        let (sink, hook_slot) = (Arc::clone(&bodies), Arc::clone(&hook));
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let (sink, hook_slot) = (Arc::clone(&sink), Arc::clone(&hook_slot));
                tokio::spawn(async move {
                    let mut buffer: Vec<u8> = Vec::new();
                    let mut chunk = [0_u8; 8192];
                    let head_end = loop {
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buffer.extend_from_slice(chunk.get(..n).unwrap_or_default()),
                        }
                        if let Some(p) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                            break p + 4;
                        }
                    };
                    let head = String::from_utf8_lossy(buffer.get(..head_end).unwrap_or_default()).to_string();
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
                            Ok(n) => buffer.extend_from_slice(chunk.get(..n).unwrap_or_default()),
                        }
                    }
                    let body = String::from_utf8_lossy(buffer.get(head_end..).unwrap_or_default()).to_string();
                    let response = if path.contains("chat/completions") {
                        let number = {
                            let mut bodies = sink.lock();
                            bodies.push(body.clone());
                            bodies.len()
                        };
                        let hook = hook_slot.lock().clone();
                        if let Some(hook) = hook {
                            let _ = tokio::task::spawn_blocking(move || hook(number)).await;
                        }
                        // A runaway tool loop ends as an outage, never as a hang.
                        let reply = if number > MAX_CHAT_REQUESTS {
                            Reply::Outage
                        } else {
                            decide(&body)
                        };
                        match reply {
                            Reply::Text(reply) => {
                                let payload = serde_json::json!({
                                    "id": "chatcmpl-accept", "object": "chat.completion", "created": 1, "model": MODEL,
                                    "choices": [{"index": 0, "message": {"role": "assistant", "content": reply}, "finish_reason": "stop"}],
                                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                                })
                                .to_string();
                                format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                                    payload.len()
                                )
                            }
                            Reply::Outage => {
                                let payload = "{\"error\":\"loopback outage\"}";
                                format!(
                                    "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                                    payload.len()
                                )
                            }
                        }
                    } else {
                        let payload = serde_json::json!({"object": "list", "data": [{"id": MODEL, "object": "model", "created": 1, "owned_by": "accept"}]}).to_string();
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                            payload.len()
                        )
                    };
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.flush().await;
                });
            }
        });
        Self {
            addr,
            bodies,
            hook,
            server,
        }
    }

    pub fn provider(&self) -> String {
        format!("custom:http://{}/v1", self.addr)
    }

    /// Installs (or clears) the hook run before each chat request is answered.
    pub fn set_hook(&self, hook: Option<Hook>) {
        *self.hook.lock() = hook;
    }

    /// Number of chat requests received so far.
    pub fn requests(&self) -> usize {
        self.bodies.lock().len()
    }

    /// Holds the chat request numbered `number` until one more experience of
    /// the owner store holds a selection receipt than it does now, so that the
    /// receipt of the coming run (written off the turn path) is on record
    /// before the turn can end. Without it the write could land after the
    /// terminal and count as late evidence. Returns the hook; the caller
    /// installs it (alone, or inside a larger hook).
    pub fn selection_gate(number: usize, store: &Arc<SkillLifecycleStore>) -> Hook {
        let before = selection_count(store);
        let store = Arc::clone(store);
        Arc::new(move |at| {
            if at == number {
                wait_for_selection_beyond(&store, before);
            }
        })
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// A configuration that reaches only the loopback model.
pub(super) fn template(root: &Path, loopback: &Loopback) -> Config {
    let mut config = Config::default();
    config.workspace_dir = root.join("workspace");
    config.config_path = root.join("config.toml");
    config.default_provider = Some(loopback.provider());
    config.default_model = Some(MODEL.to_string());
    config.api_key = Some("ACCEPT-LOOPBACK-SENTINEL".to_string());
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

// ---- skill texts --------------------------------------------------------------

/// The `n`th item of a list the test knows to be long enough.
pub(super) fn nth<T>(items: &[T], n: usize) -> &T {
    items.get(n).expect("test: the list is long enough")
}

/// A string field of a JSON value.
pub(super) fn text_of<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(serde_json::Value::as_str)
}

/// A skill text: frontmatter that declares the required tool, then the rules.
pub(super) fn skill_text(rules: &[(&str, &str)]) -> String {
    use std::fmt::Write as _;
    let mut text = format!(
        "---\nname: release-notes\ndescription: Writes release notes and hotfix notes for a build\nrequired-tools: [{REQUIRED_TOOL}]\n---\n\n# Release notes\n\nRULES-BEGIN\n"
    );
    for n in 1..=18 {
        let _ = writeln!(text, "Step {n}: list the changes of part {n} of the build in order.");
    }
    for (kind, token) in rules {
        let _ = writeln!(text, "RULE {kind} => {token}");
    }
    text.push_str("RULES-END\n");
    text
}

// ---- the world ------------------------------------------------------------------

pub(super) fn skill() -> SkillId {
    SkillId::from_legacy_dir(SKILL_DIR).expect("test: skill id")
}

/// The owner id the real agent entry derives for a workspace.
pub(super) fn owner_id_for(workspace: &Path) -> String {
    crate::runtime::envelope::RuntimeEnvelope::agent(workspace.to_string_lossy(), "accept-probe").resolved_owner_id()
}

pub(super) struct World {
    pub tmp: TempDir,
    /// The owner workspace: production runs, the lifecycle store and the
    /// legacy `skills/` directory all live here.
    pub ws: PathBuf,
    /// Where evaluation spend is charged (the operator's ledger).
    pub operator_ws: PathBuf,
    pub scratch: PathBuf,
    pub owner: OwnerScope,
    pub store: Arc<SkillLifecycleStore>,
    pub loopback: Loopback,
}

impl World {
    pub async fn new() -> Self {
        let tmp = TempDir::new().expect("test: tempdir");
        let root = tmp.path().canonicalize().expect("test: canonical root");
        let ws = root.join("owner-ws");
        let operator_ws = root.join("operator-ws");
        let scratch = root.join("scratch");
        for dir in [&ws, &operator_ws] {
            std::fs::create_dir_all(dir).expect("test: workspace");
        }
        let owner = OwnerScope::new(&owner_id_for(&ws)).expect("test: owner scope");
        let store = Arc::new(SkillLifecycleStore::open(&ws, owner.clone()).expect("test: store"));
        let loopback = Loopback::start().await;
        Self {
            tmp,
            ws,
            operator_ws,
            scratch,
            owner,
            store,
            loopback,
        }
    }

    pub fn root(&self) -> PathBuf {
        self.tmp.path().canonicalize().expect("test: canonical root")
    }

    /// Writes the legacy skill directory and imports it through the migration
    /// path (the way an installed skill becomes managed). Returns the first
    /// revision.
    pub fn onboard(&self, text: &str) -> RevisionId {
        let dir = self.ws.join("skills").join(SKILL_DIR);
        std::fs::create_dir_all(&dir).expect("test: skill dir");
        std::fs::write(dir.join("SKILL.md"), text).expect("test: skill file");
        let plan = plan_migration(&self.ws, Some(&self.store)).expect("test: plan");
        let report = apply_migration(&self.store, &self.ws, &plan);
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        let binding = self
            .store
            .active_binding(&skill())
            .expect("test: binding readable")
            .expect("test: skill is bound");
        assert_eq!(binding.generation, 1);
        binding.revision_id
    }

    /// The configuration of a production run in the owner workspace, with the
    /// lifecycle section the CLI and the tool read.
    pub fn production_config(&self, mode: SkillsLifecycleModeConfig, suites: &[(&SkillId, &FrozenSuite)]) -> Config {
        let mut config = template(&self.root(), &self.loopback);
        config.workspace_dir.clone_from(&self.ws);
        config.config_path = self.ws.join("config.toml");
        config.tool_tiering.always_include = vec!["skill_read".to_string()];
        config.skills_lifecycle = SkillsLifecycleConfig {
            enabled: true,
            mode,
            owner_scope: Some(self.owner.as_str().to_string()),
            suites: suites
                .iter()
                .map(|(skill_id, suite)| SkillSuiteRegistration {
                    skill_id: skill_id.as_str().to_string(),
                    suite_digest: suite.digest().to_string(),
                })
                .collect(),
            ..SkillsLifecycleConfig::default()
        };
        config
    }

    /// The service the CLI and the agent tool use for this configuration.
    pub fn service(&self, config: &Config) -> Arc<SkillsLifecycleService> {
        crate::skills::lifecycle_runtime::open_service(&config.workspace_dir, &config.skills_lifecycle)
            .expect("test: service builds")
            .expect("test: lifecycle enabled")
    }

    /// One task through the real single-shot agent entry (`prx agent -m`).
    pub async fn run_task(&self, config: &Config, prompt: &str) -> anyhow::Result<String> {
        let run = crate::agent::run(
            config.clone(),
            Some(prompt.to_string()),
            None,
            None,
            0.0,
            tokio_util::sync::CancellationToken::new(),
        );
        tokio::time::timeout(std::time::Duration::from_secs(60), run)
            .await
            .map_err(|_| anyhow::anyhow!("the agent run did not finish within 60s"))?
    }

    /// [`Self::run_task`] for a task that selects a catalog skill, with the
    /// selection receipt of the run on record before the turn can end. The
    /// receipt is written off the turn path; left to race with the hydration
    /// and the terminal of the same task for the experience's lock, a slow
    /// disk could make one of their bounded lock waits expire.
    pub async fn run_task_selected(&self, config: &Config, prompt: &str) -> anyhow::Result<String> {
        self.loopback.set_hook(Some(Loopback::selection_gate(
            self.loopback.requests() + 1,
            &self.store,
        )));
        let result = self.run_task(config, prompt).await;
        self.loopback.set_hook(None);
        result
    }

    /// The executor evaluation runs use: the real agent entry in fresh
    /// workspaces, charged to the operator ledger.
    pub fn executor(&self) -> AgentCaseExecutor {
        AgentCaseExecutor::new(
            template(&self.root(), &self.loopback),
            self.scratch.clone(),
            0.0,
            AccountingOwner::new(self.operator_ws.clone()),
        )
    }

    /// Every experience record in the owner store, oldest first.
    pub fn experiences(&self) -> Vec<SkillExperience> {
        let dir = self.store.root().join("experiences");
        let mut ids: Vec<String> = std::fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|e| {
                        e.file_name()
                            .to_str()
                            .and_then(|n| n.strip_suffix(".json"))
                            .filter(|id| id.starts_with("exp-"))
                            .map(str::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default();
        ids.sort();
        let mut records: Vec<SkillExperience> = ids
            .iter()
            .filter_map(|id| self.store.read_experience_by_id(id).expect("test: experience readable"))
            .collect();
        records.sort_by_key(|record| record.created_at);
        records
    }

    /// The experiences of `expected` root tasks that ran through the agent
    /// entry without reading any managed skill, oldest first, once their
    /// terminal is joined. The terminal creates these records itself, so the
    /// run has finished by the time one exists.
    pub async fn uncovered_runs(&self, expected: usize) -> Vec<SkillExperience> {
        for _ in 0..200 {
            let runs: Vec<SkillExperience> = self
                .experiences()
                .into_iter()
                .filter(|record| record.hydration_receipts.is_empty() && record.selection_receipts.is_empty())
                .collect();
            if runs.len() == expected && runs.iter().all(|record| record.terminal.is_some()) {
                return runs;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let seen: Vec<String> = self
            .experiences()
            .iter()
            .map(|r| {
                format!(
                    "task={} hyd={} sel={} term={:?}",
                    r.task_id,
                    r.hydration_receipts.len(),
                    r.selection_receipts.len(),
                    r.terminal.as_ref().map(|t| t.outcome)
                )
            })
            .collect();
        panic!("test: {expected} uncovered runs did not appear: {seen:#?}");
    }

    /// The experiences of `expected` real agent runs (they carry a run id and
    /// a hydration), oldest first, once their terminal is joined:
    /// the terminal join is awaited by the run itself. The selection receipt is
    /// written off the turn path and depends on retrieval, so it is not waited for.
    pub async fn production_runs(&self, expected: usize) -> Vec<SkillExperience> {
        for _ in 0..200 {
            let runs: Vec<SkillExperience> = self
                .experiences()
                .into_iter()
                .filter(|record| record.correlation.run_id.is_some() && !record.hydration_receipts.is_empty())
                .collect();
            if runs.len() == expected && runs.iter().all(|record| record.terminal.is_some()) {
                return runs;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let seen: Vec<String> = self
            .experiences()
            .iter()
            .map(|r| {
                format!(
                    "task={} run={:?} sel={} hyd={} term={:?}",
                    r.task_id,
                    r.correlation.run_id,
                    r.selection_receipts.len(),
                    r.hydration_receipts.len(),
                    r.terminal.as_ref().map(|t| t.outcome)
                )
            })
            .collect();
        panic!("test: {expected} settled production runs did not appear: {seen:#?}");
    }
}

/// How many experiences of `store` hold a selection receipt.
pub(super) fn selection_count(store: &SkillLifecycleStore) -> usize {
    let dir = store.root().join("experiences");
    std::fs::read_dir(&dir).map_or(0, |entries| {
        entries
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.strip_suffix(".json"))
                    .filter(|id| id.starts_with("exp-"))
                    .is_some_and(|id| {
                        store
                            .read_experience_by_id(id)
                            .ok()
                            .flatten()
                            .is_some_and(|record| !record.selection_receipts.is_empty())
                    })
            })
            .count()
    })
}

/// Waits (bounded) until more than `before` experiences hold a selection receipt.
pub(super) fn wait_for_selection_beyond(store: &SkillLifecycleStore, before: usize) -> bool {
    for _ in 0..100 {
        if selection_count(store) > before {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    false
}

// ---- suites -----------------------------------------------------------------------

pub(super) fn final_of(token: &str) -> Validator {
    Validator::ResponseContains {
        text: format!("FINAL:{token}"),
    }
}

pub(super) fn case(id: &str, split: EvalSplit, family: &str, prompt: &str, validator: Validator) -> EvalCase {
    EvalCase {
        case_id: id.to_string(),
        split,
        task_family: family.to_string(),
        prompt: prompt.to_string(),
        fixtures: Vec::new(),
        validators: vec![validator],
        expect_skill_read: family != "unrelated",
        critical: family == "hotfix" || family == "unrelated",
        max_elapsed_ms: 120_000,
    }
}

/// The frozen suite of the skill under test. Release cases want `release_token`;
/// the critical hotfix case wants `hotfix_token`; the negative trigger case
/// must not load the skill; the holdout alone holds a `legacy` case. Validation
/// and holdout prompts are disjoint.
pub(super) fn notes_suite(owner: &OwnerScope, release_token: &str, hotfix_token: &str) -> FrozenSuite {
    let mut cases = Vec::new();
    for (split, label, builds) in [
        (EvalSplit::Validation, "val", [11_u32, 12, 13]),
        (EvalSplit::Holdout, "hold", [21, 22, 23]),
    ] {
        for (n, build) in builds.iter().enumerate() {
            cases.push(case(
                &format!("{label}-release-{n}"),
                split,
                "release",
                &format!("TASK[release] Write the release notes for build {build}."),
                final_of(release_token),
            ));
        }
        cases.push(case(
            &format!("{label}-hotfix"),
            split,
            "hotfix",
            &format!("TASK[hotfix] Write the hotfix note for patch {}.", builds[0]),
            final_of(hotfix_token),
        ));
        cases.push(case(
            &format!("{label}-unrelated"),
            split,
            "unrelated",
            &format!("What is {} plus {}?", builds[0], builds[1]),
            Validator::ResponseContains {
                text: "plain answer".to_string(),
            },
        ));
    }
    cases.push(case(
        "hold-legacy",
        EvalSplit::Holdout,
        "legacy",
        "TASK[legacy] Write the note for the legacy branch 7.",
        final_of("legacy-v1"),
    ));
    EvalSuite {
        schema_version: 1,
        suite_id: "release-notes-acceptance".to_string(),
        owner_scope: owner.clone(),
        skill_id: skill(),
        trials: 1,
        min_cases: 4,
        allowed_regressions: 0,
        deterministic: true,
        min_gain_cases: 1,
        validation_attempts: 3,
        holdout_attempts: 2,
        required_tools: vec!["file_read".to_string()],
        side_effects: SideEffects::None,
        budget: SuiteBudget {
            max_runs: 64,
            max_total_elapsed_ms: 900_000,
            max_total_tokens: 1_000_000,
            max_total_cost_micros: 1_000_000,
        },
        cases,
    }
    .freeze()
    .expect("test: freeze")
}

// ---- the scripted proposer ----------------------------------------------------------

/// A deterministic `ProposerModel`: replies with fixed patches in order.
pub(super) struct ScriptedProposer {
    binding: ModelBinding,
    replies: Mutex<std::collections::VecDeque<String>>,
    calls: AtomicUsize,
}

impl ScriptedProposer {
    pub fn new(replies: Vec<String>) -> Self {
        Self {
            binding: ModelBinding {
                provider_label: "scripted-fixture".to_string(),
                model: "scripted-proposer".to_string(),
                temperature_milli: 0,
            },
            replies: Mutex::new(replies.into()),
            calls: AtomicUsize::new(0),
        }
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ProposerModel for ScriptedProposer {
    fn binding(&self) -> &ModelBinding {
        &self.binding
    }

    async fn complete(
        &self,
        _system: &str,
        _user: &str,
        _tally: Arc<TallyCounters>,
    ) -> Result<ModelReply, ModelCallError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let next = self.replies.lock().pop_front();
        next.map_or_else(
            || Err(ModelCallError::failed("the scripted proposer has no reply left")),
            |text| {
                Ok(ModelReply {
                    text,
                    usage: crate::skills::lifecycle::ReportedUsage::default(),
                })
            },
        )
    }
}

/// A proposer patch that replaces `old` with `new` in `SKILL.md`.
pub(super) fn replace_patch(summary: &str, old: &str, new: &str) -> String {
    multi_patch(summary, &[(old, new)])
}

/// A proposer patch of several replacements in `SKILL.md`, applied in order.
pub(super) fn multi_patch(summary: &str, edits: &[(&str, &str)]) -> String {
    let edits: Vec<serde_json::Value> = edits
        .iter()
        .map(|(old, new)| serde_json::json!({"file": "SKILL.md", "op": "replace", "old": old, "new": new}))
        .collect();
    serde_json::json!({"summary": summary, "edits": edits}).to_string()
}

/// The same suite without its critical hotfix cases: what a "filtered" or
/// renamed-away suite would look like.
pub(super) fn without_hotfix(suite: &FrozenSuite) -> FrozenSuite {
    let mut altered = suite.suite().clone();
    altered.cases.retain(|case| case.task_family != "hotfix");
    altered.freeze().expect("test: freeze altered suite")
}

// ---- experiences ----------------------------------------------------------------------

/// Observations of a finished run that served its provider, saw the whole
/// trace and called the skill's declared tool.
pub(super) fn complete_observations() -> TerminalObservations {
    TerminalObservations {
        tool_calls: vec![ObservedToolCall {
            capability_id: REQUIRED_TOOL.to_string(),
            status: ObservedCallStatus::Succeeded,
        }],
        complete: true,
        provider: ProviderEnd::Served,
        catalog_ids: Some(std::iter::once(REQUIRED_TOOL.to_string()).collect()),
        ..TerminalObservations::default()
    }
}

/// Settles a task that read the managed skill: hydration through the real
/// `skill_read` tool, then the terminal through the production call-in that
/// the agent's finalization uses.
pub(super) async fn read_skill_in_task(world: &World, config: &Config, task: &str) -> serde_json::Value {
    use crate::tools::Tool;
    let tool = crate::tools::SkillReadTool::new(world.ws.clone(), config.clone());
    let args = serde_json::json!({
        "name": SKILL_DIR,
        "_zc_scope_trusted": true,
        "_zc_scope": {"owner_id": world.owner.as_str(), "task_id": task}
    });
    let result = tool.execute(args).await.expect("test: skill_read executes");
    serde_json::from_str(&result.output).expect("test: skill_read json")
}

pub(super) async fn settle(world: &World, task: &str, kind: TerminalKind, observations: Option<TerminalObservations>) {
    settle_task_terminal(
        &world.ws,
        Some(world.owner.as_str().to_string()),
        Some(task.to_string()),
        &format!("term-{task}"),
        TerminalObservation {
            kind,
            awaiting_approval: false,
            verified: true,
            recovered: false,
            root: true,
        },
        observations,
    )
    .await;
}

pub(super) fn consolidate(world: &World, statement: &str, evidence: &[String]) -> KnowledgePattern {
    world
        .store
        .consolidate_pattern(&ConsolidationRequest {
            task_family: "release".to_string(),
            failure_class: FailureClass::MissingGuidance,
            skill_id: Some(skill()),
            scope: PatternScope {
                environment: "local".to_string(),
                capabilities: vec!["file_read".to_string()],
            },
            statement: statement.to_string(),
            applicability: vec!["release notes tasks".to_string()],
            exceptions: Vec::new(),
            evidence: evidence.to_vec(),
            counterevidence: Vec::new(),
        })
        .expect("test: consolidate")
        .pattern
}

/// A pattern of repeated work that no skill covered (the zero-baseline class).
pub(super) fn consolidate_uncovered(world: &World, statement: &str, evidence: &[String]) -> KnowledgePattern {
    try_consolidate_uncovered(world, statement, evidence).expect("test: consolidate uncovered work")
}

pub(super) fn try_consolidate_uncovered(
    world: &World,
    statement: &str,
    evidence: &[String],
) -> Result<KnowledgePattern, crate::skills::lifecycle::LifecycleError> {
    world
        .store
        .consolidate_pattern(&ConsolidationRequest {
            task_family: "release".to_string(),
            failure_class: FailureClass::MissingSkill,
            skill_id: None,
            scope: PatternScope {
                environment: "local".to_string(),
                capabilities: vec![REQUIRED_TOOL.to_string()],
            },
            statement: statement.to_string(),
            applicability: vec!["release notes tasks".to_string()],
            exceptions: Vec::new(),
            evidence: evidence.to_vec(),
            counterevidence: Vec::new(),
        })
        .map(|outcome| outcome.pattern)
}

/// A proposer patch for a new skill: text appended to its skeleton.
pub(super) fn append_patch(summary: &str, text: &str) -> String {
    serde_json::json!({
        "summary": summary,
        "edits": [{"file": "SKILL.md", "op": "append", "new": text}]
    })
    .to_string()
}

pub(super) fn exp_id(world: &World, task: &str) -> String {
    experience_id(&world.owner, task)
}
