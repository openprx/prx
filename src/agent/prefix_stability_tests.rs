//! Does one chat session actually hold its provider request prefix still?
//!
//! On every cache-aware provider (OpenAI, Kimi, Anthropic) the system prompt
//! and the `tools` array are the head of the request, and a cache hit requires
//! a byte-identical prefix. Two per-turn decisions sit inside that prefix and
//! both used to be re-taken against the current turn's wording:
//!
//! * skill RAG re-ranks the `## Available Skills` section, which sits ahead of
//!   the identity and runtime sections, and
//! * capability routing re-selects the published tool set.
//!
//! Rephrasing the same question therefore re-prefilled the whole conversation.
//! The existing tests for both mechanisms assert on synthetic fixtures — a
//! literal base system prompt and probe tools — so neither could see this.
//! These tests run the real `build_runtime_system_prompt`, the real skill
//! retrieval, and the real router over several turns of one session.

#![cfg(test)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::indexing_slicing
)]

use crate::agent::runtime_prompt::{build_runtime_system_prompt, select_prompt_skills};
use crate::agent::turn_contracts::{ChatMode, ToolLoopOutcome};
use crate::agent::turn_engine::run_turn;
use crate::agent::turn_request::ToolLoopMemory;
use crate::config::{Config, ToolTieringConfig};
use crate::memory::embeddings::LocalHashEmbedding;
use crate::providers::traits::{ChatMessage, ChatRequest, ChatResponse, Provider};
use crate::skills::{SessionSkillExposure, Skill, SkillTool};
use crate::tools::legacy_intent_tests::{
    SessionToolExposure, SessionToolSurface, UnroutedToolPolicy, resolve_tools_for_intent,
};
use crate::tools::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const MODEL: &str = "prefix-stability-model";

fn skill(name: &str, description: &str) -> Skill {
    Skill {
        name: name.to_string(),
        description: description.to_string(),
        version: "0.1.0".to_string(),
        author: None,
        tags: Vec::new(),
        tools: vec![SkillTool {
            name: format!("{name}_helper"),
            description: format!("helper for {name}"),
            kind: "shell".to_string(),
            command: "true".to_string(),
            args: std::collections::HashMap::new(),
        }],
        prompts: Vec::new(),
        location: Some(std::path::PathBuf::from(format!("/skills/{name}/SKILL.md"))),
        embedding: None,
    }
}

/// Eight skills whose descriptions pull in different directions, so top_k = 5
/// retrieval genuinely re-ranks between rewordings of one question.
async fn catalog() -> Vec<Skill> {
    let mut skills = vec![
        skill("pdf-report", "extract tables and text out of PDF report files"),
        skill("web-research", "search the open web and summarise the pages found"),
        skill("git-release", "cut a release branch, tag it and push the commit"),
        skill("sql-tuning", "read slow query plans and propose index changes"),
        skill("csv-cleanup", "normalise messy CSV exports into tidy columns"),
        skill("image-crop", "crop and resize screenshots before publishing them"),
        skill("inbox-triage", "sort an email inbox and draft short replies"),
        skill("meeting-notes", "turn a raw transcript into decisions and actions"),
    ];
    let embedder = LocalHashEmbedding::new("local-hash", 128);
    crate::skills::hydrate_skill_embeddings(&mut skills, &embedder)
        .await
        .expect("fixture embeddings");
    skills
}

struct FixtureTool {
    name: &'static str,
    tier: ToolTier,
    categories: &'static [ToolCategory],
}

#[async_trait]
impl Tool for FixtureTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "prefix stability fixture"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _arguments: serde_json::Value) -> anyhow::Result<ToolResult> {
        Ok(ToolResult {
            success: true,
            output: String::new(),
            error: None,
        })
    }
    fn tier(&self) -> ToolTier {
        self.tier
    }
    fn categories(&self) -> &'static [ToolCategory] {
        self.categories
    }
}

fn registry() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(FixtureTool {
            name: "fixture_shell",
            tier: ToolTier::Core,
            categories: &[],
        }) as Box<dyn Tool>,
        Box::new(FixtureTool {
            name: "fixture_web_search",
            tier: ToolTier::Standard,
            categories: &[ToolCategory::WebBrowsing],
        }) as Box<dyn Tool>,
        Box::new(FixtureTool {
            name: "fixture_http",
            tier: ToolTier::Extended,
            categories: &[ToolCategory::WebBrowsing],
        }) as Box<dyn Tool>,
        Box::new(FixtureTool {
            name: "fixture_git",
            tier: ToolTier::Standard,
            categories: &[ToolCategory::DevOps],
        }) as Box<dyn Tool>,
        Box::new(FixtureTool {
            name: "fixture_cron",
            tier: ToolTier::Extended,
            categories: &[ToolCategory::Scheduling],
        }) as Box<dyn Tool>,
    ]
}

