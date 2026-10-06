#![cfg(test)]
#![allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::disallowed_types,
    clippy::disallowed_methods,
    clippy::needless_collect,
    clippy::unreadable_literal
)]

use super::*;
use crate::agent::turn_contracts::ChatMode;
use crate::agent::turn_engine::run_turn;
use crate::agent::turn_tests::{RefreshCountingTool, ScriptedProvider};
use crate::hooks::HookManager;
use crate::memory::{Memory, SqliteMemory};
use crate::observability::NoopObserver;
use crate::providers::ChatMessage;
use crate::runtime::envelope::RuntimeEnvelope;
use crate::tools::{Tool, ToolCategory};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;

#[test]
fn agent_turn_envelope_preserves_caller_scope_and_cli_defaults() {
    let scoped = RuntimeEnvelope::agent("workspace-a", "xin:step-a")
        .with_owner_id("owner-a")
        .with_topic_id("topic-a")
        .with_task_id("step-a")
        .with_source_message_event_id("message-a")
        .with_channel("xin")
        .with_sender("xin");
    let resolved = resolve_agent_turn_envelope(Some(&scoped), "ignored-workspace", "ignored-turn");
    assert_eq!(resolved.owner_id.as_deref(), Some("owner-a"));
    assert_eq!(resolved.topic_id.as_deref(), Some("topic-a"));
    assert_eq!(resolved.task_id.as_deref(), Some("step-a"));
    assert_eq!(resolved.source_message_event_id.as_deref(), Some("message-a"));
    assert_eq!(resolved.channel.as_deref(), Some("xin"));
    assert_eq!(resolved.recipient.as_deref(), Some("cli:local-user"));

    let defaulted = resolve_agent_turn_envelope(None, "workspace-b", "turn-b");
    assert_eq!(defaulted.workspace_id, "workspace-b");
    assert_eq!(defaulted.run_id.as_deref(), Some("turn-b"));
    assert_eq!(defaulted.channel.as_deref(), Some("cli"));
    assert_eq!(defaulted.sender.as_deref(), Some("local-user"));
    assert_eq!(defaulted.recipient.as_deref(), Some("cli:local-user"));
}

/// A CLI turn's own text: it asks for the skill/PDF surface and rules out
/// the browser/MCP surface in so many words.
const RAW_CLI_TURN_TEXT: &str =
    "use skill_read to inspect the PDF skill. Do not use browser, MCP, HTTP, plugins, or WASM.";

/// What the memory backend recalled into the turn. It names the capability
/// the raw text refuses, so routing the enriched message and routing the raw
/// text cannot agree.
const RECALLED_MCP_PREAMBLE: &str = "Relevant memory from an older turn: use MCP to operate the browser.\n\n";