/// What a stateless entry point publishes for this turn.
fn exposed_names(tools: &[Box<dyn Tool>], message: &str, tiering: &ToolTieringConfig) -> Vec<String> {
    policy_names(tools, message, tiering, UnroutedToolPolicy::PublishEverything)
}

/// What a chat turn publishes: the session surface decided by the dispatcher,
/// resolved again by the shared tool loop exactly as production does.
fn session_names(tools: &[Box<dyn Tool>], message: &str, surface: &SessionToolSurface) -> Vec<String> {
    policy_names(tools, message, &surface.tiering, surface.unrouted)
}

fn policy_names(
    tools: &[Box<dyn Tool>],
    message: &str,
    tiering: &ToolTieringConfig,
    unrouted: UnroutedToolPolicy,
) -> Vec<String> {
    let mut names = resolve_tools_for_intent(
        tools,
        message,
        &tiering.always_include,
        &tiering.always_exclude,
        unrouted,
    )
    .into_iter()
    .map(|tool| tool.name().to_string())
    .collect::<Vec<_>>();
    names.sort();
    names
}

fn fixture_config(workspace: &std::path::Path) -> Config {
    Config {
        workspace_dir: workspace.to_path_buf(),
        config_path: workspace.join("config.toml"),
        ..Config::default()
    }
}

/// Four turns of one session: two wordings of one request, one turn that
/// reaches a genuinely new capability, and one that goes back to the first
/// subject. The system prompt must be byte-identical throughout and the
/// published tool set must move exactly once.
///
/// MUTATION GUARD: make `SessionToolExposure::sticky_surface` return
/// `base.clone()` and the last two tool-set assertions go red; drop the
/// catalog short-circuit in `select_prompt_skills` and the prompt assertions
/// go red.
#[tokio::test]
async fn one_session_holds_its_request_prefix_still_across_rewordings() {
    let workspace = tempfile::TempDir::new().expect("fixture workspace");
    let config = fixture_config(workspace.path());
    let skills = catalog().await;
    let tools = registry();
    let embedder = LocalHashEmbedding::new("local-hash", 128);
    let base = ToolTieringConfig::default();

    let mut skill_exposure = SessionSkillExposure::new();
    let tool_exposure = SessionToolExposure::new();

    let turns = [
        "search the web for the current rustls release notes",
        "can you search online and tell me what changed in rustls lately",
        "set a timer so cron reminds me about it tomorrow",
        "have a look at the rustls website again and search for the changelog",
    ];

    let mut prompts = Vec::new();
    let mut sticky_tools = Vec::new();
    let mut raw_tools = Vec::new();
    for message in turns {
        let retrieved = select_prompt_skills(message, &skills, &config, &embedder).await;
        let exposed = skill_exposure.absorb(&skills, &retrieved);
        prompts.push(build_runtime_system_prompt(&config, MODEL, &exposed, true));
        let surface = tool_exposure.sticky_surface(&base, &tools, message);
        sticky_tools.push(session_names(&tools, message, &surface));
        raw_tools.push(exposed_names(&tools, message, &base));
    }

    for (index, prompt) in prompts.iter().enumerate().skip(1) {
        assert_eq!(
            prompt.len(),
            prompts[0].len(),
            "turn {index} moved the system prompt byte count"
        );
        assert_eq!(prompt, &prompts[0], "turn {index} changed the system prompt");
    }

    assert_eq!(
        sticky_tools[0], sticky_tools[1],
        "a rewording of the same request must publish the same tools"
    );
    assert!(
        sticky_tools[0].iter().any(|name| name == "fixture_web_search"),
        "the fixture must route the named capability: {:?}",
        sticky_tools[0]
    );
    assert!(
        !sticky_tools[0].iter().any(|name| name == "fixture_cron"),
        "an unreached capability must stay off the early turns: {:?}",
        sticky_tools[0]
    );

    // Fixture proof: routing this turn alone drops the web tools.
    assert!(
        !raw_tools[2].iter().any(|name| name == "fixture_web_search"),
        "fixture is useless unless turn 3 would have dropped the web tools: {:?}",
        raw_tools[2]
    );
    assert!(
        sticky_tools[2].iter().any(|name| name == "fixture_cron"),
        "a new capability must reach its tool: {:?}",
        sticky_tools[2]
    );
    assert!(
        sticky_tools[2].iter().any(|name| name == "fixture_web_search"),
        "and must not drop what earlier turns already published: {:?}",
        sticky_tools[2]
    );

    // Fixture proof: routing turn 4 alone drops the scheduling tool again.
    assert!(
        !raw_tools[3].iter().any(|name| name == "fixture_cron"),
        "fixture is useless unless turn 4 would have dropped cron: {:?}",
        raw_tools[3]
    );
    assert_eq!(
        sticky_tools[3], sticky_tools[2],
        "the published set must settle after the one capability it gained"
    );

    println!(
        "prefix stability: system prompt {} bytes across {} turns, tools {:?}",
        prompts[0].len(),
        turns.len(),
        sticky_tools[3]
    );
}

/// A catalog larger than `[skill_rag] top_k` cannot be published whole, so
/// retrieval does move with the wording. The session union is what stops that
/// from re-prefilling the conversation on every turn: the section may grow,
/// never shrink, and it settles as soon as a subject comes back.
#[tokio::test]
async fn an_oversized_skill_catalog_grows_into_a_stable_section() {
    let workspace = tempfile::TempDir::new().expect("fixture workspace");
    let mut config = fixture_config(workspace.path());
    config.skill_rag.top_k = 3;
    let skills = catalog().await;
    let embedder = LocalHashEmbedding::new("local-hash", 128);

    let messages = [
        "extract the tables out of this PDF report",
        "crop the screenshot before we publish it",
        "pull the tables out of that PDF again",
    ];

    let mut raw = Vec::new();
    let mut prompts = Vec::new();
    let mut exposure = SessionSkillExposure::new();
    for message in messages {
        let retrieved = select_prompt_skills(message, &skills, &config, &embedder).await;
        assert_eq!(retrieved.len(), 3, "an oversized catalog must still be narrowed");
        raw.push(crate::skills::skills_to_prompt(&retrieved, workspace.path()));
        let exposed = exposure.absorb(&skills, &retrieved);
        prompts.push(build_runtime_system_prompt(&config, MODEL, &exposed, true));
    }

    assert_ne!(
        raw[0], raw[1],
        "fixture is useless unless retrieval moves between these two subjects"
    );
    assert!(
        prompts[1].len() >= prompts[0].len(),
        "the section may only grow inside one session"
    );
    assert_eq!(
        prompts[2], prompts[1],
        "returning to an earlier subject must not move the prefix again"
    );

    exposure.reset();
    let retrieved = select_prompt_skills(messages[0], &skills, &config, &embedder).await;
    let exposed = exposure.absorb(&skills, &retrieved);
    assert_eq!(
        build_runtime_system_prompt(&config, MODEL, &exposed, true),
        prompts[0],
        "a session boundary must start the union over"
    );
}

/// The system prompt built from one skill set must not depend on the order
/// retrieval happened to return it in — this is the part `skills_to_prompt`
/// owns, and the F0 tests never exercised it through the real builder.
///
/// MUTATION GUARD: drop the sort in `canonical_skill_order` and this goes red.
#[tokio::test]
async fn the_real_system_prompt_is_a_pure_function_of_the_exposed_skill_set() {
    let workspace = tempfile::TempDir::new().expect("fixture workspace");
    let config = fixture_config(workspace.path());
    let skills = catalog().await;

    let forward: Vec<Skill> = skills.iter().take(5).cloned().collect();
    let mut shuffled = forward.clone();
    shuffled.reverse();
    shuffled.swap(0, 2);
    let mut with_duplicate = shuffled.clone();
    with_duplicate.push(forward[1].clone());

    let a = build_runtime_system_prompt(&config, MODEL, &forward, true);
    let b = build_runtime_system_prompt(&config, MODEL, &shuffled, true);
    let c = build_runtime_system_prompt(&config, MODEL, &with_duplicate, true);

    assert_eq!(a, b, "a reordered skill set must render byte-identically");
    assert_eq!(a, c, "a duplicated skill name must not double the section");

    let dropped: Vec<Skill> = forward.iter().take(4).cloned().collect();
    assert_ne!(
        a,
        build_runtime_system_prompt(&config, MODEL, &dropped, true),
        "changing the exposed set must still change the prompt"
    );
}