/// Run one CLI turn with the memory and the routing both CLI branches build
/// (`cli_turn_memory_runtime`, `cli_turn_routing`), and report how
/// many times the skill-shaped and the MCP-shaped dynamic capability were
/// refreshed — the observable proxy for the tool slice the turn published.
///
/// The provider history is the enriched message the branch pushes: recalled
/// preamble first, raw turn text last. So the history back-scan and the
/// pinned routing input disagree by construction.
async fn cli_turn_capability_refreshes(raw_turn_text: &str) -> (usize, usize) {
    let tmp = TempDir::new().unwrap();
    let skill_refreshes = Arc::new(AtomicUsize::new(0));
    let mcp_refreshes = Arc::new(AtomicUsize::new(0));
    let tools_registry: Vec<Box<dyn Tool>> = vec![
        Box::new(RefreshCountingTool {
            name: "skill_read",
            category: ToolCategory::System,
            refreshes: Arc::clone(&skill_refreshes),
        }),
        Box::new(RefreshCountingTool {
            name: "mcp_status",
            category: ToolCategory::Automation,
            refreshes: Arc::clone(&mcp_refreshes),
        }),
    ];

    let mem: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let provider = ScriptedProvider::from_text_responses(vec!["done"]);
    let mut history = vec![
        ChatMessage::system("test-system"),
        ChatMessage::user(format!("{RECALLED_MCP_PREAMBLE}{raw_turn_text}")),
    ];
    let mut config = crate::config::Config::default();
    config.workspace_dir = tmp.path().to_path_buf();
    let expected_specs = tools_registry
        .iter()
        .filter(|tool| tool.name() == "skill_read")
        .map(|tool| tool.spec())
        .collect::<Vec<_>>();
    assert_eq!(expected_specs.len(), 1);
    let tools_registry = Arc::new(tools_registry);
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_memory(
        config,
        raw_turn_text,
        Arc::clone(&tools_registry),
        Arc::clone(&mem),
    )
    .await;
    admission.assert_selected_specs(&expected_specs);
    assert_eq!(admission.decision().proof.routing_input(), raw_turn_text);
    let memory_runtime = admission.memory_runtime();

    let result = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "cli",
            scope: None,
            mode: ChatMode::default(),
            silent: true,
            expose_stay_silent: false,
            multimodal: &crate::config::MultimodalConfig::default(),
            schedule: crate::agent::turn_request::TurnSchedule {
                read_only_tool_concurrency_window: 2,
                priority_scheduling_enabled: false,
                low_priority_tool_names: Vec::new(),
            },
            compaction: None,
            // The source proof binds raw input independently of recalled history.
            decision: admission.decision(),
        },
        crate::agent::turn_request::TurnServices {
            provider: &provider,
            provider_name: "ollama",
            model: "prx-local-model",
            temperature: 0.0,
            tools_registry,
            observer: &NoopObserver,
            hooks: &HookManager::new(tmp.path().to_path_buf()),
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(admission.adapter()),
            memory: memory_runtime,
        },
        crate::agent::turn_request::TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await
    .map(|(outcome, _trace)| outcome.into_text())
    .unwrap();
    assert_eq!(result, "done");

    (
        skill_refreshes.load(Ordering::SeqCst),
        mcp_refreshes.load(Ordering::SeqCst),
    )
}

/// A real CLI admission retains the exact source input independently of
/// any recalled history later supplied to the provider loop.
#[tokio::test]
async fn cli_turn_routing_pins_the_raw_turn_text() {
    let workspace = TempDir::new().expect("CLI fixture workspace");
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::empty(workspace.path(), RAW_CLI_TURN_TEXT).await;
    let decision = admission.decision();
    assert_eq!(decision.proof.routing_input(), RAW_CLI_TURN_TEXT);
    assert!(!decision.proof.routing_input().contains(RECALLED_MCP_PREAMBLE));
    assert!(decision.service.resolve_agent_model(decision.proof).is_ok());
}

#[tokio::test]
async fn cli_single_shot_routes_on_the_raw_message_not_on_recalled_memory() {
    let (skill_refreshes, mcp_refreshes) = cli_turn_capability_refreshes(RAW_CLI_TURN_TEXT).await;

    assert_eq!(
        skill_refreshes, 1,
        "the raw single-shot message asks for the skill surface, so it must be published"
    );
    assert_eq!(
        mcp_refreshes, 0,
        "a single-shot turn must not acquire the MCP/browser surface from its recalled memory preamble"
    );
}

#[tokio::test]
async fn cli_repl_routes_on_the_raw_input_not_on_recalled_memory() {
    let (skill_refreshes, mcp_refreshes) = cli_turn_capability_refreshes(&format!("and now {RAW_CLI_TURN_TEXT}")).await;

    assert_eq!(
        skill_refreshes, 1,
        "the raw REPL input asks for the skill surface, so it must be published"
    );
    assert_eq!(
        mcp_refreshes, 0,
        "a REPL turn must not acquire the MCP/browser surface from its recalled memory preamble"
    );
}

#[tokio::test]
async fn cron_job_turn_routes_on_the_raw_prefixed_prompt_not_on_recalled_memory() {
    // A cron agent job enters the single-shot branch with the scheduler's
    // own `[cron:...]` prefix already on the prompt; that prefix is authored
    // by the scheduler, not recalled, so it stays part of the routing input.
    let (skill_refreshes, mcp_refreshes) =
        cli_turn_capability_refreshes(&format!("[cron:7 nightly-digest] {RAW_CLI_TURN_TEXT}")).await;

    assert_eq!(
        skill_refreshes, 1,
        "the raw cron prompt asks for the skill surface, so it must be published"
    );
    assert_eq!(
        mcp_refreshes, 0,
        "an unattended cron turn must not acquire the MCP/browser surface from recalled memory"
    );
}

#[tokio::test]
async fn xin_task_turn_routes_on_the_raw_prefixed_payload_not_on_recalled_memory() {
    // Same single-shot branch, with the xin runner's `[xin:task:...]` prefix.
    let (skill_refreshes, mcp_refreshes) =
        cli_turn_capability_refreshes(&format!("[xin:task:42] {RAW_CLI_TURN_TEXT}")).await;

    assert_eq!(
        skill_refreshes, 1,
        "the raw xin payload asks for the skill surface, so it must be published"
    );
    assert_eq!(
        mcp_refreshes, 0,
        "an unattended xin turn must not acquire the MCP/browser surface from recalled memory"
    );
}