/// Records the system prefix of every provider request, and answers the first
/// call with an empty response so the loop takes its corrective-retry path.
struct RetryPrefixProvider {
    calls: AtomicUsize,
    system_digests: parking_lot::Mutex<Vec<String>>,
    tail_roles: parking_lot::Mutex<Vec<String>>,
    saw_late_system: parking_lot::Mutex<bool>,
    saw_adjacent_user: parking_lot::Mutex<bool>,
    role_shapes: parking_lot::Mutex<Vec<Vec<String>>>,
}

impl RetryPrefixProvider {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            system_digests: parking_lot::Mutex::new(Vec::new()),
            tail_roles: parking_lot::Mutex::new(Vec::new()),
            saw_late_system: parking_lot::Mutex::new(false),
            saw_adjacent_user: parking_lot::Mutex::new(false),
            role_shapes: parking_lot::Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl Provider for RetryPrefixProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        anyhow::bail!("the prefix stability fixture always uses the chat path")
    }

    async fn chat(&self, request: ChatRequest<'_>, _model: &str, _temperature: f64) -> anyhow::Result<ChatResponse> {
        if request
            .messages
            .iter()
            .enumerate()
            .any(|(index, message)| message.role == "system" && index != 0)
        {
            *self.saw_late_system.lock() = true;
        }
        let roles = request
            .messages
            .iter()
            .map(|message| message.role.clone())
            .collect::<Vec<_>>();
        if roles.windows(2).any(|pair| pair == ["user", "user"]) {
            *self.saw_adjacent_user.lock() = true;
        }
        self.role_shapes.lock().push(roles);
        let system = request
            .messages
            .first()
            .filter(|message| message.role == "system")
            .map(|message| message.content.as_str())
            .unwrap_or_default();
        self.system_digests
            .lock()
            .push(format!("{:x}", Sha256::digest(system.as_bytes())));
        if let Some(last) = request.messages.last() {
            self.tail_roles.lock().push(format!("{}:{}", last.role, last.content));
        }
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            // An empty answer with no tool calls: exactly what triggers the
            // corrective retry that used to append request-local system context.
            return Ok(ChatResponse {
                text: Some(String::new()),
                tool_calls: Vec::new(),
                reasoning_content: None,
            });
        }
        Ok(ChatResponse {
            text: Some("Recovered answer.".to_string()),
            tool_calls: Vec::new(),
            reasoning_content: None,
        })
    }
}

/// A corrective retry is the one place inside a turn that adds request-local
/// instructions. Folding them into the leading system message rewrote the
/// prefix every cache-aware provider keys on — on the turn that has already
/// paid for one wasted provider call.
///
/// MUTATION GUARD: push the two retry instructions as `ChatMessage::system`
/// again and the digest assertion goes red.
#[tokio::test]
async fn a_corrective_retry_round_does_not_move_the_system_prefix() {
    // Q45: Corrective retry preserves system prefix and canonical message-role ordering.
    q115_boxed_fixture_work(
        crate::runtime::turn_decision::evidence_fixture::with_evidence_sufficient(q115_boxed_fixture_work(async {
            let workspace = tempfile::TempDir::new().expect("fixture workspace");
            let provider = RetryPrefixProvider::new();
            let tools: Vec<Box<dyn Tool>> = Vec::new();
            let mut history = vec![
                ChatMessage::system("You are a helpful assistant.\n\n## Identity\n\nPRX."),
                ChatMessage::user("what changed in rustls lately"),
            ];

            let admission = crate::agent::turn_tests::admission::AdmittedTurn::empty(
                workspace.path(),
                "what changed in rustls lately",
            )
            .await;
            let (outcome, _trace) = run_turn(
                &mut history,
                crate::agent::turn_request::TurnRequest {
                    channel_name: "terminal",
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
                    decision: admission.decision(),
                },
                crate::agent::turn_request::TurnServices {
                    provider: &provider,
                    provider_name: "ollama",
                    model: "prx-local-model",
                    temperature: 0.0,
                    tools_registry: Arc::new(tools),
                    observer: &crate::observability::NoopObserver,
                    hooks: &crate::hooks::HookManager::new(workspace.path().to_path_buf()),
                    approval: None,
                    approval_resolver: None,
                    runtime_adapter: Some(admission.adapter()),
                    memory: ToolLoopMemory::none(),
                },
                crate::agent::turn_request::TurnDelivery {
                    cancellation: None,
                    on_delta: None,
                    on_tool_call: None,
                },
            )
            .await
            .expect("the corrective retry must complete the turn");

            assert!(matches!(outcome, ToolLoopOutcome::Text(ref text) if text == "Recovered answer."));

            let digests = provider.system_digests.lock().clone();
            assert_eq!(digests.len(), 2, "the fixture must actually retry once");
            assert_eq!(
                digests[0], digests[1],
                "the retry round must send a byte-identical system prefix"
            );
            assert!(
                !*provider.saw_late_system.lock(),
                "no request may carry a system message outside index 0"
            );

            // Fixture proof: the corrective instruction really did reach the retry, at
            // the tail, so the assertion above is not passing because nothing was added.
            let tails = provider.tail_roles.lock().clone();
            assert!(
                tails[1].starts_with("user:") && tails[1].contains("The previous assistant response was empty."),
                "the retry instruction must ride at the tail of the request: {}",
                tails[1]
            );
            assert!(
                history
                    .iter()
                    .all(|message| !message.content.contains("The previous assistant response was empty.")),
                "the corrective instruction must stay request-local"
            );

            // The retry must not change the *shape* of the request either. Pushing the
            // instruction as its own turn produced `[system, user, user]`, which no
            // provider in this crate coalesces and strict chat templates reject.
            let shapes = provider.role_shapes.lock().clone();
            assert!(
                !*provider.saw_adjacent_user.lock(),
                "a corrective retry must not produce two adjacent user turns: {shapes:?}"
            );
            assert_eq!(
                shapes[0], shapes[1],
                "the retry round must send the same role sequence as the first round: {shapes:?}"
            );

            // The instruction is the runtime's words, not the operator's, so it is
            // labelled and therefore separable from the turn it rides on.
            let (operator_text, aside) = tails[1]
                .trim_start_matches("user:")
                .split_once(crate::agent::response_guard::CORRECTIVE_ASIDE_PREFIX)
                .expect("the corrective aside must carry its marker");
            assert_eq!(
                operator_text.trim(),
                "what changed in rustls lately",
                "everything before the marker must be exactly what the operator wrote"
            );
            assert!(
                aside.contains("The previous assistant response was empty."),
                "everything after the marker must be the runtime's instruction: {aside}"
            );
        })),
    )
    .await;
}

#[inline(never)]
fn q115_boxed_fixture_work<'a, F: std::future::Future + 'a>(
    work: F,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = F::Output> + 'a>> {
    Box::pin(work)
}

struct CacheMetricsProvider {
    name: String,
    calls: AtomicUsize,
    tool_round: bool,
    fail: bool,
    streaming: bool,
}

impl CacheMetricsProvider {
    fn new(name: &str, tool_round: bool, streaming: bool, fail: bool) -> Self {
        Self {
            name: format!("custom:http://127.0.0.1:1/{name}"),
            calls: AtomicUsize::new(0),
            tool_round,
            streaming,
            fail,
        }
    }
    fn response(&self) -> anyhow::Result<ChatResponse> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        anyhow::ensure!(!self.fail, "cache fixture provider failed");
        let tool = self.tool_round && call == 0;
        Ok(ChatResponse {
            text: (!tool).then(|| "Cache fixture answer.".into()),
            tool_calls: if tool {
                vec![crate::providers::ToolCall {
                    id: "cache-fixture-call".into(),
                    name: "fixture_shell".into(),
                    arguments: "{}".into(),
                }]
            } else {
                Vec::new()
            },
            reasoning_content: None,
        })
    }
    fn usage() -> crate::llm::route_decision::TokenUsage {
        crate::llm::route_decision::TokenUsage::reported_with_cache(
            Some(1000),
            Some(10),
            Some(1010),
            Some(0),
            Some(800),
        )
    }
}

#[async_trait]
impl Provider for CacheMetricsProvider {
    fn capabilities(&self) -> crate::providers::traits::ProviderCapabilities {
        crate::providers::traits::ProviderCapabilities {
            native_tool_calling: true,
            vision: false,
        }
    }