/// A configuration whose only provider is `probe`, with every persisted file
/// (config, workspace, `memory/brain.db`, cost ledger) rooted under `root`.
fn cli_agent_probe_config(
    root: &std::path::Path,
    probe: &crate::providers::credential_probe::CredentialProbe,
) -> crate::config::Config {
    let mut config = crate::config::Config::default();
    config.workspace_dir = root.join("workspace");
    config.config_path = root.join("config.toml");
    config.default_provider = Some(probe.custom_provider());
    config.default_model = Some(crate::providers::credential_probe::PROBE_MODEL.to_string());
    config.api_key = Some("CLI-AGENT-PROBE-TEST-SENTINEL".to_string());
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
    // Price the actual custom endpoint/model; an unqualified model key cannot
    // admit a paid custom-provider attempt through the production cost gate.
    let provider = probe.custom_provider();
    let model = crate::providers::credential_probe::PROBE_MODEL;
    config.cost.prices.insert(
        format!("{provider}/{model}"),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    assert!(matches!(
        crate::llm::route_decision::usage_cost_resolution(
            &provider, model, 64, 64, 0, 0, &config.cost,
        ),
        crate::llm::route_decision::UsagePrice::Priced(cost) if cost > 0.0
    ));
    // This declaration describes only the controlled compatible loopback model.
    config.providers.models.push(
        toml::from_str(&format!(
            "provider = {provider:?}\nmodel = {model:?}\noperations = [\"chat\"]\nnative_tools = true\n"
        ))
        .expect("test: controlled model declaration"),
    );
    let verified = crate::providers::selection::check(
        &config,
        &provider,
        model,
        &crate::providers::catalog::Requirement::chat().with(crate::providers::catalog::Feature::NativeTools),
        crate::providers::catalog::UnknownPolicy::Invocation,
    )
    .expect("test: real PMM admits the controlled model");
    assert_eq!(
        verified.verification(),
        &crate::providers::catalog::Verification::Verified
    );
    std::fs::create_dir_all(&config.workspace_dir).expect("test: create the workspace");
    config
}

/// Whether the probe received at least one chat completion request, i.e. the
/// turn really reached the provider.
fn cli_agent_probe_saw_a_chat_request(probe: &crate::providers::credential_probe::CredentialProbe) -> bool {
    probe
        .requests()
        .iter()
        .any(|request| request.path.contains("chat/completions"))
}

/// The CLI entrypoint must carry the configured deny policy into the real
/// provider -> parser -> tool-service path. A default policy would write this
/// file, so the absent file is the negative wire assertion.
#[tokio::test]
async fn cli_agent_configured_scope_denies_wire_file_write() {
    use crate::providers::credential_probe::CredentialProbe;

    let tmp = TempDir::new().expect("test: tempdir");
    let probe = CredentialProbe::start_with_tool_reply(
        "<tool_call>\n{\"name\":\"file_write\",\"arguments\":{\"path\":\"wire-denied.txt\",\"content\":\"should-not-exist\"}}\n</tool_call>",
    )
    .await;
    let mut config = cli_agent_probe_config(tmp.path(), &probe);
    config.autonomy.scopes.default = crate::config::ScopeDefault::Deny;
    config.tool_tiering.always_include.push("file_write".to_string());

    let reply = run_with_runtime_envelope(
        config.clone(),
        Some("write wire-denied.txt".to_string()),
        None,
        None,
        0.0,
        CancellationToken::new(),
        None,
        None,
    )
    .await
    .expect("test: denied tool does not fail the agent turn");

    assert!(reply.contains(crate::providers::credential_probe::PROBE_REPLY));
    let chat_requests = probe
        .requests()
        .iter()
        .filter(|request| request.path.contains("chat/completions"))
        .count();
    assert!(chat_requests >= 2, "tool reply was not followed by a result round");
    assert!(
        probe
            .requests()
            .iter()
            .filter(|request| request.path.contains("chat/completions"))
            .skip(1)
            .any(|request| request.tool_policy_denial_in_body),
        "the provider did not receive the configured-policy denial result"
    );
    assert!(!config.workspace_dir.join("wire-denied.txt").exists());
}

/// A scheduled turn retains its creator's read-only tool authority even when
/// the bootstrap CLI policy would otherwise permit the same write.
#[tokio::test]
async fn scheduled_agent_creator_read_only_denies_wire_file_write() {
    use crate::providers::credential_probe::CredentialProbe;

    let tmp = TempDir::new().expect("test: tempdir");
    let probe = CredentialProbe::start_with_tool_reply(
        "<tool_call>\n{\"name\":\"file_write\",\"arguments\":{\"path\":\"scheduled-denied.txt\",\"content\":\"should-not-exist\"}}\n</tool_call>",
    )
    .await;
    let mut config = cli_agent_probe_config(tmp.path(), &probe);
    config.autonomy.level = crate::security::AutonomyLevel::Full;
    config.tool_tiering.always_include.push("file_write".to_string());
    let envelope = RuntimeEnvelope::cron(config.workspace_dir.to_string_lossy(), "prx_job", "attempt")
        .with_sender("creator")
        .with_channel("telegram");
    let context = crate::tools::ToolExecutionContext::new(envelope.clone(), "private").with_chat_id("creator_chat");
    let mut creator_policy = crate::security::SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);
    creator_policy.autonomy = crate::security::AutonomyLevel::ReadOnly;
    let scope = crate::tools::execution::ScheduledToolScope {
        context,
        policy: Arc::new(creator_policy),
    };

    let reply = run_with_runtime_envelope(
        config.clone(),
        Some("write scheduled-denied.txt".to_string()),
        None,
        None,
        0.0,
        CancellationToken::new(),
        Some(envelope),
        Some(scope),
    )
    .await
    .expect("test: scheduled denied tool does not fail the agent turn");

    assert!(reply.contains(crate::providers::credential_probe::PROBE_REPLY));
    let requests = probe.requests();
    assert!(
        requests
            .iter()
            .filter(|request| request.path.contains("chat/completions"))
            .count()
            >= 2,
        "tool reply was not followed by a result round"
    );
    assert!(
        requests
            .iter()
            .filter(|request| request.path.contains("chat/completions"))
            .skip(1)
            .any(|request| request.tool_policy_denial_in_body),
        "the provider did not receive the creator-policy denial result"
    );
    assert!(!config.workspace_dir.join("scheduled-denied.txt").exists());
}

/// D-053 / REDACT-TURN probe: a single-shot `prx agent` turn whose provider
/// replies with a secret runs `run_with_runtime_envelope` to the `Completed`
/// `finalize_turn` commit; nothing under the workspace may keep the secret, so
/// this fails before the REDACT-TURN commits.
#[tokio::test]
async fn cli_agent_persists_no_secret_from_a_reply() {
    use crate::agent::terminal::persistence_probe::{PROBE_SECRET, assert_nothing_under_persists_the_probe_secret};
    use crate::providers::credential_probe::CredentialProbe;

    let tmp = TempDir::new().expect("test: tempdir");
    let probe = CredentialProbe::start_replying(&format!("Here is the key {PROBE_SECRET} you asked for")).await;
    let mut config = cli_agent_probe_config(tmp.path(), &probe);
    config.autonomy.level = crate::security::AutonomyLevel::ReadOnly;
    // Real provider-persistence fixture: a controlled local decision selects
    // the legal Answer candidate through actual compiler/validator/gates.
    // No tool completion or invented grant is needed to test reply redaction.
    let driver = crate::agent::agent::native_decision_fixture::Driver::start_answer()
        .await
        .expect("owned loopback decision driver");
    crate::agent::agent::native_decision_fixture::configure(&mut config, driver.endpoint())
        .expect("actual compatible decision model binding");
    let workspace = config.workspace_dir.clone();

    let reply = driver
        .run(
            true,
            run_with_runtime_envelope(
                config,
                Some("Return a short PRX project greeting.".to_string()),
                None,
                None,
                0.0,
                CancellationToken::new(),
                None,
                None,
            ),
        )
        .await
        .expect("test: the single-shot agent turn completes");

    assert!(
        reply.contains("Here is the key"),
        "test: the turn returned the provider reply, got {reply:?}"
    );
    assert!(
        cli_agent_probe_saw_a_chat_request(&probe),
        "test: the turn reached the provider"
    );
    assert_cli_redaction_answer_receipts(&workspace, "completed");
    assert_nothing_under_persists_the_probe_secret(tmp.path());
}

/// D-053 / REDACT-TURN probe: a single-shot `prx agent` turn whose provider
/// fails with an error echoing a secret runs `run_with_runtime_envelope` to the
/// `Failed` `finalize_turn` commit; nothing under the workspace may keep the
/// secret, so this fails before the REDACT-TURN commits.
#[tokio::test]
async fn cli_agent_persists_no_secret_from_a_provider_error() {
    use crate::agent::terminal::persistence_probe::{PROBE_SECRET, assert_nothing_under_persists_the_probe_secret};
    use crate::providers::credential_probe::CredentialProbe;

    let tmp = TempDir::new().expect("test: tempdir");
    let probe = CredentialProbe::start_failing(400, &format!("upstream rejected key {PROBE_SECRET}")).await;
    let mut config = cli_agent_probe_config(tmp.path(), &probe);
    config.autonomy.level = crate::security::AutonomyLevel::ReadOnly;
    // Real provider-persistence fixture: a controlled local decision selects
    // the legal Answer candidate through actual compiler/validator/gates.
    // No tool completion or invented grant is needed to test reply redaction.
    let driver = crate::agent::agent::native_decision_fixture::Driver::start_answer()
        .await
        .expect("owned loopback decision driver");
    crate::agent::agent::native_decision_fixture::configure(&mut config, driver.endpoint())
        .expect("actual compatible decision model binding");
    let workspace = config.workspace_dir.clone();

    let result = driver
        .run(
            true,
            run_with_runtime_envelope(
                config,
                Some("Return a short PRX project greeting.".to_string()),
                None,
                None,
                0.0,
                CancellationToken::new(),
                None,
                None,
            ),
        )
        .await;

    assert!(result.is_err(), "test: a failing provider fails the single-shot turn");
    assert!(
        cli_agent_probe_saw_a_chat_request(&probe),
        "test: the turn reached the provider"
    );
    assert_cli_redaction_answer_receipts(&workspace, "failed");
    assert_nothing_under_persists_the_probe_secret(tmp.path());
}