    async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
        anyhow::bail!("cache fixture requires the main chat path")
    }
    async fn chat_traced(
        &self,
        _: ChatRequest<'_>,
        model: &str,
        _: f64,
    ) -> anyhow::Result<crate::providers::traits::ChatTrace> {
        assert!(!self.streaming, "streaming fixture must reach the stream path");
        Ok(crate::providers::traits::ChatTrace {
            response: self.response()?,
            attempts: Vec::new(),
            final_provider: self.name.clone(),
            final_model: model.into(),
            served_member: None,
            tokens_used: Self::usage(),
        })
    }
    fn supports_streaming(&self) -> bool {
        self.streaming
    }
    fn stream_chat_with_history(
        &self,
        _: &[ChatMessage],
        _: &str,
        _: f64,
        _: crate::providers::traits::StreamOptions,
    ) -> futures_util::stream::BoxStream<
        'static,
        crate::providers::traits::StreamResult<crate::providers::traits::StreamChunk>,
    > {
        use crate::providers::traits::{StreamChunk, ToolCallChunk};
        assert!(self.streaming);
        let response = self.response().expect("successful streaming fixture");
        let mut chunks = vec![Ok(StreamChunk::usage(Self::usage()))];
        if let Some(text) = response.text {
            chunks.push(Ok(StreamChunk::delta(text)));
        }
        if !response.tool_calls.is_empty() {
            chunks.push(Ok(StreamChunk::tool_call_chunk(
                response
                    .tool_calls
                    .into_iter()
                    .enumerate()
                    .map(|(index, call)| ToolCallChunk::new(call.id, call.name, call.arguments, index))
                    .collect(),
            )));
        }
        chunks.push(Ok(StreamChunk::final_chunk()));
        Box::pin(futures_util::stream::iter(chunks))
    }
}

async fn cache_metrics_turn(
    workspace: &std::path::Path,
    provider: &CacheMetricsProvider,
    scope: Option<&crate::agent::turn_contracts::ScopeContext<'_>>,
    memory: ToolLoopMemory,
) -> anyhow::Result<()> {
    let tools: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(FixtureTool {
        name: "fixture_shell",
        tier: ToolTier::Core,
        categories: &[],
    })]);
    let mut config = fixture_config(workspace);
    // Admission validates the endpoint and price even for a scripted provider.
    config.cost.prices.insert(
        format!("{}/{MODEL}", provider.name),
        crate::config::schema::ModelPricing {
            input: 0.0,
            output: 0.0,
            cache_write: 0.0,
            cache_read: 0.0,
        },
    );
    let memory_backend: Arc<dyn crate::memory::Memory> = Arc::new(crate::memory::SqliteMemory::new(workspace).unwrap());
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_model(
        config.clone(),
        "cache fixture request",
        Arc::clone(&tools),
        memory_backend,
        (&provider.name, MODEL),
    )
    .await;
    let mut adapter = admission.adapter();
    adapter.stream_provider = provider.streaming;
    let system = build_runtime_system_prompt(&config, MODEL, &[], true);
    let mut history = vec![ChatMessage::system(system), ChatMessage::user("cache fixture request")];
    let hooks = crate::hooks::HookManager::new(workspace.to_path_buf());
    let result = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "terminal",
            scope,
            mode: ChatMode::default(),
            silent: true,
            expose_stay_silent: false,
            multimodal: &config.multimodal,
            schedule: crate::agent::turn_request::TurnSchedule {
                read_only_tool_concurrency_window: 2,
                priority_scheduling_enabled: false,
                low_priority_tool_names: Vec::new(),
            },
            compaction: None,
            decision: admission.decision(),
        },
        crate::agent::turn_request::TurnServices {
            provider,
            provider_name: &provider.name,
            model: MODEL,
            temperature: 0.0,
            tools_registry: tools,
            observer: &crate::observability::NoopObserver,
            hooks: &hooks,
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(adapter),
            memory,
        },
        crate::agent::turn_request::TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await;
    result.map(|_| ())
}

fn cache_row(provider: &CacheMetricsProvider) -> crate::agent::prompt_cache_metrics::PromptCacheStats {
    crate::agent::prompt_cache_metrics::snapshot()
        .get(&format!("{}/{MODEL}", provider.name))
        .cloned()
        .unwrap_or_default()
}