// Read actual post-cleanup receipts; a policy refusal or empty persistence
// cannot count as a provider-redaction positive.
fn assert_cli_redaction_answer_receipts(workspace: &std::path::Path, status: &str) {
    let terminals = crate::memory::sqlite::fixture_readback::terminal_rows(workspace).unwrap();
    assert_eq!(terminals.len(), 1, "one actual CLI whole-turn terminal");
    let terminal: serde_json::Value = serde_json::from_str(&terminals.first().unwrap().raw_payload).unwrap();
    assert_eq!(terminal.get("status").and_then(serde_json::Value::as_str), Some(status));
    let decision_id = terminal
        .get("decision_id")
        .and_then(serde_json::Value::as_str)
        .expect("actual CLI decision identity");
    let tool_events = crate::memory::sqlite::fixture_readback::tool_event_count(workspace).unwrap();
    assert_eq!(tool_events, 0, "Answer redaction fixture must execute no tools");
    let store = crate::causal_tree::store::CausalDecisionStore::open_unshared_read_only(workspace);
    let scope = crate::causal_tree::store::DecisionScope::Owner {
        workspace_id: store.workspace_id().unwrap(),
        owner_id: std::sync::Arc::from(terminals[0].owner_id.as_deref().expect("actual terminal owner")),
    };
    let decision = store
        .reader()
        .get(
            &scope,
            &crate::causal_tree::ids::DecisionId::parse(decision_id).unwrap(),
        )
        .unwrap()
        .unwrap();
    let (directive, backend) = (decision.directive_kind, decision.backend_kind);
    assert_eq!(directive, "answer", "the actual service must select Answer");
    assert_eq!(
        backend, "compatible_http",
        "builtin fallback is not controlled-answer acceptance"
    );
}

#[test]
fn cli_entry_future_fits_default_stack_and_unpolled_drop_is_lazy() {
    let workspace = TempDir::new().expect("test workspace");
    let mut config = Config::default();
    config.workspace_dir = workspace.path().to_path_buf();
    config.config_path = workspace.path().join("config.toml");
    let shutdown = CancellationToken::new();
    let future = run_with_runtime_envelope(
        config,
        Some("prx stack layout input".to_owned()),
        None,
        None,
        0.0,
        shutdown.clone(),
        None,
        None,
    );
    assert!(std::mem::size_of_val(&future) <= 64 * 1024);
    drop(future);
    assert!(!shutdown.is_cancelled());
    assert!(crate::runtime::turn_cleanup::current_tracker().is_none());
    assert_eq!(
        std::fs::read_dir(workspace.path()).expect("fixture directory").count(),
        0
    );
}

/// Every file under `root` with its size, for a before/after comparison.
fn workspace_tree(root: &std::path::Path) -> std::collections::BTreeMap<std::path::PathBuf, u64> {
    let mut tree = std::collections::BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.metadata() {
                Ok(metadata) if metadata.is_dir() => {
                    tree.insert(path.clone(), 0);
                    pending.push(path);
                }
                Ok(metadata) => {
                    tree.insert(path, metadata.len());
                }
                Err(_) => {}
            }
        }
    }
    tree
}

/// D-065 M-1: a selection the resolver refuses is refused before the runtime
/// bootstrap, so a refused `prx agent` run (and a cron or xin agent job,
/// which enter here) leaves the workspace exactly as it was: no memory
/// database, snapshot, hygiene state, plugin directory or audit file, and
/// nothing sent.
#[tokio::test]
async fn a_refused_selection_leaves_no_bootstrap_side_effects() {
    use crate::providers::credential_probe::CredentialProbe;

    let tmp = TempDir::new().expect("test: tempdir");
    let probe = CredentialProbe::start().await;
    let mut config = cli_agent_probe_config(tmp.path(), &probe);
    config.providers.models.push(
        toml::from_str(&format!(
            "provider = \"{}\"\nmodel = \"jev-decision-test\"\noperations = [\"decision_endpoint\"]\n",
            probe.custom_provider()
        ))
        .expect("test: declaration"),
    );
    config.default_model = Some("jev-decision-test".to_string());
    let before = workspace_tree(tmp.path());

    let error = run_with_runtime_envelope(
        config,
        Some("hi".to_string()),
        None,
        None,
        0.0,
        CancellationToken::new(),
        None,
        None,
    )
    .await
    .expect_err("a decision-only default is refused");

    assert!(format!("{error:#}").contains("cannot be used for chat"), "{error:#}");
    assert_eq!(
        workspace_tree(tmp.path()),
        before,
        "a refused run changed the workspace (bootstrap side effects)"
    );
    assert!(probe.requests().is_empty(), "nothing is sent");
}