async fn cache_metrics_two_rounds(streaming: bool) {
    let workspace = tempfile::tempdir().unwrap();
    let provider = CacheMetricsProvider::new(
        if streaming {
            "cache-streaming-fixture"
        } else {
            "cache-buffered-fixture"
        },
        true,
        streaming,
        false,
    );
    cache_metrics_turn(workspace.path(), &provider, None, ToolLoopMemory::none())
        .await
        .unwrap();
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    let row = cache_row(&provider);
    assert_eq!(row.requests_total, 2);
    assert_eq!(row.cache_reported_total, 2);
    assert_eq!(row.cache_hit_requests_total, 2);
    assert_eq!(row.cache_read_tokens_total, 1600);
    assert_eq!(
        (
            row.series_first_total,
            row.series_continuations_total,
            row.series_unkeyed_total
        ),
        (1, 1, 2)
    );
}

#[tokio::test]
async fn prompt_cache_records_two_buffered_tool_loop_rounds() {
    q115_boxed_fixture_work(cache_metrics_two_rounds(false)).await;
}

#[tokio::test]
async fn prompt_cache_records_two_streaming_tool_loop_rounds() {
    q115_boxed_fixture_work(cache_metrics_two_rounds(true)).await;
}

#[tokio::test]
async fn prompt_cache_does_not_record_failed_round_trip() {
    q115_boxed_fixture_work(async {
        let workspace = tempfile::tempdir().unwrap();
        let provider = CacheMetricsProvider::new("cache-failed-fixture", false, false, true);
        assert!(
            cache_metrics_turn(workspace.path(), &provider, None, ToolLoopMemory::none())
                .await
                .is_err()
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert_eq!(cache_row(&provider).requests_total, 0);
    })
    .await;
}

async fn cache_metrics_identity_series(scoped: bool, event_keyed: bool) {
    use crate::agent::turn_contracts::ScopeContext;
    let workspace = tempfile::tempdir().unwrap();
    let provider = CacheMetricsProvider::new(
        if scoped {
            "cache-scoped-fixture"
        } else if event_keyed {
            "cache-event-fixture"
        } else {
            "cache-unkeyed-fixture"
        },
        false,
        false,
        false,
    );
    let config = fixture_config(workspace.path());
    let policy = crate::security::SecurityPolicy::from_config(&config.autonomy, workspace.path());
    let scope = ScopeContext {
        policy: &policy,
        sender: "prx_user",
        channel: "terminal",
        chat_type: "private",
        chat_id: "cache-series",
        session_key: Some("cache-series"),
        owner_id: None,
        owner_isolated: false,
        topic_id: None,
        task_id: None,
        source_message_event_id: None,
        config_generation_id: None,
        config_source_revision: None,
    };
    std::fs::write(workspace.path().join("IDENTITY.md"), "PRX fixture identity one").unwrap();
    for turn in 0..3 {
        if turn == 2 {
            std::fs::write(workspace.path().join("IDENTITY.md"), "PRX fixture identity two").unwrap();
        }
        let mut event = crate::memory::MessageEventScope::new("chat", crate::memory::MemoryVisibility::Workspace)
            .with_channel("terminal");
        if event_keyed {
            event = event.with_session_key("cache-event-session");
        }
        cache_metrics_turn(
            workspace.path(),
            &provider,
            scoped.then_some(&scope),
            ToolLoopMemory::none().with_request_event_scope(event),
        )
        .await
        .unwrap();
        let row = cache_row(&provider);
        assert_eq!(row.requests_total, turn + 1);
        assert_eq!(
            row.system_prefix_changed_total,
            u64::from(turn == 2 && (scoped || event_keyed))
        );
    }
    let row = cache_row(&provider);
    assert_eq!(row.tools_prefix_changed_total, 0);
    if scoped || event_keyed {
        assert_eq!(
            (
                row.series_first_total,
                row.series_continuations_total,
                row.series_unkeyed_total
            ),
            (1, 2, 0)
        );
    } else {
        assert_eq!(
            (
                row.series_first_total,
                row.series_continuations_total,
                row.series_unkeyed_total
            ),
            (3, 0, 3)
        );
    }
}

#[tokio::test]
async fn prompt_cache_scoped_turns_compare_identity_prefix_changes() {
    q115_boxed_fixture_work(cache_metrics_identity_series(true, false)).await;
}

#[tokio::test]
async fn prompt_cache_cli_event_scope_compares_identity_prefix_changes() {
    q115_boxed_fixture_work(cache_metrics_identity_series(false, true)).await;
    q115_boxed_fixture_work(cache_metrics_identity_series(false, false)).await;
}
