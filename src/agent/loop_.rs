use crate::approval::{ApprovalManager, ApprovalRequest, ApprovalResponse};
use crate::config::Config;
use crate::hooks::{payload_error, HookEvent, HookManager};
use crate::memory::{self, Memory, MemoryCategory};
use crate::multimodal;
use crate::observability::{self, Observer, ObserverEvent};
use crate::providers::{
    self, ChatMessage, ChatRequest, Provider, ProviderCapabilityError, ToolCall,
};
use crate::runtime;
use crate::security::SecurityPolicy;
use crate::tools::{self, Tool};
use crate::util::truncate_with_ellipsis;
use anyhow::Result;
use regex::{Regex, RegexSet};
use std::collections::{hash_map::DefaultHasher, BTreeSet, HashMap};
use std::fmt::Write;
use std::hash::{Hash, Hasher};
use std::io::Write as _;
use std::sync::{Arc, LazyLock};
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Minimum characters per chunk when relaying LLM text to a streaming draft.
const STREAM_CHUNK_MIN_CHARS: usize = 80;

/// Context for scope-based tool access control.
/// When present, `run_tool_call_loop` will check each tool call against
/// the security policy's scope rules before execution.
pub(crate) struct ScopeContext<'a> {
    pub policy: &'a SecurityPolicy,
    pub sender: &'a str,
    pub channel: &'a str,
    pub chat_type: &'a str,
    pub chat_id: &'a str,
    /// Optional multi-layer tool policy pipeline (P3-1).
    /// When set, tool calls are additionally evaluated against the pipeline
    /// before execution. A denial from the pipeline blocks the tool call.
    pub policy_pipeline: Option<&'a crate::security::PolicyPipeline>,
}

/// P2 concurrency governance controls used by the tool scheduler.
#[derive(Debug, Clone)]
pub struct ToolConcurrencyGovernanceConfig {
    pub kill_switch_force_serial: bool,
    pub rollout_stage: String,
    pub rollout_sample_percent: u8,
    pub rollout_channels: Vec<String>,
    pub auto_rollback_enabled: bool,
    pub rollback_timeout_rate_threshold: f64,
    pub rollback_cancel_rate_threshold: f64,
    pub rollback_error_rate_threshold: f64,
}

impl Default for ToolConcurrencyGovernanceConfig {
    fn default() -> Self {
        Self {
            kill_switch_force_serial: false,
            rollout_stage: "off".to_string(),
            rollout_sample_percent: 0,
            rollout_channels: Vec::new(),
            auto_rollback_enabled: true,
            rollback_timeout_rate_threshold: 0.2,
            rollback_cancel_rate_threshold: 0.2,
            rollback_error_rate_threshold: 0.2,
        }
    }
}

/// Default maximum agentic tool-use iterations per user message to prevent runaway loops.
/// Used as a safe fallback when `max_tool_iterations` is unset or configured as zero.
const DEFAULT_MAX_TOOL_ITERATIONS: usize = 10;

/// Mid-turn history length threshold: trim immediately inside the tool-call loop when history
/// exceeds this value, rather than waiting for the turn to finish.
const MID_TURN_COMPACT_THRESHOLD: usize = 80;

/// Minimum user-message length (in chars) for auto-save to memory.
/// Matches the channel-side constant in `channels/mod.rs`.
const AUTOSAVE_MIN_MESSAGE_CHARS: usize = 20;
const TOOL_PARSE_LOG_PREVIEW_CHARS: usize = 200;

/// Maximum characters allowed for a single tool result before truncation.
const MAX_TOOL_RESULT_CHARS: usize = 30_000;

/// Timeout (seconds) for apply_configurable_compaction before falling back to aggressive trim.
const COMPACTION_TIMEOUT_SECS: u64 = 300;

/// Maximum number of times to retry an LLM call after a context overflow error.
const MAX_OVERFLOW_RETRIES: usize = 3;

static SENSITIVE_KEY_PATTERNS: LazyLock<RegexSet> = LazyLock::new(|| {
    RegexSet::new([
        r"(?i)token",
        r"(?i)api[_-]?key",
        r"(?i)password",
        r"(?i)secret",
        r"(?i)user[_-]?key",
        r"(?i)bearer",
        r"(?i)credential",
    ])
    .expect("compile regex set: sensitive key name patterns")
});

static SENSITIVE_KV_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(token|api[_-]?key|password|secret|user[_-]?key|bearer|credential)["']?\s*[:=]\s*(?:"([^"]{8,})"|'([^']{8,})'|([a-zA-Z0-9_\-\.]{8,}))"#).expect("compile regex: sensitive key-value pair pattern")
});

static SENSITIVE_LOG_KV_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)(["']?\b(?:token|api[_-]?key|password|secret|user[_-]?key|credential|key)\b["']?\s*[:=]\s*)(?:"[^"]{8,}"|'[^']{8,}'|[^\s,;}{]{8,})"#,
    )
    .expect("compile regex: sensitive log key-value pattern")
});

static SENSITIVE_BEARER_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)\bbearer\s+[a-z0-9._~+/=-]{8,}"#).expect("compile regex: bearer token pattern"));

/// Scrub credentials from tool output to prevent accidental exfiltration.
/// Replaces known credential patterns with a redacted placeholder while preserving
/// a small prefix for context.
fn scrub_credentials(input: &str) -> String {
    SENSITIVE_KV_REGEX
        .replace_all(input, |caps: &regex::Captures| {
            let full_match = &caps[0];
            let key = &caps[1];
            let val = caps
                .get(2)
                .or(caps.get(3))
                .or(caps.get(4))
                .map(|m| m.as_str())
                .unwrap_or("");

            // Preserve first 4 chars for context, then redact
            let prefix = if val.len() > 4 { &val[..4] } else { "" };

            if full_match.contains(':') {
                if full_match.contains('"') {
                    format!("\"{}\": \"{}*[REDACTED]\"", key, prefix)
                } else {
                    format!("{}: {}*[REDACTED]", key, prefix)
                }
            } else if full_match.contains('=') {
                if full_match.contains('"') {
                    format!("{}=\"{}*[REDACTED]\"", key, prefix)
                } else {
                    format!("{}={}*[REDACTED]", key, prefix)
                }
            } else {
                format!("{}: {}*[REDACTED]", key, prefix)
            }
        })
        .to_string()
}

fn sanitize_tool_parse_log_preview(input: &str) -> String {
    let with_secret_prefixes_scrubbed = providers::scrub_secret_patterns(input);
    let with_bearer_scrubbed = SENSITIVE_BEARER_REGEX
        .replace_all(&with_secret_prefixes_scrubbed, "Bearer [REDACTED]")
        .to_string();
    let with_kv_scrubbed = SENSITIVE_LOG_KV_REGEX
        .replace_all(&with_bearer_scrubbed, "$1[REDACTED]")
        .to_string();
    if with_kv_scrubbed.chars().count() <= TOOL_PARSE_LOG_PREVIEW_CHARS {
        return with_kv_scrubbed;
    }

    with_kv_scrubbed
        .chars()
        .take(TOOL_PARSE_LOG_PREVIEW_CHARS)
        .collect()
}

/// Default trigger for auto-compaction when non-system message count exceeds this threshold.
/// Prefer passing the config-driven value via `run_tool_call_loop`; this constant is only
/// used when callers omit the parameter.
const DEFAULT_MAX_HISTORY_MESSAGES: usize = 50;

/// Keep this many most-recent non-system messages after compaction.
const COMPACTION_KEEP_RECENT_MESSAGES: usize = 20;

/// Safety cap for compaction source transcript passed to the summarizer.
const COMPACTION_MAX_SOURCE_CHARS: usize = 12_000;

/// Max characters retained in stored compaction summary.
const COMPACTION_MAX_SUMMARY_CHARS: usize = 2_000;
const MEMORY_FLUSH_MAX_CHARS: usize = 800;

struct RecalledMemoryContext {
    preamble: String,
    ids: Vec<String>,
}

/// Convert a tool registry to OpenAI function-calling format for native tool support.
fn tools_to_openai_format(tools_registry: &[Box<dyn Tool>]) -> Vec<serde_json::Value> {
    tools_registry
        .iter()
        .map(|tool| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": tool.name(),
                    "description": tool.description(),
                    "parameters": tool.parameters_schema()
                }
            })
        })
        .collect()
}

fn autosave_memory_key(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4())
}

/// Trim conversation history to prevent unbounded growth.
/// Preserves the system prompt (first message if role=system) and the most recent messages.
fn trim_history(history: &mut Vec<ChatMessage>, max_history: usize) {
    // Nothing to trim if within limit
    let has_system = history.first().map_or(false, |m| m.role == "system");
    let non_system_count = if has_system {
        history.len() - 1
    } else {
        history.len()
    };

    if non_system_count <= max_history {
        return;
    }

    let start = if has_system { 1 } else { 0 };
    let to_remove = non_system_count - max_history;
    history.drain(start..start + to_remove);
}

/// Aggressive trim fallback: keep only the most recent non-system messages.
fn apply_aggressive_trim(history: &mut Vec<ChatMessage>, keep_recent: usize) {
    trim_history(history, keep_recent.max(1));
}

/// Truncate a tool result string if it exceeds max_chars.
/// Appends a note describing how many characters were dropped.
fn truncate_tool_result_if_needed(content: &str, max_chars: usize) -> String {
    if content.len() <= max_chars {
        return content.to_string();
    }
    // Safe UTF-8 boundary
    let boundary = content
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|&i| i <= max_chars)
        .last()
        .unwrap_or(0);
    let kept = &content[..boundary];
    format!(
        "{}\n\n[truncated: output exceeded context limit ({} chars, kept {})]",
        kept,
        content.len(),
        boundary
    )
}

/// Returns true when an LLM error indicates the request exceeded the context window.
fn is_context_overflow_error(err: &anyhow::Error) -> bool {
    let msg = err.to_string().to_lowercase();
    msg.contains("context_length_exceeded")
        || msg.contains("maximum context length")
        || msg.contains("token limit")
        || msg.contains("too many tokens")
        || msg.contains("context window")
        || msg.contains("max_tokens")
        || msg.contains("prompt is too long")
}

/// Token-aware mid-turn trim: remove the oldest non-system messages one at a time
/// until the estimated token count drops below max_tokens.
fn trim_history_token_aware(history: &mut Vec<ChatMessage>, max_tokens: usize) {
    loop {
        if estimate_history_tokens(history) <= max_tokens {
            break;
        }
        let has_system = history.first().is_some_and(|m| m.role == "system");
        let start = if has_system { 1 } else { 0 };
        if history.len() <= start + 1 {
            break; // nothing left to remove
        }
        history.remove(start);
    }
}

fn build_compaction_transcript(messages: &[ChatMessage]) -> String {
    let mut transcript = String::new();
    for msg in messages {
        let role = msg.role.to_uppercase();
        let _ = writeln!(transcript, "{role}: {}", msg.content.trim());
    }

    if transcript.chars().count() > COMPACTION_MAX_SOURCE_CHARS {
        truncate_with_ellipsis(&transcript, COMPACTION_MAX_SOURCE_CHARS)
    } else {
        transcript
    }
}

fn apply_compaction_summary(
    history: &mut Vec<ChatMessage>,
    start: usize,
    compact_end: usize,
    summary: &str,
) {
    let summary_msg = ChatMessage::assistant(format!("[Compaction summary]\n{}", summary.trim()));
    history.splice(start..compact_end, std::iter::once(summary_msg));
}

fn estimate_history_tokens(history: &[ChatMessage]) -> usize {
    // Fast heuristic: ~4 chars/token + small per-message framing.
    history
        .iter()
        .map(|msg| msg.role.chars().count() + msg.content.chars().count() + 12)
        .sum::<usize>()
        / 4
}

fn compaction_trigger_limit(config: &crate::config::AgentCompactionConfig) -> Option<usize> {
    if config.mode == crate::config::AgentCompactionMode::Off {
        return None;
    }
    let max_tokens = config.max_context_tokens;
    let reserve = config.reserve_tokens;
    if max_tokens <= reserve {
        return Some(0);
    }
    Some(max_tokens - reserve)
}

async fn apply_configurable_compaction(
    history: &mut Vec<ChatMessage>,
    provider: &dyn Provider,
    model: &str,
    config: &crate::config::AgentCompactionConfig,
) -> Result<bool> {
    let Some(limit) = compaction_trigger_limit(config) else {
        return Ok(false);
    };
    if estimate_history_tokens(history) <= limit {
        return Ok(false);
    }

    let has_system = history.first().is_some_and(|m| m.role == "system");
    let start = if has_system { 1 } else { 0 };
    let non_system_count = history.len().saturating_sub(start);
    if non_system_count <= 1 {
        return Ok(false);
    }

    let keep_recent = config.keep_recent_messages.max(1).min(non_system_count);
    let compact_count = non_system_count.saturating_sub(keep_recent);
    if compact_count == 0 {
        return Ok(false);
    }
    let compact_end = start + compact_count;
    let to_compact = history[start..compact_end].to_vec();
    let timestamp = chrono::Utc::now().to_rfc3339();

    if config.memory_flush {
        let flush_prompt = format!(
            "Write a concise memory flush message (max 6 bullets) capturing durable facts, decisions, and unresolved tasks from this history:\n\n{}",
            build_compaction_transcript(&to_compact)
        );
        let flush_note = provider
            .chat_with_system(
                Some("You write memory flush notes before context compaction."),
                &flush_prompt,
                model,
                0.1,
            )
            .await
            .unwrap_or_else(|_| "Memory flush fallback: key context retained.".to_string());
        history.insert(
            compact_end,
            ChatMessage::assistant(format!(
                "[Memory flush at {timestamp}: {}]",
                truncate_with_ellipsis(&flush_note, MEMORY_FLUSH_MAX_CHARS)
            )),
        );
    }

    let summary = match config.mode {
        crate::config::AgentCompactionMode::Safeguard => {
            let transcript = build_compaction_transcript(&to_compact);
            let structured_prompt = format!(
                "Summarize this conversation into a structured context summary. \
                 You MUST include ALL of these sections (use ## headers):\n\
                 ## Decisions\n\
                 ## Open TODOs\n\
                 ## Constraints/Rules\n\
                 ## Pending user asks\n\
                 ## Exact identifiers\n\
                 Preserve all UUIDs, hashes, file paths, URLs, port numbers, and \
                 IP addresses EXACTLY as written. Never paraphrase identifiers.\n\n\
                 ## Progress\n\
                 ### Done\n\
                 ### In Progress\n\
                 ### Blocked\n\n\
                 ## Recent turns preserved verbatim\n\
                 Include the last 2-3 user/assistant exchanges word-for-word.\n\n\
                 ## Critical Context\n\
                 Key technical details that would be lost if forgotten.\n\n\
                 Conversation to summarize:\n{}",
                transcript
            );
            let raw = provider
                .chat_with_system(
                    Some("You are a context compaction summarizer."),
                    &structured_prompt,
                    model,
                    0.2,
                )
                .await
                .unwrap_or_else(|_| "Summary unavailable; compacted conservatively.".to_string());
            // Validate: require at least 3 of the expected ## headers.
            let required_headers = ["## Decisions", "## Open TODOs", "## Constraints", "## Pending", "## Exact", "## Progress", "## Critical Context"];
            let found_count = required_headers.iter().filter(|h| raw.contains(*h)).count();
            let validated = if found_count < 3 {
                tracing::warn!(
                    found_count,
                    "Compaction summary missing required sections; retrying with explicit prompt"
                );
                let retry_prompt = format!(
                    "The previous summary was missing required sections. \
                     You MUST produce a summary with these exact ## headers: \
                     ## Decisions, ## Open TODOs, ## Constraints/Rules, ## Pending user asks, \
                     ## Exact identifiers, ## Progress, ## Critical Context. \
                     Do not skip any. Conversation:\n{}",
                    transcript
                );
                provider
                    .chat_with_system(
                        Some("You are a context compaction summarizer. Always include all required ## sections."),
                        &retry_prompt,
                        model,
                        0.1,
                    )
                    .await
                    .unwrap_or(raw)
            } else {
                raw
            };
            truncate_with_ellipsis(&validated, COMPACTION_MAX_SUMMARY_CHARS)
        }
        crate::config::AgentCompactionMode::Aggressive => {
            let mut summary = format!(
                "Aggressive compaction removed {} older messages; retained {} recent messages.",
                compact_count, keep_recent
            );
            if let Some(last_user) = to_compact.iter().rev().find(|m| m.role == "user") {
                summary.push_str(" Last preserved user intent: ");
                summary.push_str(&truncate_with_ellipsis(&last_user.content, 240));
            }
            truncate_with_ellipsis(&summary, COMPACTION_MAX_SUMMARY_CHARS)
        }
        crate::config::AgentCompactionMode::Off => return Ok(false),
    };

    history.splice(
        start..compact_end,
        std::iter::once(ChatMessage::assistant(format!(
            "[Context compacted at {timestamp}. Summary: {summary}]"
        ))),
    );

    // P1-1: Inject post-compaction context refresh so the model knows critical
    // rules may have been summarized and should re-read workspace instructions.
    history.push(ChatMessage::user(
        "[Post-compaction context refresh]\n\
         Session was just compacted. Critical rules may have been summarized.\n\
         Re-read workspace instructions if available."
            .to_string(),
    ));

    Ok(true)
}

/// Extract key facts from messages about to be compacted and store them in memory.
/// This prevents permanent loss of important context during auto-compaction.
/// Failures are soft — compaction must not be blocked by a flush error.
async fn pre_compaction_flush(
    messages_to_compact: &[ChatMessage],
    mem: &dyn Memory,
    provider: &dyn Provider,
    model: &str,
) -> anyhow::Result<()> {
    // Build a transcript of user/assistant turns only
    let transcript: String = messages_to_compact
        .iter()
        .filter(|m| m.role == "user" || m.role == "assistant")
        .map(|m| {
            let snippet = &m.content[..m.content.len().min(500)];
            format!("[{}]: {}", m.role, snippet)
        })
        .collect::<Vec<_>>()
        .join("\n");

    if transcript.trim().is_empty() {
        return Ok(());
    }

    let extract_prompt = format!(
        "Extract key facts, decisions, preferences, and unresolved tasks from this conversation. \
         Output as a concise bullet list (max 8 items). Only include durable information worth \
         remembering long-term. If nothing is worth remembering, output 'NONE'.\n\n{transcript}"
    );

    let extraction = provider
        .chat_with_system(
            Some("You extract key facts from conversations for long-term memory storage."),
            &extract_prompt,
            model,
            0.0,
        )
        .await;

    if let Ok(text) = extraction {
        let text = text.trim().to_string();
        if !text.is_empty() && text.to_uppercase() != "NONE" {
            let date = chrono::Local::now().format("%Y-%m-%d_%H-%M");
            let key = format!("compaction_flush_{date}");
            mem.store(&key, &text, MemoryCategory::Conversation, None)
                .await?;
            tracing::info!(
                chars = text.len(),
                key = %key,
                "Pre-compaction flush: key facts saved to memory"
            );
        }
    }

    Ok(())
}

async fn auto_compact_history(
    history: &mut Vec<ChatMessage>,
    provider: &dyn Provider,
    model: &str,
    max_history: usize,
    mem: &dyn Memory,
) -> Result<bool> {
    let has_system = history.first().map_or(false, |m| m.role == "system");
    let non_system_count = if has_system {
        history.len().saturating_sub(1)
    } else {
        history.len()
    };

    if non_system_count <= max_history {
        return Ok(false);
    }

    let start = if has_system { 1 } else { 0 };
    let keep_recent = COMPACTION_KEEP_RECENT_MESSAGES.min(non_system_count);
    let compact_count = non_system_count.saturating_sub(keep_recent);
    if compact_count == 0 {
        return Ok(false);
    }

    let compact_end = start + compact_count;
    let to_compact: Vec<ChatMessage> = history[start..compact_end].to_vec();

    // Pre-compaction flush: extract and persist key facts before they are lost.
    pre_compaction_flush(&to_compact, mem, provider, model)
        .await
        .ok(); // soft failure — never block compaction

    let transcript = build_compaction_transcript(&to_compact);

    let summarizer_system = "You are a conversation compaction engine. Summarize older chat history into concise context for future turns. Preserve: user preferences, commitments, decisions, unresolved tasks, key facts. Omit: filler, repeated chit-chat, verbose tool logs. Output plain text bullet points only.";

    let summarizer_user = format!(
        "Summarize the following conversation history for context preservation. Keep it short (max 12 bullet points).\n\n{}",
        transcript
    );

    let summary_raw = provider
        .chat_with_system(Some(summarizer_system), &summarizer_user, model, 0.2)
        .await
        .unwrap_or_else(|_| {
            // Fallback to deterministic local truncation when summarization fails.
            truncate_with_ellipsis(&transcript, COMPACTION_MAX_SUMMARY_CHARS)
        });

    let summary = truncate_with_ellipsis(&summary_raw, COMPACTION_MAX_SUMMARY_CHARS);
    apply_compaction_summary(history, start, compact_end, &summary);

    Ok(true)
}

/// Build context preamble by searching memory for relevant entries.
/// Entries with a hybrid score below `min_relevance_score` are dropped to
/// prevent unrelated memories from bleeding into the conversation.
async fn build_context(
    mem: &dyn Memory,
    user_msg: &str,
    min_relevance_score: f64,
) -> RecalledMemoryContext {
    let mut context = String::new();
    let mut ids = Vec::new();

    // Pull relevant memories for this message
    if let Ok(entries) = mem.recall(user_msg, 5, None).await {
        let relevant: Vec<_> = entries
            .iter()
            .filter(|e| match e.score {
                Some(score) => score >= min_relevance_score,
                None => true,
            })
            .collect();

        if !relevant.is_empty() {
            context.push_str("[Memory context]\n");
            for entry in &relevant {
                if memory::is_assistant_autosave_key(&entry.key) {
                    continue;
                }
                ids.push(entry.id.clone());
                let _ = writeln!(context, "- {}: {}", entry.key, entry.content);
            }
            if context != "[Memory context]\n" {
                context.push('\n');
            } else {
                context.clear();
            }
        }
    }

    RecalledMemoryContext {
        preamble: context,
        ids,
    }
}

async fn increment_recalled_useful_counts(mem: &dyn Memory, recalled_ids: &[String]) {
    let unique_ids: BTreeSet<&str> = recalled_ids.iter().map(String::as_str).collect();
    for id in unique_ids {
        if let Err(error) = mem.increment_useful_count(id).await {
            tracing::debug!(memory_id = id, error = %error, "failed to increment useful_count");
        }
    }
}

async fn select_prompt_skills(
    query: &str,
    skills: &[crate::skills::Skill],
    config: &Config,
    embedder: &dyn crate::memory::embeddings::EmbeddingProvider,
) -> Vec<crate::skills::Skill> {
    if !config.skill_rag.enabled {
        return skills.to_vec();
    }

    crate::skills::select_skills_by_relevance(query, skills, config.skill_rag.top_k, embedder).await
}

fn build_runtime_system_prompt(
    config: &Config,
    model_name: &str,
    tool_descs: &[(&str, &str)],
    skills: &[crate::skills::Skill],
    native_tools: bool,
    tools_registry: &[Box<dyn Tool>],
) -> String {
    let bootstrap_max_chars = if config.agent.compact_context {
        Some(6000)
    } else {
        None
    };
    let mut system_prompt = crate::channels::build_system_prompt_with_mode(
        &config.workspace_dir,
        model_name,
        tool_descs,
        skills,
        Some(&config.identity),
        bootstrap_max_chars,
        native_tools,
    );

    if !native_tools {
        system_prompt.push_str(&build_tool_instructions(tools_registry));
    }

    system_prompt
}

/// Find a tool by name in the registry.
fn find_tool<'a>(tools: &'a [Box<dyn Tool>], name: &str) -> Option<&'a dyn Tool> {
    tools
        .iter()
        .find(|t| t.supports_name(name))
        .map(|t| t.as_ref())
}

fn parse_arguments_value(raw: Option<&serde_json::Value>) -> serde_json::Value {
    match raw {
        Some(serde_json::Value::String(s)) => serde_json::from_str::<serde_json::Value>(s)
            .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new())),
        Some(value) => value.clone(),
        None => serde_json::Value::Object(serde_json::Map::new()),
    }
}

fn parse_tool_call_value(value: &serde_json::Value) -> Option<ParsedToolCall> {
    if let Some(function) = value.get("function") {
        let name = function
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if !name.is_empty() {
            let arguments = parse_arguments_value(
                function
                    .get("arguments")
                    .or_else(|| function.get("parameters")),
            );
            return Some(ParsedToolCall { name, arguments });
        }
    }

    let name = value
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();

    if name.is_empty() {
        return None;
    }

    let arguments =
        parse_arguments_value(value.get("arguments").or_else(|| value.get("parameters")));
    Some(ParsedToolCall { name, arguments })
}

fn parse_tool_calls_from_json_value(value: &serde_json::Value) -> Vec<ParsedToolCall> {
    let mut calls = Vec::new();

    if let Some(tool_calls) = value.get("tool_calls").and_then(|v| v.as_array()) {
        for call in tool_calls {
            if let Some(parsed) = parse_tool_call_value(call) {
                calls.push(parsed);
            }
        }

        if !calls.is_empty() {
            return calls;
        }
    }

    if let Some(array) = value.as_array() {
        for item in array {
            if let Some(parsed) = parse_tool_call_value(item) {
                calls.push(parsed);
            }
        }
        return calls;
    }

    if let Some(parsed) = parse_tool_call_value(value) {
        calls.push(parsed);
    }

    calls
}

fn tool_call_close_tag_for_name(name: &str) -> Option<&'static str> {
    match name {
        "tool_call" => Some("</tool_call>"),
        "toolcall" => Some("</toolcall>"),
        "tool-call" => Some("</tool-call>"),
        "tool_use" => Some("</tool_use>"),
        "invoke" => Some("</invoke>"),
        _ => None,
    }
}

fn parse_tool_call_open_tag(tag: &str) -> Option<&'static str> {
    if !(tag.starts_with('<') && tag.ends_with('>')) {
        return None;
    }

    let mut inner = tag[1..tag.len() - 1].trim_start();
    if inner.starts_with('/') {
        return None;
    }
    if inner.ends_with('/') {
        inner = inner[..inner.len().saturating_sub(1)].trim_end();
    }

    let name = inner
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    tool_call_close_tag_for_name(&name)
}

fn find_first_tool_call_open_tag(haystack: &str) -> Option<(usize, usize, &'static str)> {
    let mut search_from = 0usize;
    while search_from < haystack.len() {
        let rel_lt = haystack[search_from..].find('<')?;
        let start = search_from + rel_lt;
        let rel_gt = haystack[start..].find('>')?;
        let end_exclusive = start + rel_gt + 1;
        let tag = &haystack[start..end_exclusive];
        if let Some(close_tag) = parse_tool_call_open_tag(tag) {
            return Some((start, end_exclusive, close_tag));
        }
        search_from = end_exclusive;
    }
    None
}

fn extract_first_json_value_with_end(input: &str) -> Option<(serde_json::Value, usize)> {
    let trimmed = input.trim_start();
    let trim_offset = input.len().saturating_sub(trimmed.len());

    for (byte_idx, ch) in trimmed.char_indices() {
        if ch != '{' && ch != '[' {
            continue;
        }

        let slice = &trimmed[byte_idx..];
        let mut stream = serde_json::Deserializer::from_str(slice).into_iter::<serde_json::Value>();
        if let Some(Ok(value)) = stream.next() {
            let consumed = stream.byte_offset();
            if consumed > 0 {
                return Some((value, trim_offset + byte_idx + consumed));
            }
        }
    }

    None
}

fn strip_leading_close_tags(mut input: &str) -> &str {
    loop {
        let trimmed = input.trim_start();
        if !trimmed.starts_with("</") {
            return trimmed;
        }

        let Some(close_end) = trimmed.find('>') else {
            return "";
        };
        input = &trimmed[close_end + 1..];
    }
}

/// Extract JSON values from a string.
///
/// # Security Warning
///
/// This function extracts ANY JSON objects/arrays from the input. It MUST only
/// be used on content that is already trusted to be from the LLM, such as
/// content inside `<invoke>` tags where the LLM has explicitly indicated intent
/// to make a tool call. Do NOT use this on raw user input or content that
/// could contain prompt injection payloads.
fn extract_json_values(input: &str) -> Vec<serde_json::Value> {
    let mut values = Vec::new();
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return values;
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        values.push(value);
        return values;
    }

    let char_positions: Vec<(usize, char)> = trimmed.char_indices().collect();
    let mut idx = 0;
    while idx < char_positions.len() {
        let (byte_idx, ch) = char_positions[idx];
        if ch == '{' || ch == '[' {
            let slice = &trimmed[byte_idx..];
            let mut stream =
                serde_json::Deserializer::from_str(slice).into_iter::<serde_json::Value>();
            if let Some(Ok(value)) = stream.next() {
                let consumed = stream.byte_offset();
                if consumed > 0 {
                    values.push(value);
                    let next_byte = byte_idx + consumed;
                    while idx < char_positions.len() && char_positions[idx].0 < next_byte {
                        idx += 1;
                    }
                    continue;
                }
            }
        }
        idx += 1;
    }

    values
}

/// Find the end position of a JSON object by tracking balanced braces.
fn find_json_end(input: &str) -> Option<usize> {
    let trimmed = input.trim_start();
    let offset = input.len() - trimmed.len();

    if !trimmed.starts_with('{') {
        return None;
    }

    let mut depth = 0;
    let mut in_string = false;
    let mut escape_next = false;

    for (i, ch) in trimmed.char_indices() {
        if escape_next {
            escape_next = false;
            continue;
        }

        match ch {
            '\\' if in_string => escape_next = true,
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(offset + i + ch.len_utf8());
                }
            }
            _ => {}
        }
    }

    None
}

/// Parse GLM-style tool calls from response text.
/// GLM uses proprietary formats like:
/// - `browser_open/url>https://example.com`
/// - `shell/command>ls -la`
/// - `http_request/url>https://api.example.com`
fn map_glm_tool_alias(tool_name: &str) -> &str {
    match tool_name {
        "browser_open" | "browser" | "web_search" | "shell" | "bash" => "shell",
        "http_request" | "http" => "http_request",
        _ => tool_name,
    }
}

fn build_curl_command(url: &str) -> Option<String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }

    if url.chars().any(char::is_whitespace) {
        return None;
    }

    let escaped = url.replace('\'', r#"'\\''"#);
    Some(format!("curl -s '{}'", escaped))
}

fn parse_glm_style_tool_calls(text: &str) -> Vec<(String, serde_json::Value, Option<String>)> {
    let mut calls = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Format: tool_name/param>value or tool_name/{json}
        if let Some(pos) = line.find('/') {
            let tool_part = &line[..pos];
            let rest = &line[pos + 1..];

            if tool_part.chars().all(|c| c.is_alphanumeric() || c == '_') {
                let tool_name = map_glm_tool_alias(tool_part);

                if let Some(gt_pos) = rest.find('>') {
                    let param_name = rest[..gt_pos].trim();
                    let value = rest[gt_pos + 1..].trim();

                    let arguments = match tool_name {
                        "shell" => {
                            if param_name == "url" {
                                let Some(command) = build_curl_command(value) else {
                                    continue;
                                };
                                serde_json::json!({"command": command})
                            } else if value.starts_with("http://") || value.starts_with("https://")
                            {
                                if let Some(command) = build_curl_command(value) {
                                    serde_json::json!({"command": command})
                                } else {
                                    serde_json::json!({"command": value})
                                }
                            } else {
                                serde_json::json!({"command": value})
                            }
                        }
                        "http_request" => {
                            serde_json::json!({"url": value, "method": "GET"})
                        }
                        _ => serde_json::json!({param_name: value}),
                    };

                    calls.push((tool_name.to_string(), arguments, Some(line.to_string())));
                    continue;
                }

                if rest.starts_with('{') {
                    if let Ok(json_args) = serde_json::from_str::<serde_json::Value>(rest) {
                        calls.push((tool_name.to_string(), json_args, Some(line.to_string())));
                    }
                }
            }
        }

        // Plain URL
        if let Some(command) = build_curl_command(line) {
            calls.push((
                "shell".to_string(),
                serde_json::json!({"command": command}),
                Some(line.to_string()),
            ));
        }
    }

    calls
}

fn strip_markdown_fence_block(input: &str) -> String {
    let trimmed = input.trim();
    if !trimmed.starts_with("```") {
        return trimmed.to_string();
    }

    let Some(first_newline) = trimmed.find('\n') else {
        return trimmed.to_string();
    };
    let content = &trimmed[first_newline + 1..];
    let content = if let Some(end) = content.rfind("```") {
        &content[..end]
    } else {
        content
    };
    content.trim().to_string()
}

fn parse_recipient_tool_calls(recipient: &str, body: &str) -> Vec<ParsedToolCall> {
    let mut calls = Vec::new();
    let recipient = recipient.trim();
    if recipient.is_empty() {
        return calls;
    }

    for value in extract_json_values(body) {
        let parsed_calls = parse_tool_calls_from_json_value(&value);
        if !parsed_calls.is_empty() {
            calls.extend(parsed_calls);
            continue;
        }

        if value.is_object() {
            calls.push(ParsedToolCall {
                name: recipient.to_string(),
                arguments: value,
            });
        }
    }

    if !calls.is_empty() {
        return calls;
    }

    let cleaned_body = strip_markdown_fence_block(body);
    if cleaned_body.is_empty() {
        return calls;
    }

    if recipient == "shell" || recipient == "bash" {
        calls.push(ParsedToolCall {
            name: "shell".to_string(),
            arguments: serde_json::json!({ "command": cleaned_body }),
        });
    }

    calls
}

fn parse_codex_to_style_tool_calls(text: &str) -> (String, Vec<ParsedToolCall>) {
    static CODEX_TO_HEADER_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)^\s*(?:assistant\s+)?to=([a-zA-Z0-9_.-]+)\s+code\s*$"#).expect("compile regex: codex to=recipient header")
    });

    let mut found_header = false;
    let mut calls = Vec::new();
    let mut text_parts = Vec::new();
    let mut plain_text = String::new();
    let mut current_recipient: Option<String> = None;
    let mut current_body = String::new();

    let flush_current =
        |recipient: &mut Option<String>, body: &mut String, out_calls: &mut Vec<ParsedToolCall>| {
            if let Some(recipient) = recipient.take() {
                out_calls.extend(parse_recipient_tool_calls(&recipient, body));
                body.clear();
            }
        };

    for line in text.lines() {
        if let Some(cap) = CODEX_TO_HEADER_RE.captures(line) {
            found_header = true;
            flush_current(&mut current_recipient, &mut current_body, &mut calls);
            current_recipient = cap.get(1).map(|m| m.as_str().to_string());
            continue;
        }

        if current_recipient.is_some() {
            if !current_body.is_empty() {
                current_body.push('\n');
            }
            current_body.push_str(line);
        } else {
            if !plain_text.is_empty() {
                plain_text.push('\n');
            }
            plain_text.push_str(line);
        }
    }
    flush_current(&mut current_recipient, &mut current_body, &mut calls);

    if !calls.is_empty() {
        if !plain_text.trim().is_empty() {
            text_parts.push(plain_text.trim().to_string());
        }
    } else if !found_header && !text.trim().is_empty() {
        text_parts.push(text.trim().to_string());
    } else if found_header {
        text_parts.push(text.trim().to_string());
    }

    (text_parts.join("\n"), calls)
}

fn parse_assistant_recipient_tool_calls(text: &str) -> (String, Vec<ParsedToolCall>) {
    static ASSISTANT_RECIPIENT_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(?is)<assistant\b([^>]*)>(.*?)</assistant>"#).expect("compile regex: assistant XML tag pattern"));
    static RECIPIENT_ATTR_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)\brecipient\s*=\s*["']?([a-zA-Z0-9_.-]+)["']?"#).expect("compile regex: recipient attribute pattern")
    });

    let mut calls = Vec::new();
    let mut text_parts = Vec::new();
    let mut last_end = 0usize;

    for cap in ASSISTANT_RECIPIENT_RE.captures_iter(text) {
        let Some(full_match) = cap.get(0) else {
            continue;
        };
        let before = &text[last_end..full_match.start()];
        if !before.trim().is_empty() {
            text_parts.push(before.trim().to_string());
        }

        let attrs = cap.get(1).map_or("", |m| m.as_str());
        let body = cap.get(2).map_or("", |m| m.as_str());
        if let Some(recipient_match) = RECIPIENT_ATTR_RE.captures(attrs) {
            if let Some(recipient) = recipient_match.get(1).map(|m| m.as_str()) {
                calls.extend(parse_recipient_tool_calls(recipient, body));
            }
        }

        last_end = full_match.end();
    }

    if !calls.is_empty() {
        let tail = &text[last_end..];
        if !tail.trim().is_empty() {
            text_parts.push(tail.trim().to_string());
        }
    } else if !text.trim().is_empty() {
        text_parts.push(text.trim().to_string());
    }

    (text_parts.join("\n"), calls)
}

fn looks_like_unparsed_tool_call_syntax(text: &str) -> bool {
    static COMPLETE_TOOL_SYNTAX: LazyLock<RegexSet> = LazyLock::new(|| {
        RegexSet::new([
            r"(?is)<tool[_-]?call\b[^>]*>\s*(?:\{[\s\S]*?\}|\[[\s\S]*?\])\s*</tool[_-]?call>",
            r"(?is)<toolcall\b[^>]*>\s*(?:\{[\s\S]*?\}|\[[\s\S]*?\])\s*</toolcall>",
            r"(?is)<tool_use\b[^>]*>\s*(?:\{[\s\S]*?\}|\[[\s\S]*?\])\s*</tool_use>",
            r"(?is)<invoke\b[^>]*>\s*(?:\{[\s\S]*?\}|\[[\s\S]*?\])\s*</invoke>",
            r#"(?is)<assistant\b[^>]*\brecipient\s*=\s*["']?[a-zA-Z0-9_.-]+["']?[^>]*>\s*(?:\{[\s\S]*?\}|\[[\s\S]*?\]|```[\s\S]*?```)\s*</assistant>"#,
            r"(?is)(?:^|\n)\s*(?:assistant\s+)?to=[a-zA-Z0-9_.-]+\s+code\s*(?:\r?\n)+\s*(?:```(?:json|tool[_-]?call|toolcall|invoke)?\s*(?:\r?\n)+)?\s*(?:\{|\[)",
        ])
        .expect("compile regex set: complete tool syntax patterns")
    });
    COMPLETE_TOOL_SYNTAX.is_match(text)
}

// ── Tool-Call Parsing ─────────────────────────────────────────────────────
// LLM responses may contain tool calls in multiple formats depending on
// the provider. Parsing follows a priority chain:
//   1. OpenAI-style JSON with `tool_calls` array (native API)
//   2. XML tags: <tool_call>, <toolcall>, <tool-call>, <tool_use>, <invoke>
//   3. Markdown code blocks with `tool_call` language
//   4. GLM-style line-based format (e.g. `shell/command>ls`)
// SECURITY: We never fall back to extracting arbitrary JSON from the
// response body, because that would enable prompt-injection attacks where
// malicious content in emails/files/web pages mimics a tool call.

/// Parse tool calls from an LLM response that uses XML-style function calling.
///
/// Expected format (common with system-prompt-guided tool use):
/// ```text
/// <tool_call>
/// {"name": "shell", "arguments": {"command": "ls"}}
/// </tool_call>
/// ```
///
/// Also accepts common tag variants (`<toolcall>`, `<tool-call>`) for model
/// compatibility.
///
/// Also supports JSON with `tool_calls` array from OpenAI-format responses.
fn parse_tool_calls(response: &str) -> (String, Vec<ParsedToolCall>) {
    let mut text_parts = Vec::new();
    let mut calls = Vec::new();
    let mut remaining = response;

    // First, try to parse as OpenAI-style JSON response with tool_calls array
    // This handles providers like Minimax that return tool_calls in native JSON format
    if let Ok(json_value) = serde_json::from_str::<serde_json::Value>(response.trim()) {
        calls = parse_tool_calls_from_json_value(&json_value);
        if !calls.is_empty() {
            // If we found tool_calls, extract any content field as text
            if let Some(content) = json_value.get("content").and_then(|v| v.as_str()) {
                if !content.trim().is_empty() {
                    text_parts.push(content.trim().to_string());
                }
            }
            return (text_parts.join("\n"), calls);
        }
    }

    // Fall back to XML-style tool-call tag parsing.
    while let Some((start, open_end, close_tag)) = find_first_tool_call_open_tag(remaining) {
        // Everything before the tag is text
        let before = &remaining[..start];
        if !before.trim().is_empty() {
            text_parts.push(before.trim().to_string());
        }

        let after_open = &remaining[open_end..];
        if let Some(close_idx) = after_open.find(close_tag) {
            let inner = &after_open[..close_idx];
            let mut parsed_any = false;
            let json_values = extract_json_values(inner);
            for value in json_values {
                let parsed_calls = parse_tool_calls_from_json_value(&value);
                if !parsed_calls.is_empty() {
                    parsed_any = true;
                    calls.extend(parsed_calls);
                }
            }

            if !parsed_any {
                let sanitized_preview = sanitize_tool_parse_log_preview(inner);
                tracing::error!(
                    raw_tool_call_body = sanitized_preview.as_str(),
                    "Malformed <tool_call> JSON: expected tool-call object in tag body"
                );
            }

            remaining = &after_open[close_idx + close_tag.len()..];
        } else {
            if let Some(json_end) = find_json_end(after_open) {
                if let Ok(value) =
                    serde_json::from_str::<serde_json::Value>(&after_open[..json_end])
                {
                    let parsed_calls = parse_tool_calls_from_json_value(&value);
                    if !parsed_calls.is_empty() {
                        calls.extend(parsed_calls);
                        remaining = strip_leading_close_tags(&after_open[json_end..]);
                        continue;
                    }
                }
            }

            if let Some((value, consumed_end)) = extract_first_json_value_with_end(after_open) {
                let parsed_calls = parse_tool_calls_from_json_value(&value);
                if !parsed_calls.is_empty() {
                    calls.extend(parsed_calls);
                    remaining = strip_leading_close_tags(&after_open[consumed_end..]);
                    continue;
                }
            }

            remaining = &remaining[start..];
            break;
        }
    }

    // If XML tags found nothing, try markdown code blocks with tool_call language.
    // Models behind OpenRouter sometimes output ```tool_call ... ``` or hybrid
    // ```tool_call ... </tool_call> instead of structured API calls or XML tags.
    if calls.is_empty() {
        static MD_TOOL_CALL_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(
                r"(?s)```(?:tool[_-]?call|invoke)\s*\n(.*?)(?:```|</tool[_-]?call>|</toolcall>|</tool_use>|</invoke>)",
            )
            .expect("compile regex: markdown tool_call block pattern")
        });
        let mut md_text_parts: Vec<String> = Vec::new();
        let mut last_end = 0;

        for cap in MD_TOOL_CALL_RE.captures_iter(response) {
            let full_match = cap.get(0).expect("regex capture group 0 always exists");
            let before = &response[last_end..full_match.start()];
            if !before.trim().is_empty() {
                md_text_parts.push(before.trim().to_string());
            }
            let inner = &cap[1];
            let json_values = extract_json_values(inner);
            for value in json_values {
                let parsed_calls = parse_tool_calls_from_json_value(&value);
                calls.extend(parsed_calls);
            }
            last_end = full_match.end();
        }

        if !calls.is_empty() {
            let after = &response[last_end..];
            if !after.trim().is_empty() {
                md_text_parts.push(after.trim().to_string());
            }
            text_parts = md_text_parts;
            remaining = "";
        }
    }

    // OpenAI Codex text protocol: `assistant to=<tool> code ...`
    if calls.is_empty() {
        let (parsed_text, parsed_calls) = parse_codex_to_style_tool_calls(response);
        if !parsed_calls.is_empty() {
            calls = parsed_calls;
            if !parsed_text.is_empty() {
                text_parts = vec![parsed_text];
            } else {
                text_parts.clear();
            }
            remaining = "";
        }
    }

    // Assistant-recipient XML protocol: <assistant recipient="tool">...</assistant>
    if calls.is_empty() {
        let (parsed_text, parsed_calls) = parse_assistant_recipient_tool_calls(response);
        if !parsed_calls.is_empty() {
            calls = parsed_calls;
            if !parsed_text.is_empty() {
                text_parts = vec![parsed_text];
            } else {
                text_parts.clear();
            }
            remaining = "";
        }
    }

    // GLM-style tool calls (browser_open/url>https://..., shell/command>ls, etc.)
    if calls.is_empty() {
        let glm_calls = parse_glm_style_tool_calls(remaining);
        if !glm_calls.is_empty() {
            let mut cleaned_text = remaining.to_string();
            for (name, args, raw) in &glm_calls {
                calls.push(ParsedToolCall {
                    name: name.clone(),
                    arguments: args.clone(),
                });
                if let Some(r) = raw {
                    cleaned_text = cleaned_text.replace(r, "");
                }
            }
            if !cleaned_text.trim().is_empty() {
                text_parts.push(cleaned_text.trim().to_string());
            }
            remaining = "";
        }
    }

    // SECURITY: We do NOT fall back to extracting arbitrary JSON from the response
    // here. That would enable prompt injection attacks where malicious content
    // (e.g., in emails, files, or web pages) could include JSON that mimics a
    // tool call. Tool calls MUST be explicitly wrapped in either:
    // 1. OpenAI-style JSON with a "tool_calls" array
    // 2. OpenPRX tool-call tags (<tool_call>, <toolcall>, <tool-call>)
    // 3. Markdown code blocks with tool_call/toolcall/tool-call language
    // 4. Explicit GLM line-based call formats (e.g. `shell/command>...`)
    // This ensures only the LLM's intentional tool calls are executed.

    // Remaining text after last tool call
    if !remaining.trim().is_empty() {
        text_parts.push(remaining.trim().to_string());
    }

    (text_parts.join("\n"), calls)
}

fn parse_structured_tool_calls(tool_calls: &[ToolCall]) -> Vec<ParsedToolCall> {
    tool_calls
        .iter()
        .map(|call| ParsedToolCall {
            name: call.name.clone(),
            arguments: serde_json::from_str::<serde_json::Value>(&call.arguments)
                .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new())),
        })
        .collect()
}

/// Build assistant history entry in JSON format for native tool-call APIs.
/// `convert_messages` in the OpenRouter provider parses this JSON to reconstruct
/// the proper `NativeMessage` with structured `tool_calls`.
fn build_native_assistant_history(text: &str, tool_calls: &[ToolCall]) -> String {
    let calls_json: Vec<serde_json::Value> = tool_calls
        .iter()
        .map(|tc| {
            serde_json::json!({
                "id": tc.id,
                "name": tc.name,
                "arguments": tc.arguments,
            })
        })
        .collect();

    let content = if text.trim().is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::String(text.trim().to_string())
    };

    serde_json::json!({
        "content": content,
        "tool_calls": calls_json,
    })
    .to_string()
}

fn build_assistant_history_with_tool_calls(text: &str, tool_calls: &[ToolCall]) -> String {
    let mut parts = Vec::new();

    if !text.trim().is_empty() {
        parts.push(text.trim().to_string());
    }

    for call in tool_calls {
        let arguments = serde_json::from_str::<serde_json::Value>(&call.arguments)
            .unwrap_or_else(|_| serde_json::Value::String(call.arguments.clone()));
        let payload = serde_json::json!({
            "id": call.id,
            "name": call.name,
            "arguments": arguments,
        });
        parts.push(format!("<tool_call>\n{payload}\n</tool_call>"));
    }

    parts.join("\n")
}

#[derive(Debug, Clone)]
struct ParsedToolCall {
    name: String,
    arguments: serde_json::Value,
}

#[derive(Debug)]
pub(crate) struct ToolLoopCancelled;

impl std::fmt::Display for ToolLoopCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("tool loop cancelled")
    }
}

impl std::error::Error for ToolLoopCancelled {}

pub(crate) fn is_tool_loop_cancelled(err: &anyhow::Error) -> bool {
    err.chain().any(|source| source.is::<ToolLoopCancelled>())
}

static TOOL_BARRIERS: LazyLock<
    parking_lot::Mutex<HashMap<&'static str, Arc<tokio::sync::Mutex<()>>>>,
> = LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));

fn tool_barrier_key(name: &str) -> Option<&'static str> {
    match name {
        // Workspace mutations / command execution.
        "file_write" | "shell" | "git_operations" => Some("workspace_write"),
        // Shared runtime configuration / scheduler state.
        "config_reload" | "cron" | "cron_add" | "cron_update" | "cron_remove" | "cron_run"
        | "schedule" | "proxy_config" => Some("runtime_config"),
        // Shared long-term memory writes.
        "memory_store" | "memory_forget" => Some("memory_write"),
        // Background session lifecycle operations.
        "sessions_spawn" | "sessions_send" | "delegate" | "subagents" => Some("session_lifecycle"),
        _ => None,
    }
}

async fn acquire_tool_barrier(key: &'static str) -> tokio::sync::OwnedMutexGuard<()> {
    let barrier = {
        let mut barriers = TOOL_BARRIERS.lock();
        barriers
            .entry(key)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };

    barrier.lock_owned().await
}

/// Execute a single turn of the agent loop: send messages, parse tool calls,
/// execute tools, and loop until the LLM produces a final text response.
/// When `silent` is true, suppresses stdout (for channel use).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn agent_turn(
    provider: &dyn Provider,
    history: &mut Vec<ChatMessage>,
    tools_registry: &[Box<dyn Tool>],
    observer: &dyn Observer,
    hooks: &HookManager,
    provider_name: &str,
    model: &str,
    temperature: f64,
    silent: bool,
    multimodal_config: &crate::config::MultimodalConfig,
    max_tool_iterations: usize,
    parallel_tools_enabled: bool,
    read_only_tool_concurrency_window: usize,
    read_only_tool_timeout_secs: u64,
    priority_scheduling_enabled: bool,
    low_priority_tool_names: Vec<String>,
    concurrency_governance: ToolConcurrencyGovernanceConfig,
) -> Result<String> {
    run_tool_call_loop(
        provider,
        history,
        tools_registry,
        observer,
        hooks,
        provider_name,
        model,
        temperature,
        silent,
        None,
        "channel",
        multimodal_config,
        max_tool_iterations,
        parallel_tools_enabled,
        read_only_tool_concurrency_window,
        read_only_tool_timeout_secs,
        priority_scheduling_enabled,
        low_priority_tool_names,
        concurrency_governance,
        None,
        None,
        None,
        None,
    )
    .await
}

async fn execute_one_tool(
    call_name: &str,
    mut call_arguments: serde_json::Value,
    tools_registry: &[Box<dyn Tool>],
    observer: &dyn Observer,
    cancellation_token: Option<&CancellationToken>,
    scope_ctx: Option<&ScopeContext<'_>>,
) -> Result<String> {
    let Some(tool) = find_tool(tools_registry, call_name) else {
        return Ok(format!("Unknown tool: {call_name}"));
    };

    let root = call_arguments
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("tool arguments must be a JSON object"))?;
    if let Some(ctx) = scope_ctx {
        root.insert(
            "_zc_scope".to_string(),
            serde_json::json!({
                "sender": ctx.sender,
                "channel": ctx.channel,
                "chat_type": ctx.chat_type,
                "chat_id": ctx.chat_id,
            }),
        );
        root.insert(
            "_zc_scope_trusted".to_string(),
            serde_json::Value::Bool(true),
        );
    } else {
        // Never trust user-provided scope payloads when runtime has no trusted scope.
        root.remove("_zc_scope");
        root.insert(
            "_zc_scope_trusted".to_string(),
            serde_json::Value::Bool(false),
        );
    }

    let _barrier_guard = if let Some(key) = tool_barrier_key(call_name) {
        Some(acquire_tool_barrier(key).await)
    } else {
        None
    };

    observer.record_event(&ObserverEvent::ToolCallStart {
        tool: call_name.to_string(),
    });
    let start = Instant::now();

    let tool_future = tool.execute(call_arguments);
    let tool_result = if let Some(token) = cancellation_token {
        tokio::select! {
            () = token.cancelled() => return Err(ToolLoopCancelled.into()),
            result = tool_future => result,
        }
    } else {
        tool_future.await
    };

    match tool_result {
        Ok(r) => {
            observer.record_event(&ObserverEvent::ToolCall {
                tool: call_name.to_string(),
                duration: start.elapsed(),
                success: r.success,
            });
            if r.success {
                Ok(scrub_credentials(&r.output))
            } else {
                Ok(format!("Error: {}", r.error.unwrap_or_else(|| r.output)))
            }
        }
        Err(e) => {
            observer.record_event(&ObserverEvent::ToolCall {
                tool: call_name.to_string(),
                duration: start.elapsed(),
                success: false,
            });
            Ok(format!("Error executing {call_name}: {e}"))
        }
    }
}

/// Conservative scheduler policy:
/// - only known read-only tools can run concurrently;
/// - all stateful tools execute strictly serially;
/// - read-only batches use a bounded concurrency window and timeout guard.
#[derive(Debug, Clone)]
struct ReadOnlyToolScheduleConfig {
    parallel_enabled: bool,
    concurrency_window: usize,
    timeout_secs: u64,
    priority_enabled: bool,
    low_priority_tool_names: std::collections::HashSet<String>,
    rollout_stage: String,
    kill_switch_applied: bool,
    auto_rollback_enabled: bool,
    rollback_timeout_rate_threshold: f64,
    rollback_cancel_rate_threshold: f64,
    rollback_error_rate_threshold: f64,
}

#[derive(Debug, Clone)]
struct RolloutDecision {
    enabled: bool,
    stage: String,
    kill_switch_applied: bool,
    reason: &'static str,
}

#[derive(Debug, Clone)]
struct BatchExecutionOutcome {
    results: Vec<String>,
    total_calls: usize,
    timeout_count: usize,
    cancel_count: usize,
    error_count: usize,
}

fn rollout_stage_default_sample(stage: &str) -> u8 {
    match stage {
        "stage_a" => 5,
        "stage_b" => 25,
        "stage_c" => 50,
        "full" => 100,
        _ => 0,
    }
}

fn rollout_effective_sample_percent(stage: &str, configured_percent: u8) -> u8 {
    let stage_default = rollout_stage_default_sample(stage);
    if configured_percent == 0 {
        stage_default
    } else {
        configured_percent.min(stage_default.max(1))
    }
}

fn rollout_sampling_key(channel_name: &str, scope_ctx: Option<&ScopeContext<'_>>) -> String {
    if let Some(scope) = scope_ctx {
        format!(
            "{}:{}:{}:{}",
            channel_name, scope.channel, scope.sender, scope.chat_id
        )
    } else {
        channel_name.to_string()
    }
}

fn rollout_sample_selected(key: &str, sample_percent: u8) -> bool {
    if sample_percent >= 100 {
        return true;
    }
    if sample_percent == 0 {
        return false;
    }
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    (hasher.finish() % 100) < u64::from(sample_percent)
}

fn resolve_rollout_decision(
    parallel_tools_enabled: bool,
    governance: &ToolConcurrencyGovernanceConfig,
    channel_name: &str,
    scope_ctx: Option<&ScopeContext<'_>>,
) -> RolloutDecision {
    if !parallel_tools_enabled {
        return RolloutDecision {
            enabled: false,
            stage: "off".to_string(),
            kill_switch_applied: false,
            reason: "parallel_tools_disabled",
        };
    }
    if governance.kill_switch_force_serial {
        return RolloutDecision {
            enabled: false,
            stage: governance.rollout_stage.clone(),
            kill_switch_applied: true,
            reason: "kill_switch_force_serial",
        };
    }

    let stage = governance.rollout_stage.trim().to_ascii_lowercase();
    if stage == "off" {
        return RolloutDecision {
            enabled: false,
            stage,
            kill_switch_applied: false,
            reason: "rollout_off",
        };
    }

    if !governance.rollout_channels.is_empty()
        && !governance
            .rollout_channels
            .iter()
            .any(|name| name == channel_name)
    {
        return RolloutDecision {
            enabled: false,
            stage,
            kill_switch_applied: false,
            reason: "channel_not_in_rollout_allowlist",
        };
    }

    if stage == "full" {
        return RolloutDecision {
            enabled: true,
            stage,
            kill_switch_applied: false,
            reason: "full_rollout",
        };
    }

    let sample_percent =
        rollout_effective_sample_percent(&stage, governance.rollout_sample_percent).min(100);
    let selected = rollout_sample_selected(
        &rollout_sampling_key(channel_name, scope_ctx),
        sample_percent,
    );
    RolloutDecision {
        enabled: selected,
        stage,
        kill_switch_applied: false,
        reason: if selected {
            "sample_selected"
        } else {
            "sample_not_selected"
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolSchedulingClass {
    ReadOnly,
    Stateful,
}

fn classify_tool_call(name: &str) -> ToolSchedulingClass {
    if is_read_only_tool_name(name) {
        ToolSchedulingClass::ReadOnly
    } else {
        ToolSchedulingClass::Stateful
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolPriority {
    High,
    Low,
}

fn classify_tool_priority(name: &str, schedule: &ReadOnlyToolScheduleConfig) -> ToolPriority {
    if schedule.priority_enabled && schedule.low_priority_tool_names.contains(name) {
        ToolPriority::Low
    } else {
        ToolPriority::High
    }
}

fn is_read_only_tool_name(name: &str) -> bool {
    matches!(
        name,
        "file_read"
            | "memory_get"
            | "memory_recall"
            | "memory_search"
            | "web_fetch"
            | "web_search_tool"
            | "image_info"
            | "session_status"
            | "sessions_list"
            | "sessions_history"
            | "cron_list"
            | "cron_runs"
            | "agents_list"
            | "hardware_board_info"
            | "hardware_memory_map"
            | "hardware_memory_read"
    )
}

fn scope_or_pipeline_denial(
    call: &ParsedToolCall,
    scope_ctx: Option<&ScopeContext<'_>>,
) -> Option<String> {
    let ctx = scope_ctx?;
    if !ctx
        .policy
        .is_tool_allowed(&call.name, ctx.sender, ctx.channel, ctx.chat_type)
    {
        return Some(format!(
            "Error: Tool '{}' is not permitted for this user/channel context.",
            call.name
        ));
    }

    if let Some(pipeline) = ctx.policy_pipeline {
        let eval_ctx = crate::security::EvalContext {
            channel: ctx.channel.to_string(),
            chat_type: ctx.chat_type.to_string(),
            sender: ctx.sender.to_string(),
        };
        let decision = pipeline.evaluate(&call.name, &eval_ctx);
        if !decision.allowed {
            return Some(format!(
                "Error: Tool '{}' blocked by policy pipeline: {}",
                call.name, decision.reason
            ));
        }
    }

    None
}

async fn execute_tool_call_serial(
    call: &ParsedToolCall,
    tools_registry: &[Box<dyn Tool>],
    observer: &dyn Observer,
    approval: Option<&ApprovalManager>,
    channel_name: &str,
    cancellation_token: Option<&CancellationToken>,
    scope_ctx: Option<&ScopeContext<'_>>,
) -> Result<String> {
    if let Some(denied) = scope_or_pipeline_denial(call, scope_ctx) {
        return Ok(denied);
    }

    if let Some(mgr) = approval {
        if mgr.needs_approval(&call.name) {
            let request = ApprovalRequest {
                tool_name: call.name.clone(),
                arguments: call.arguments.clone(),
            };

            let decision = if channel_name == "cli" {
                mgr.prompt_cli(&request)
            } else {
                ApprovalResponse::No
            };

            mgr.record_decision(&call.name, &call.arguments, decision, channel_name);

            if decision == ApprovalResponse::No {
                return Ok("Denied by user.".to_string());
            }
        }
    }

    execute_one_tool(
        &call.name,
        call.arguments.clone(),
        tools_registry,
        observer,
        cancellation_token,
        scope_ctx,
    )
    .await
}

async fn execute_read_only_batch(
    calls: &[ParsedToolCall],
    tools_registry: &[Box<dyn Tool>],
    observer: &dyn Observer,
    schedule: ReadOnlyToolScheduleConfig,
    cancellation_token: Option<&CancellationToken>,
    scope_ctx: Option<&ScopeContext<'_>>,
) -> Result<BatchExecutionOutcome> {
    use futures::stream::{self, StreamExt};
    use std::time::Duration;

    let timeout = Duration::from_secs(schedule.timeout_secs);
    let batch_calls: Vec<ParsedToolCall> = calls.to_vec();

    let mut indexed_results: Vec<(usize, String)> =
        stream::iter(batch_calls.into_iter().enumerate())
            .map(|(idx, call)| async move {
                if let Some(denied) = scope_or_pipeline_denial(&call, scope_ctx) {
                    return Ok((idx, denied));
                }

                let execute_future = execute_one_tool(
                    &call.name,
                    call.arguments.clone(),
                    tools_registry,
                    observer,
                    cancellation_token,
                    scope_ctx,
                );

                let bounded = async {
                    match tokio::time::timeout(timeout, execute_future).await {
                        Ok(result) => result,
                        Err(_) => Ok(format!(
                            "Error: Tool '{}' timed out after {}s.",
                            call.name, schedule.timeout_secs
                        )),
                    }
                };

                let result = if let Some(token) = cancellation_token {
                    tokio::select! {
                        () = token.cancelled() => return Err(ToolLoopCancelled.into()),
                        bounded_result = bounded => bounded_result?,
                    }
                } else {
                    bounded.await?
                };

                Ok((idx, result))
            })
            .buffer_unordered(schedule.concurrency_window)
            .collect::<Vec<Result<(usize, String)>>>()
            .await
            .into_iter()
            .collect::<Result<Vec<(usize, String)>>>()?;

    indexed_results.sort_by_key(|(idx, _)| *idx);
    let results = indexed_results
        .into_iter()
        .map(|(_, result)| result)
        .collect::<Vec<String>>();
    let timeout_count = results
        .iter()
        .filter(|value| value.contains("timed out"))
        .count();
    let cancel_count = results
        .iter()
        .filter(|value| value.contains("cancelled") || value.contains("Cancelled"))
        .count();
    let error_count = results
        .iter()
        .filter(|value| value.starts_with("Error"))
        .count();

    Ok(BatchExecutionOutcome {
        total_calls: results.len(),
        results,
        timeout_count,
        cancel_count,
        error_count,
    })
}

async fn execute_tools_with_policy(
    tool_calls: &[ParsedToolCall],
    tools_registry: &[Box<dyn Tool>],
    observer: &dyn Observer,
    approval: Option<&ApprovalManager>,
    channel_name: &str,
    schedule: ReadOnlyToolScheduleConfig,
    cancellation_token: Option<&CancellationToken>,
    scope_ctx: Option<&ScopeContext<'_>>,
) -> Result<Vec<String>> {
    let mut ordered_indices = Vec::with_capacity(tool_calls.len());
    if schedule.priority_enabled {
        ordered_indices.extend(tool_calls.iter().enumerate().filter_map(|(idx, call)| {
            (classify_tool_priority(&call.name, &schedule) == ToolPriority::High).then_some(idx)
        }));
        ordered_indices.extend(tool_calls.iter().enumerate().filter_map(|(idx, call)| {
            (classify_tool_priority(&call.name, &schedule) == ToolPriority::Low).then_some(idx)
        }));
    } else {
        ordered_indices.extend(0..tool_calls.len());
    }

    let mut results_by_original = vec![String::new(); tool_calls.len()];
    let mut cursor = 0;
    let mut force_serial_for_remaining_turn = !schedule.parallel_enabled;

    while cursor < ordered_indices.len() {
        if cancellation_token.is_some_and(CancellationToken::is_cancelled) {
            return Err(ToolLoopCancelled.into());
        }

        let index = ordered_indices[cursor];
        let call = &tool_calls[index];
        let approval_required = approval.is_some_and(|mgr| mgr.needs_approval(&call.name));

        if classify_tool_call(&call.name) == ToolSchedulingClass::ReadOnly
            && !approval_required
            && !force_serial_for_remaining_turn
        {
            let batch_start = cursor;
            let mut batch_end = cursor + 1;
            while batch_end < ordered_indices.len() {
                let next_index = ordered_indices[batch_end];
                let next_call = &tool_calls[next_index];
                let next_approval = approval.is_some_and(|mgr| mgr.needs_approval(&next_call.name));
                if classify_tool_call(&next_call.name) != ToolSchedulingClass::ReadOnly
                    || next_approval
                {
                    break;
                }
                batch_end += 1;
            }

            let batch_calls: Vec<ParsedToolCall> = ordered_indices[batch_start..batch_end]
                .iter()
                .map(|idx| tool_calls[*idx].clone())
                .collect();
            let batch_outcome = execute_read_only_batch(
                &batch_calls,
                tools_registry,
                observer,
                schedule.clone(),
                cancellation_token,
                scope_ctx,
            )
            .await?;

            let total = batch_outcome.total_calls.max(1);
            let timeout_rate = batch_outcome.timeout_count as f64 / total as f64;
            let cancel_rate = batch_outcome.cancel_count as f64 / total as f64;
            let error_rate = batch_outcome.error_count as f64 / total as f64;
            let mut rollback_reason: Option<String> = None;
            let mut rollback_triggered = false;
            if schedule.auto_rollback_enabled {
                if timeout_rate > schedule.rollback_timeout_rate_threshold {
                    rollback_reason = Some("timeout_rate".to_string());
                    rollback_triggered = true;
                } else if cancel_rate > schedule.rollback_cancel_rate_threshold {
                    rollback_reason = Some("cancel_rate".to_string());
                    rollback_triggered = true;
                } else if error_rate > schedule.rollback_error_rate_threshold {
                    rollback_reason = Some("error_rate".to_string());
                    rollback_triggered = true;
                }
            }
            if rollback_triggered {
                force_serial_for_remaining_turn = true;
            }

            observer.record_event(&ObserverEvent::ToolBatch {
                rollout_stage: schedule.rollout_stage.clone(),
                batch_size: batch_outcome.total_calls,
                concurrency_window: schedule.concurrency_window,
                timeout_count: batch_outcome.timeout_count,
                cancel_count: batch_outcome.cancel_count,
                error_count: batch_outcome.error_count,
                degraded: force_serial_for_remaining_turn,
                rollback: rollback_triggered,
                rollback_reason: rollback_reason.clone(),
                kill_switch_applied: schedule.kill_switch_applied,
            });

            tracing::info!(
                rollout_stage = %schedule.rollout_stage,
                batch_size = batch_outcome.total_calls,
                concurrency_window = schedule.concurrency_window,
                timeout_count = batch_outcome.timeout_count,
                cancel_count = batch_outcome.cancel_count,
                error_count = batch_outcome.error_count,
                timeout_rate = timeout_rate,
                cancel_rate = cancel_rate,
                error_rate = error_rate,
                degraded = force_serial_for_remaining_turn,
                rollback = rollback_triggered,
                rollback_reason = ?rollback_reason,
                kill_switch_applied = schedule.kill_switch_applied,
                "tool batch execution"
            );

            for (offset, result) in batch_outcome.results.into_iter().enumerate() {
                let original_index = ordered_indices[batch_start + offset];
                results_by_original[original_index] = result;
            }
            cursor = batch_end;
            continue;
        }

        let result = execute_tool_call_serial(
            call,
            tools_registry,
            observer,
            approval,
            channel_name,
            cancellation_token,
            scope_ctx,
        )
        .await?;
        results_by_original[index] = result;
        cursor += 1;
    }

    Ok(results_by_original)
}

// ── Agent Tool-Call Loop ──────────────────────────────────────────────────
// Core agentic iteration: send conversation to the LLM, parse any tool
// calls from the response, execute them, append results to history, and
// repeat until the LLM produces a final text-only answer.
//
// Loop invariant: at the start of each iteration, `history` contains the
// full conversation so far (system prompt + user messages + prior tool
// results). The loop exits when:
//   • the LLM returns no tool calls (final answer), or
//   • max_iterations is reached (runaway safety), or
//   • the cancellation token fires (external abort).

/// Execute a single turn of the agent loop: send messages, parse tool calls,
/// execute tools, and loop until the LLM produces a final text response.
///
/// Pass `scope_ctx` to enable per-user/channel/chat_type tool access control.
/// When `None`, no scope-based restriction is applied.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_tool_call_loop(
    provider: &dyn Provider,
    history: &mut Vec<ChatMessage>,
    tools_registry: &[Box<dyn Tool>],
    observer: &dyn Observer,
    hooks: &HookManager,
    provider_name: &str,
    model: &str,
    temperature: f64,
    silent: bool,
    approval: Option<&ApprovalManager>,
    channel_name: &str,
    multimodal_config: &crate::config::MultimodalConfig,
    max_tool_iterations: usize,
    parallel_tools_enabled: bool,
    read_only_tool_concurrency_window: usize,
    read_only_tool_timeout_secs: u64,
    priority_scheduling_enabled: bool,
    low_priority_tool_names: Vec<String>,
    concurrency_governance: ToolConcurrencyGovernanceConfig,
    compaction_config: Option<&crate::config::AgentCompactionConfig>,
    cancellation_token: Option<CancellationToken>,
    on_delta: Option<tokio::sync::mpsc::Sender<String>>,
    scope_ctx: Option<&ScopeContext<'_>>,
) -> Result<String> {
    let max_iterations = if max_tool_iterations == 0 {
        DEFAULT_MAX_TOOL_ITERATIONS
    } else {
        max_tool_iterations.min(1000)
    };

    let tool_specs: Vec<crate::tools::ToolSpec> = tools_registry
        .iter()
        .flat_map(|tool| tool.specs())
        .collect();
    let rollout = resolve_rollout_decision(
        parallel_tools_enabled,
        &concurrency_governance,
        channel_name,
        scope_ctx,
    );
    tracing::info!(
        channel = channel_name,
        rollout_stage = %rollout.stage,
        parallel_enabled = rollout.enabled,
        kill_switch_applied = rollout.kill_switch_applied,
        reason = rollout.reason,
        "tool scheduler rollout decision"
    );
    let read_only_schedule = ReadOnlyToolScheduleConfig {
        parallel_enabled: rollout.enabled,
        concurrency_window: read_only_tool_concurrency_window.max(1),
        timeout_secs: read_only_tool_timeout_secs.max(1),
        priority_enabled: priority_scheduling_enabled,
        low_priority_tool_names: low_priority_tool_names.into_iter().collect(),
        rollout_stage: rollout.stage.clone(),
        kill_switch_applied: rollout.kill_switch_applied,
        auto_rollback_enabled: concurrency_governance.auto_rollback_enabled,
        rollback_timeout_rate_threshold: concurrency_governance
            .rollback_timeout_rate_threshold
            .clamp(0.0, 1.0),
        rollback_cancel_rate_threshold: concurrency_governance
            .rollback_cancel_rate_threshold
            .clamp(0.0, 1.0),
        rollback_error_rate_threshold: concurrency_governance
            .rollback_error_rate_threshold
            .clamp(0.0, 1.0),
    };
    let use_native_tools = provider.supports_native_tools() && !tool_specs.is_empty();

    // P1-3: Proactive pre-turn memory flush — compact before starting the loop
    // if the context is already above 85% of the token budget.
    if let Some(config) = compaction_config {
        let token_estimate = estimate_history_tokens(history);
        let threshold = (config.max_context_tokens as f64 * 0.85) as usize;
        if token_estimate > threshold {
            tracing::info!(
                token_estimate,
                threshold,
                "Pre-turn memory flush: context near limit, running pre-emptive compaction"
            );
            match tokio::time::timeout(
                std::time::Duration::from_secs(COMPACTION_TIMEOUT_SECS),
                apply_configurable_compaction(history, provider, model, config),
            )
            .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => tracing::warn!("Pre-turn compaction failed: {e}"),
                Err(_) => {
                    tracing::warn!(
                        "Pre-turn compaction timed out after {}s, applying aggressive trim",
                        COMPACTION_TIMEOUT_SECS
                    );
                    apply_aggressive_trim(history, config.keep_recent_messages);
                }
            }
        }
    }

    let mut overflow_retries: usize = 0;

    for _iteration in 0..max_iterations {
        if cancellation_token
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(ToolLoopCancelled.into());
        }

        // P0-3: Wrap mid-loop compaction in a safety timeout.
        if let Some(config) = compaction_config {
            match tokio::time::timeout(
                std::time::Duration::from_secs(COMPACTION_TIMEOUT_SECS),
                apply_configurable_compaction(history, provider, model, config),
            )
            .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    tracing::warn!(
                        "Compaction timed out after {}s, falling back to aggressive trim",
                        COMPACTION_TIMEOUT_SECS
                    );
                    apply_aggressive_trim(history, config.keep_recent_messages);
                }
            }
        }

        let image_marker_count = multimodal::count_image_markers(history);
        if image_marker_count > 0 && !provider.supports_vision() {
            return Err(ProviderCapabilityError {
                provider: provider_name.to_string(),
                capability: "vision".to_string(),
                message: format!(
                    "received {image_marker_count} image marker(s), but this provider does not support vision input"
                ),
            }
            .into());
        }

        let prepared_messages =
            multimodal::prepare_messages_for_provider(history, multimodal_config).await?;
        for tool in tools_registry {
            if let Err(err) = tool.refresh().await {
                let message = format!("refresh failed for tool {}: {err}", tool.name());
                observer.record_event(&ObserverEvent::Error {
                    component: "tool-refresh".to_string(),
                    message: message.clone(),
                });
                hooks
                    .emit(HookEvent::Error, payload_error("tool-refresh", &message))
                    .await;
            }
        }

        hooks
            .emit(
                HookEvent::LlmRequest,
                serde_json::json!({
                    "provider": provider_name,
                    "model": model,
                    "messages_count": history.len(),
                }),
            )
            .await;
        observer.record_event(&ObserverEvent::LlmRequest {
            provider: provider_name.to_string(),
            model: model.to_string(),
            messages_count: history.len(),
        });

        let llm_started_at = Instant::now();

        // Unified path via Provider::chat so provider-specific native tool logic
        // (OpenAI/Anthropic/OpenRouter/compatible adapters) is honored.
        let request_tools = if use_native_tools {
            Some(tool_specs.as_slice())
        } else {
            None
        };

        let chat_future = provider.chat(
            ChatRequest {
                messages: &prepared_messages.messages,
                tools: request_tools,
            },
            model,
            temperature,
        );

        let chat_result = if let Some(token) = cancellation_token.as_ref() {
            tokio::select! {
                () = token.cancelled() => return Err(ToolLoopCancelled.into()),
                result = chat_future => result,
            }
        } else {
            chat_future.await
        };

        // P0-1: chat_processed is Result so we can detect context overflow below.
        let chat_processed = match chat_result {
            Ok(resp) => {
                let duration = llm_started_at.elapsed();
                hooks
                    .emit(
                        HookEvent::LlmResponse,
                        serde_json::json!({
                            "provider": provider_name,
                            "model": model,
                            "duration_ms": duration.as_millis(),
                            "success": true,
                            "error_message": serde_json::Value::Null,
                        }),
                    )
                    .await;
                observer.record_event(&ObserverEvent::LlmResponse {
                    provider: provider_name.to_string(),
                    model: model.to_string(),
                    duration,
                    success: true,
                    error_message: None,
                });

                let response_text = resp.text_or_empty().to_string();
                // First try native structured tool calls (OpenAI-format).
                // Fall back to text-based parsing (XML tags, markdown blocks,
                // GLM format) only if the provider returned no native calls —
                // this ensures we support both native and prompt-guided models.
                let mut calls = parse_structured_tool_calls(&resp.tool_calls);
                let mut parsed_text = String::new();

                if calls.is_empty() {
                    let (fallback_text, fallback_calls) = parse_tool_calls(&response_text);
                    if !fallback_text.is_empty() {
                        parsed_text = fallback_text;
                    }
                    calls = fallback_calls;
                    if calls.is_empty() && looks_like_unparsed_tool_call_syntax(&response_text) {
                        let sanitized_preview = sanitize_tool_parse_log_preview(&response_text);
                        tracing::error!(
                            raw_response = sanitized_preview.as_str(),
                            "Failed to parse model tool-call syntax; suppressing raw tool-call text"
                        );
                        parsed_text = "tool execution failed".to_string();
                    }
                }

                // Preserve native tool call IDs in assistant history so role=tool
                // follow-up messages can reference the exact call id.
                let assistant_history_content = if resp.tool_calls.is_empty() {
                    response_text.clone()
                } else {
                    build_native_assistant_history(&response_text, &resp.tool_calls)
                };

                let native_calls = resp.tool_calls;
                Ok((
                    response_text,
                    parsed_text,
                    calls,
                    assistant_history_content,
                    native_calls,
                ))
            }
            Err(e) => {
                let duration = llm_started_at.elapsed();
                let error_message = crate::providers::sanitize_api_error(&e.to_string());
                hooks
                    .emit(
                        HookEvent::LlmResponse,
                        serde_json::json!({
                            "provider": provider_name,
                            "model": model,
                            "duration_ms": duration.as_millis(),
                            "success": false,
                            "error_message": error_message,
                        }),
                    )
                    .await;
                observer.record_event(&ObserverEvent::LlmResponse {
                    provider: provider_name.to_string(),
                    model: model.to_string(),
                    duration,
                    success: false,
                    error_message: Some(error_message.clone()),
                });
                hooks
                    .emit(HookEvent::Error, payload_error("llm", &error_message))
                    .await;
                Err(e)
            }
        };

        // P0-1: On context overflow, run compaction and retry (up to MAX_OVERFLOW_RETRIES).
        let (response_text, parsed_text, tool_calls, assistant_history_content, native_tool_calls) =
            match chat_processed {
                Err(ref e) if is_context_overflow_error(e) && overflow_retries < MAX_OVERFLOW_RETRIES => {
                    overflow_retries += 1;
                    tracing::warn!(
                        attempt = overflow_retries,
                        max = MAX_OVERFLOW_RETRIES,
                        "Context overflow detected; running compaction and retrying LLM call"
                    );
                    if let Some(config) = compaction_config {
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(COMPACTION_TIMEOUT_SECS),
                            apply_configurable_compaction(history, provider, model, config),
                        )
                        .await
                        {
                            Ok(Ok(_)) => {}
                            Ok(Err(e)) => tracing::warn!("Overflow retry compaction failed: {e}"),
                            Err(_) => {
                                tracing::warn!("Overflow retry compaction timed out, applying aggressive trim");
                                apply_aggressive_trim(history, config.keep_recent_messages);
                            }
                        }
                    } else {
                        apply_aggressive_trim(history, COMPACTION_KEEP_RECENT_MESSAGES);
                    }
                    continue;
                }
                Err(e) => return Err(e),
                Ok(values) => values,
            };

        let display_text = if parsed_text.is_empty() {
            response_text.clone()
        } else {
            parsed_text
        };

        if tool_calls.is_empty() {
            // No tool calls — this is the final response.
            // If a streaming sender is provided, relay the text in small chunks
            // so the channel can progressively update the draft message.
            if let Some(ref tx) = on_delta {
                // Split on whitespace boundaries, accumulating chunks of at least
                // STREAM_CHUNK_MIN_CHARS characters for progressive draft updates.
                let mut chunk = String::new();
                for word in display_text.split_inclusive(char::is_whitespace) {
                    if cancellation_token
                        .as_ref()
                        .is_some_and(CancellationToken::is_cancelled)
                    {
                        return Err(ToolLoopCancelled.into());
                    }
                    chunk.push_str(word);
                    if chunk.len() >= STREAM_CHUNK_MIN_CHARS
                        && tx.send(std::mem::take(&mut chunk)).await.is_err()
                    {
                        break; // receiver dropped
                    }
                }
                if !chunk.is_empty() {
                    let _ = tx.send(chunk).await;
                }
            }
            history.push(ChatMessage::assistant(response_text.clone()));
            return Ok(display_text);
        }

        // Print any text the LLM produced alongside tool calls (unless silent)
        if !silent && !display_text.is_empty() {
            print!("{display_text}");
            let _ = std::io::stdout().flush();
        }

        // Execute tool calls and build results. `individual_results` tracks per-call output so
        // native-mode history can emit one role=tool message per tool call with the correct ID.
        //
        // Conservative scheduler:
        // - run read-only tools in small bounded batches;
        // - keep all stateful tools strictly serial.
        let mut tool_results = String::new();
        for call in &tool_calls {
            hooks
                .emit(
                    HookEvent::ToolCallStart,
                    serde_json::json!({
                        "tool": call.name,
                        "arguments": call.arguments,
                    }),
                )
                .await;
        }

        let individual_results = execute_tools_with_policy(
            &tool_calls,
            tools_registry,
            observer,
            approval,
            channel_name,
            read_only_schedule.clone(),
            cancellation_token.as_ref(),
            scope_ctx,
        )
        .await?;

        for (call, result) in tool_calls.iter().zip(individual_results.iter()) {
            let success = !result.starts_with("Error");
            hooks
                .emit(
                    HookEvent::ToolCall,
                    serde_json::json!({
                        "tool": call.name,
                        "success": success,
                        "output": result,
                    }),
                )
                .await;
            if !success {
                hooks
                    .emit(HookEvent::Error, payload_error("tool", result))
                    .await;
            }
            // P0-2: Truncate oversized tool results before inserting into history.
            let truncated_result = truncate_tool_result_if_needed(result, MAX_TOOL_RESULT_CHARS);
            let _ = writeln!(
                tool_results,
                "<tool_result name=\"{}\">\n{}\n</tool_result>",
                call.name, truncated_result
            );
        }

        // Add assistant message with tool calls + tool results to history.
        // Native mode: use JSON-structured messages so convert_messages() can
        // reconstruct proper OpenAI-format tool_calls and tool result messages.
        // Prompt mode: use XML-based text format as before.
        history.push(ChatMessage::assistant(assistant_history_content));
        if native_tool_calls.is_empty() {
            history.push(ChatMessage::user(format!("[Tool results]\n{tool_results}")));
        } else {
            for (native_call, result) in native_tool_calls.iter().zip(individual_results.iter()) {
                // P0-2: Also truncate native tool result content.
                let truncated_result = truncate_tool_result_if_needed(result, MAX_TOOL_RESULT_CHARS);
                let tool_msg = serde_json::json!({
                    "tool_call_id": native_call.id,
                    "content": truncated_result,
                });
                history.push(ChatMessage::tool(tool_msg.to_string()));
            }
        }

        // P1-4: Token-aware mid-turn trim (primary) + count-based safety net (secondary).
        if let Some(config) = compaction_config {
            let mid_turn_tokens = estimate_history_tokens(history);
            let mid_turn_limit =
                config.max_context_tokens.saturating_sub(config.reserve_tokens);
            if mid_turn_tokens > mid_turn_limit {
                tracing::warn!(
                    mid_turn_tokens,
                    mid_turn_limit,
                    "Mid-turn token trim triggered"
                );
                trim_history_token_aware(history, mid_turn_limit);
            }
        }
        // Secondary count-based safety net to catch cases with no compaction config.
        if history.len() > 200 {
            let before = history.len();
            trim_history(history, DEFAULT_MAX_HISTORY_MESSAGES);
            tracing::info!(
                before,
                after = history.len(),
                "Mid-turn count-based safety trim triggered",
            );
        }
    }

    anyhow::bail!("Agent exceeded maximum tool iterations ({max_iterations})")
}

/// Build the tool instruction block for the system prompt so the LLM knows
/// how to invoke tools.
pub(crate) fn build_tool_instructions(tools_registry: &[Box<dyn Tool>]) -> String {
    let mut instructions = String::new();
    instructions.push_str("\n## Tool Use Protocol\n\n");
    instructions.push_str("To use a tool, wrap a JSON object in <tool_call></tool_call> tags:\n\n");
    instructions.push_str("```\n<tool_call>\n{\"name\": \"tool_name\", \"arguments\": {\"param\": \"value\"}}\n</tool_call>\n```\n\n");
    instructions.push_str(
        "CRITICAL: Output actual <tool_call> tags—never describe steps or give examples.\n\n",
    );
    instructions.push_str("Example: User says \"what's the date?\". You MUST respond with:\n<tool_call>\n{\"name\":\"shell\",\"arguments\":{\"command\":\"date\"}}\n</tool_call>\n\n");
    instructions.push_str("You may use multiple tool calls in a single response. ");
    instructions.push_str("After tool execution, results appear in <tool_result> tags. ");
    instructions
        .push_str("Continue reasoning with the results until you can give a final answer.\n\n");
    instructions.push_str("### Available Tools\n\n");

    for tool in tools_registry {
        for spec in tool.specs() {
            let _ = writeln!(
                instructions,
                "**{}**: {}\nParameters: `{}`\n",
                spec.name, spec.description, spec.parameters
            );
        }
    }

    instructions
}

// ── CLI Entrypoint ───────────────────────────────────────────────────────
// Wires up all subsystems (observer, runtime, security, memory, tools,
// provider, hardware RAG) and enters either single-shot or
// interactive REPL mode. The interactive loop manages history compaction
// and hard trimming to keep the context window bounded.

#[allow(clippy::too_many_lines)]
pub async fn run(
    config: Config,
    message: Option<String>,
    provider_override: Option<String>,
    model_override: Option<String>,
    temperature: f64,
) -> Result<String> {
    // ── Wire up agnostic subsystems ──────────────────────────────
    let base_observer = observability::create_observer(&config.observability);
    let observer: Arc<dyn Observer> = Arc::from(base_observer);
    let hooks = HookManager::new(config.workspace_dir.clone());
    let runtime: Arc<dyn runtime::RuntimeAdapter> =
        Arc::from(runtime::create_runtime(&config.runtime)?);
    let security = Arc::new(SecurityPolicy::from_config(
        &config.autonomy,
        &config.workspace_dir,
    ));

    // ── Memory (the brain) ────────────────────────────────────────
    let mem: Arc<dyn Memory> = Arc::from(memory::create_memory_with_storage_and_routes_with_acl(
        &config.memory,
        &config.embedding_routes,
        Some(&config.storage.provider.config),
        &config.workspace_dir,
        config.api_key.as_deref(),
        &config.identity_bindings,
        &config.user_policies,
    )?);
    tracing::info!(backend = mem.name(), "Memory initialized");

    // ── Tools ────────────────────────────────────────────────────
    let (composio_key, composio_entity_id) = if config.composio.enabled {
        (
            config.composio.api_key.as_deref(),
            Some(config.composio.entity_id.as_str()),
        )
    } else {
        (None, None)
    };
    let mut tools_registry = tools::all_tools_with_runtime(
        Arc::new(config.clone()),
        &security,
        runtime,
        mem.clone(),
        composio_key,
        composio_entity_id,
        &config.browser,
        &config.http_request,
        &config.workspace_dir,
        &config.agents,
        config.api_key.as_deref(),
        &config,
    );


    // ── Resolve provider ─────────────────────────────────────────
    let provider_name = provider_override
        .as_deref()
        .or(config.default_provider.as_deref())
        .unwrap_or("openrouter");

    let model_name = model_override
        .as_deref()
        .or(config.default_model.as_deref())
        .unwrap_or("anthropic/claude-sonnet-4");

    let provider_runtime_options = providers::ProviderRuntimeOptions {
        auth_profile_override: None,
        openprx_dir: config.config_path.parent().map(std::path::PathBuf::from),
        secrets_encrypt: config.secrets.encrypt,
        codex_auth_json_path: Some(config.auth.codex_auth_json_path.clone()),
        codex_auth_json_auto_import: config.auth.codex_auth_json_auto_import,
        reasoning_enabled: config.runtime.reasoning_enabled,
    };

    let provider: Box<dyn Provider> = providers::create_routed_provider_with_options(
        provider_name,
        config.api_key.as_deref(),
        config.api_url.as_deref(),
        &config.reliability,
        &config.model_routes,
        model_name,
        &provider_runtime_options,
    )?;

    observer.record_event(&ObserverEvent::AgentStart {
        provider: provider_name.to_string(),
        model: model_name.to_string(),
    });
    hooks
        .emit(
            HookEvent::AgentStart,
            serde_json::json!({
                "provider": provider_name,
                "model": model_name,
            }),
        )
        .await;

    // ── Build system prompt from workspace MD files (OpenClaw framework) ──
    let mut tool_descs: Vec<(&str, &str)> = vec![
        (
            "shell",
            "Execute terminal commands. Use when: running local checks, build/test commands, diagnostics. Don't use when: a safer dedicated tool exists, or command is destructive without approval.",
        ),
        (
            "file_read",
            "Read file contents. Use when: inspecting project files, configs, logs. Don't use when: a targeted search is enough.",
        ),
        (
            "file_write",
            "Write file contents. Use when: applying focused edits, scaffolding files, updating docs/code. Don't use when: side effects are unclear or file ownership is uncertain.",
        ),
        (
            "memory_store",
            "Save to memory. Use when: preserving durable preferences, decisions, key context. Don't use when: information is transient/noisy/sensitive without need.",
        ),
        (
            "memory_recall",
            "Search memory. Use when: retrieving prior decisions, user preferences, historical context. Don't use when: answer is already in current context.",
        ),
        (
            "memory_forget",
            "Delete a memory entry. Use when: memory is incorrect/stale or explicitly requested for removal. Don't use when: impact is uncertain.",
        ),
    ];
    tool_descs.push((
        "cron_add",
        "Create a cron job. Supports schedule kinds: cron, at, every; and job types: shell or agent.",
    ));
    tool_descs.push((
        "cron_list",
        "List all cron jobs with schedule, status, and metadata.",
    ));
    tool_descs.push(("cron_remove", "Remove a cron job by job_id."));
    tool_descs.push((
        "cron_update",
        "Patch a cron job (schedule, enabled, command/prompt, model, delivery, session_target).",
    ));
    tool_descs.push((
        "cron_run",
        "Force-run a cron job immediately and record a run history entry.",
    ));
    tool_descs.push(("cron_runs", "Show recent run history for a cron job."));
    tool_descs.push((
        "screenshot",
        "Capture a screenshot of the current screen. Returns file path and base64-encoded PNG. Use when: visual verification, UI inspection, debugging displays.",
    ));
    tool_descs.push((
        "image_info",
        "Read image file metadata (format, dimensions, size) and optionally base64-encode it. Use when: inspecting images, preparing visual data for analysis.",
    ));
    if config.browser.enabled {
        tool_descs.push((
            "browser_open",
            "Open approved HTTPS URLs in Brave Browser (allowlist-only, no scraping)",
        ));
    }
    if config.composio.enabled {
        tool_descs.push((
            "composio",
            "Execute actions on 1000+ apps via Composio (Gmail, Notion, GitHub, Slack, etc.). Use action='list' to discover, 'execute' to run (optionally with connected_account_id), 'connect' to OAuth.",
        ));
    }
    tool_descs.push((
        "schedule",
        "Manage scheduled tasks (create/list/get/cancel/pause/resume). Supports recurring cron and one-shot delays.",
    ));
    if !config.agents.is_empty() {
        tool_descs.push((
            "delegate",
            "Delegate a sub-task to a specialized agent. Use when: task needs different model/capability, or to parallelize work.",
        ));
    }
    let native_tools = provider.supports_native_tools();
    let skill_embedder = memory::create_embedder_from_config(&config, config.api_key.as_deref());
    let mut skills = crate::skills::load_skills_with_config(&config.workspace_dir, &config);
    if config.skill_rag.enabled {
        crate::skills::hydrate_skill_embeddings(&mut skills, skill_embedder.as_ref()).await?;
    }

    // ── Approval manager (supervised mode) ───────────────────────
    let approval_manager = ApprovalManager::from_config(&config.autonomy);

    // ── Execute ──────────────────────────────────────────────────
    let start = Instant::now();

    let mut final_output = String::new();

    if let Some(msg) = message {
        let selected_skills =
            select_prompt_skills(&msg, &skills, &config, skill_embedder.as_ref()).await;
        let system_prompt = build_runtime_system_prompt(
            &config,
            model_name,
            &tool_descs,
            &selected_skills,
            native_tools,
            &tools_registry,
        );

        // Auto-save user message to memory (skip short/trivial messages)
        if config.memory.auto_save
            && msg.chars().count() >= AUTOSAVE_MIN_MESSAGE_CHARS
            && memory::should_autosave_content(&msg)
        {
            let user_key = autosave_memory_key("user_msg");
            let _ = mem
                .store(&user_key, &msg, MemoryCategory::Conversation, None)
                .await;
        }

        // Inject memory context into user message
        let mem_context =
            build_context(mem.as_ref(), &msg, config.memory.min_relevance_score).await;
        let context = mem_context.preamble.clone();
        let enriched = if context.is_empty() {
            msg.clone()
        } else {
            format!("{context}{msg}")
        };

        let mut history = vec![
            ChatMessage::system(&system_prompt),
            ChatMessage::user(&enriched),
        ];

        let response = run_tool_call_loop(
            provider.as_ref(),
            &mut history,
            &tools_registry,
            observer.as_ref(),
            &hooks,
            provider_name,
            model_name,
            temperature,
            false,
            Some(&approval_manager),
            "cli",
            &config.multimodal,
            config.agent.max_tool_iterations,
            config.agent.parallel_tools,
            config.agent.read_only_tool_concurrency_window,
            config.agent.read_only_tool_timeout_secs,
            config.agent.priority_scheduling_enabled,
            config.agent.low_priority_tools.clone(),
            ToolConcurrencyGovernanceConfig {
                kill_switch_force_serial: config.agent.concurrency_kill_switch_force_serial,
                rollout_stage: config.agent.concurrency_rollout_stage.clone(),
                rollout_sample_percent: config.agent.concurrency_rollout_sample_percent,
                rollout_channels: config.agent.concurrency_rollout_channels.clone(),
                auto_rollback_enabled: config.agent.concurrency_auto_rollback_enabled,
                rollback_timeout_rate_threshold: config
                    .agent
                    .concurrency_rollback_timeout_rate_threshold,
                rollback_cancel_rate_threshold: config
                    .agent
                    .concurrency_rollback_cancel_rate_threshold,
                rollback_error_rate_threshold: config
                    .agent
                    .concurrency_rollback_error_rate_threshold,
            },
            Some(&config.agent.compaction),
            None,
            None,
            None,
        )
        .await?;
        increment_recalled_useful_counts(mem.as_ref(), &mem_context.ids).await;
        final_output = response.clone();
        println!("{response}");
        observer.record_event(&ObserverEvent::TurnComplete);
        hooks
            .emit(
                HookEvent::TurnComplete,
                serde_json::json!({
                    "mode": "single",
                    "response_chars": response.chars().count(),
                }),
            )
            .await;
    } else {
        println!("🦀 OpenPRX Interactive Mode");
        println!("Type /help for commands.\n");
        let cli = crate::channels::CliChannel::new();

        // Persistent conversation history across turns
        let mut history = if config.skill_rag.enabled {
            Vec::new()
        } else {
            vec![ChatMessage::system(build_runtime_system_prompt(
                &config,
                model_name,
                &tool_descs,
                &skills,
                native_tools,
                &tools_registry,
            ))]
        };

        loop {
            print!("> ");
            let _ = std::io::stdout().flush();

            let mut input = String::new();
            match std::io::stdin().read_line(&mut input) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => {
                    eprintln!("\nError reading input: {e}\n");
                    break;
                }
            }

            let user_input = input.trim().to_string();
            if user_input.is_empty() {
                continue;
            }
            match user_input.as_str() {
                "/quit" | "/exit" => break,
                "/help" => {
                    println!("Available commands:");
                    println!("  /help        Show this help message");
                    println!("  /clear /new  Clear conversation history");
                    println!("  /quit /exit  Exit interactive mode\n");
                    continue;
                }
                "/clear" | "/new" => {
                    println!(
                        "This will clear the current conversation and delete all session memory."
                    );
                    println!("Core memories (long-term facts/preferences) will be preserved.");
                    print!("Continue? [y/N] ");
                    let _ = std::io::stdout().flush();

                    let mut confirm = String::new();
                    if std::io::stdin().read_line(&mut confirm).is_err() {
                        continue;
                    }
                    if !matches!(confirm.trim().to_lowercase().as_str(), "y" | "yes") {
                        println!("Cancelled.\n");
                        continue;
                    }

                    history.clear();
                    if !config.skill_rag.enabled {
                        history.push(ChatMessage::system(build_runtime_system_prompt(
                            &config,
                            model_name,
                            &tool_descs,
                            &skills,
                            native_tools,
                            &tools_registry,
                        )));
                    }
                    // Clear conversation and daily memory
                    let mut cleared = 0;
                    for category in [MemoryCategory::Conversation, MemoryCategory::Daily] {
                        let entries = mem.list(Some(&category), None).await.unwrap_or_default();
                        for entry in entries {
                            if mem.forget(&entry.key).await.unwrap_or(false) {
                                cleared += 1;
                            }
                        }
                    }
                    if cleared > 0 {
                        println!("Conversation cleared ({cleared} memory entries removed).\n");
                    } else {
                        println!("Conversation cleared.\n");
                    }
                    continue;
                }
                _ => {}
            }

            // Auto-save conversation turns (skip short/trivial messages)
            if config.memory.auto_save
                && user_input.chars().count() >= AUTOSAVE_MIN_MESSAGE_CHARS
                && memory::should_autosave_content(&user_input)
            {
                let user_key = autosave_memory_key("user_msg");
                let _ = mem
                    .store(&user_key, &user_input, MemoryCategory::Conversation, None)
                    .await;
            }

            // Inject memory context into user message
            let mem_context =
                build_context(mem.as_ref(), &user_input, config.memory.min_relevance_score).await;
            let context = mem_context.preamble.clone();
            let enriched = if context.is_empty() {
                user_input.clone()
            } else {
                format!("{context}{user_input}")
            };

            let selected_skills =
                select_prompt_skills(&user_input, &skills, &config, skill_embedder.as_ref()).await;
            let system_prompt = build_runtime_system_prompt(
                &config,
                model_name,
                &tool_descs,
                &selected_skills,
                native_tools,
                &tools_registry,
            );
            if history.is_empty() {
                history.push(ChatMessage::system(system_prompt));
            } else {
                history[0] = ChatMessage::system(system_prompt);
            }
            history.push(ChatMessage::user(&enriched));

            let response = match run_tool_call_loop(
                provider.as_ref(),
                &mut history,
                &tools_registry,
                observer.as_ref(),
                &hooks,
                provider_name,
                model_name,
                temperature,
                false,
                Some(&approval_manager),
                "cli",
                &config.multimodal,
                config.agent.max_tool_iterations,
                config.agent.parallel_tools,
                config.agent.read_only_tool_concurrency_window,
                config.agent.read_only_tool_timeout_secs,
                config.agent.priority_scheduling_enabled,
                config.agent.low_priority_tools.clone(),
                ToolConcurrencyGovernanceConfig {
                    kill_switch_force_serial: config.agent.concurrency_kill_switch_force_serial,
                    rollout_stage: config.agent.concurrency_rollout_stage.clone(),
                    rollout_sample_percent: config.agent.concurrency_rollout_sample_percent,
                    rollout_channels: config.agent.concurrency_rollout_channels.clone(),
                    auto_rollback_enabled: config.agent.concurrency_auto_rollback_enabled,
                    rollback_timeout_rate_threshold: config
                        .agent
                        .concurrency_rollback_timeout_rate_threshold,
                    rollback_cancel_rate_threshold: config
                        .agent
                        .concurrency_rollback_cancel_rate_threshold,
                    rollback_error_rate_threshold: config
                        .agent
                        .concurrency_rollback_error_rate_threshold,
                },
                Some(&config.agent.compaction),
                None,
                None,
                None,
            )
            .await
            {
                Ok(resp) => resp,
                Err(e) => {
                    eprintln!("\nError: {e}\n");
                    hooks
                        .emit(
                            HookEvent::Error,
                            payload_error("agent-turn", &e.to_string()),
                        )
                        .await;
                    continue;
                }
            };
            increment_recalled_useful_counts(mem.as_ref(), &mem_context.ids).await;
            final_output = response.clone();
            if let Err(e) = crate::channels::Channel::send(
                &cli,
                &crate::channels::traits::SendMessage::new(format!("\n{response}\n"), "user"),
            )
            .await
            {
                eprintln!("\nError sending CLI response: {e}\n");
            }
            observer.record_event(&ObserverEvent::TurnComplete);
            hooks
                .emit(
                    HookEvent::TurnComplete,
                    serde_json::json!({
                        "mode": "interactive",
                        "response_chars": response.chars().count(),
                    }),
                )
                .await;

            // Auto-compaction before hard trimming to preserve long-context signal.
            if let Ok(compacted) = auto_compact_history(
                &mut history,
                provider.as_ref(),
                model_name,
                config.agent.max_history_messages,
                mem.as_ref(),
            )
            .await
            {
                if compacted {
                    println!("🧹 Auto-compaction complete");
                }
            }

            // Hard cap as a safety net.
            trim_history(&mut history, config.agent.max_history_messages);
        }
    }

    let duration = start.elapsed();
    observer.record_event(&ObserverEvent::AgentEnd {
        provider: provider_name.to_string(),
        model: model_name.to_string(),
        duration,
        tokens_used: None,
        cost_usd: None,
    });
    hooks
        .emit(
            HookEvent::AgentEnd,
            serde_json::json!({
                "duration_ms": duration.as_millis(),
                "tokens_used": serde_json::Value::Null,
            }),
        )
        .await;

    Ok(final_output)
}

/// Process a single message through the full agent (with tools and memory).
/// Used by channels (Telegram, Discord, etc.) to enable hardware and tool use.
pub async fn process_message(config: Config, message: &str) -> Result<String> {
    let observer: Arc<dyn Observer> =
        Arc::from(observability::create_observer(&config.observability));
    let hooks = HookManager::new(config.workspace_dir.clone());
    let runtime: Arc<dyn runtime::RuntimeAdapter> =
        Arc::from(runtime::create_runtime(&config.runtime)?);
    let security = Arc::new(SecurityPolicy::from_config(
        &config.autonomy,
        &config.workspace_dir,
    ));
    let mem: Arc<dyn Memory> = Arc::from(memory::create_memory_with_storage_and_routes_with_acl(
        &config.memory,
        &config.embedding_routes,
        Some(&config.storage.provider.config),
        &config.workspace_dir,
        config.api_key.as_deref(),
        &config.identity_bindings,
        &config.user_policies,
    )?);

    let (composio_key, composio_entity_id) = if config.composio.enabled {
        (
            config.composio.api_key.as_deref(),
            Some(config.composio.entity_id.as_str()),
        )
    } else {
        (None, None)
    };
    let mut tools_registry = tools::all_tools_with_runtime(
        Arc::new(config.clone()),
        &security,
        runtime,
        mem.clone(),
        composio_key,
        composio_entity_id,
        &config.browser,
        &config.http_request,
        &config.workspace_dir,
        &config.agents,
        config.api_key.as_deref(),
        &config,
    );

    let provider_name = config.default_provider.as_deref().unwrap_or("openrouter");
    let model_name = config
        .default_model
        .clone()
        .unwrap_or_else(|| "anthropic/claude-sonnet-4-20250514".into());
    let provider_runtime_options = providers::ProviderRuntimeOptions {
        auth_profile_override: None,
        openprx_dir: config.config_path.parent().map(std::path::PathBuf::from),
        secrets_encrypt: config.secrets.encrypt,
        codex_auth_json_path: Some(config.auth.codex_auth_json_path.clone()),
        codex_auth_json_auto_import: config.auth.codex_auth_json_auto_import,
        reasoning_enabled: config.runtime.reasoning_enabled,
    };
    let provider: Box<dyn Provider> = providers::create_routed_provider_with_options(
        provider_name,
        config.api_key.as_deref(),
        config.api_url.as_deref(),
        &config.reliability,
        &config.model_routes,
        &model_name,
        &provider_runtime_options,
    )?;

    let mut skills = crate::skills::load_skills_with_config(&config.workspace_dir, &config);
    let mut tool_descs: Vec<(&str, &str)> = vec![
        ("shell", "Execute terminal commands."),
        ("file_read", "Read file contents."),
        ("file_write", "Write file contents."),
        ("memory_store", "Save to memory."),
        ("memory_recall", "Search memory."),
        ("memory_forget", "Delete a memory entry."),
        ("screenshot", "Capture a screenshot."),
        ("image_info", "Read image metadata."),
    ];
    if config.browser.enabled {
        tool_descs.push(("browser_open", "Open approved URLs in browser."));
    }
    if config.composio.enabled {
        tool_descs.push(("composio", "Execute actions on 1000+ apps via Composio."));
    }
    let native_tools = provider.supports_native_tools();
    let skill_embedder = memory::create_embedder_from_config(&config, config.api_key.as_deref());
    if config.skill_rag.enabled {
        crate::skills::hydrate_skill_embeddings(&mut skills, skill_embedder.as_ref()).await?;
    }
    let selected_skills =
        select_prompt_skills(message, &skills, &config, skill_embedder.as_ref()).await;
    let system_prompt = build_runtime_system_prompt(
        &config,
        &model_name,
        &tool_descs,
        &selected_skills,
        native_tools,
        &tools_registry,
    );

    let mem_context = build_context(mem.as_ref(), message, config.memory.min_relevance_score).await;
    let context = mem_context.preamble.clone();
    let enriched = if context.is_empty() {
        message.to_string()
    } else {
        format!("{context}{message}")
    };

    let mut history = vec![
        ChatMessage::system(&system_prompt),
        ChatMessage::user(&enriched),
    ];

    let response = agent_turn(
        provider.as_ref(),
        &mut history,
        &tools_registry,
        observer.as_ref(),
        &hooks,
        provider_name,
        &model_name,
        config.default_temperature,
        true,
        &config.multimodal,
        config.agent.max_tool_iterations,
        config.agent.parallel_tools,
        config.agent.read_only_tool_concurrency_window,
        config.agent.read_only_tool_timeout_secs,
        config.agent.priority_scheduling_enabled,
        config.agent.low_priority_tools.clone(),
        ToolConcurrencyGovernanceConfig {
            kill_switch_force_serial: config.agent.concurrency_kill_switch_force_serial,
            rollout_stage: config.agent.concurrency_rollout_stage.clone(),
            rollout_sample_percent: config.agent.concurrency_rollout_sample_percent,
            rollout_channels: config.agent.concurrency_rollout_channels.clone(),
            auto_rollback_enabled: config.agent.concurrency_auto_rollback_enabled,
            rollback_timeout_rate_threshold: config
                .agent
                .concurrency_rollback_timeout_rate_threshold,
            rollback_cancel_rate_threshold: config.agent.concurrency_rollback_cancel_rate_threshold,
            rollback_error_rate_threshold: config.agent.concurrency_rollback_error_rate_threshold,
        },
    )
    .await?;
    increment_recalled_useful_counts(mem.as_ref(), &mem_context.ids).await;
    hooks
        .emit(
            HookEvent::TurnComplete,
            serde_json::json!({
                "mode": "channel",
                "response_chars": response.chars().count(),
            }),
        )
        .await;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[test]
    fn test_scrub_credentials() {
        let input = "API_KEY=sk-1234567890abcdef; token: 1234567890; password=\"secret123456\"";
        let scrubbed = scrub_credentials(input);
        assert!(scrubbed.contains("API_KEY=sk-1*[REDACTED]"));
        assert!(scrubbed.contains("token: 1234*[REDACTED]"));
        assert!(scrubbed.contains("password=\"secr*[REDACTED]\""));
        assert!(!scrubbed.contains("abcdef"));
        assert!(!scrubbed.contains("secret123456"));
    }

    #[test]
    fn test_scrub_credentials_json() {
        let input = r#"{"api_key": "sk-1234567890", "other": "public"}"#;
        let scrubbed = scrub_credentials(input);
        assert!(scrubbed.contains("\"api_key\": \"sk-1*[REDACTED]\""));
        assert!(scrubbed.contains("public"));
    }
    use crate::memory::{Memory, MemoryCategory, SqliteMemory};
    use crate::observability::NoopObserver;
    use crate::providers::traits::ProviderCapabilities;
    use crate::providers::ChatResponse;
    use tempfile::TempDir;

    struct NonVisionProvider {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for NonVisionProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok("ok".to_string())
        }
    }

    struct VisionProvider {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for VisionProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                native_tool_calling: false,
                vision: true,
            }
        }

        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok("ok".to_string())
        }

        async fn chat(
            &self,
            request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<ChatResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let marker_count = crate::multimodal::count_image_markers(request.messages);
            if marker_count == 0 {
                anyhow::bail!("expected image markers in request messages");
            }

            if request.tools.is_some() {
                anyhow::bail!("no tools should be attached for this test");
            }

            Ok(ChatResponse {
                text: Some("vision-ok".to_string()),
                tool_calls: Vec::new(),
            })
        }
    }

    struct ScriptedProvider {
        responses: Arc<Mutex<VecDeque<ChatResponse>>>,
    }

    impl ScriptedProvider {
        fn from_text_responses(responses: Vec<&str>) -> Self {
            let scripted = responses
                .into_iter()
                .map(|text| ChatResponse {
                    text: Some(text.to_string()),
                    tool_calls: Vec::new(),
                })
                .collect();
            Self {
                responses: Arc::new(Mutex::new(scripted)),
            }
        }
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            anyhow::bail!("chat_with_system should not be used in scripted provider tests");
        }

        async fn chat(
            &self,
            _request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<ChatResponse> {
            let mut responses = self
                .responses
                .lock()
                .expect("responses lock should be valid");
            responses
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("scripted provider exhausted responses"))
        }
    }

    struct DelayTool {
        name: String,
        delay_ms: u64,
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
    }

    impl DelayTool {
        fn new(
            name: &str,
            delay_ms: u64,
            active: Arc<AtomicUsize>,
            max_active: Arc<AtomicUsize>,
        ) -> Self {
            Self {
                name: name.to_string(),
                delay_ms,
                active,
                max_active,
            }
        }
    }

    #[async_trait]
    impl Tool for DelayTool {
        fn name(&self) -> &str {
            &self.name
        }

        fn description(&self) -> &str {
            "Delay tool for testing parallel tool execution"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "value": { "type": "string" }
                },
                "required": ["value"]
            })
        }

        async fn execute(
            &self,
            args: serde_json::Value,
        ) -> anyhow::Result<crate::tools::ToolResult> {
            let now_active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(now_active, Ordering::SeqCst);

            tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;

            self.active.fetch_sub(1, Ordering::SeqCst);

            let value = args
                .get("value")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();

            Ok(crate::tools::ToolResult {
                success: true,
                output: format!("ok:{value}"),
                error: None,
            })
        }
    }

    #[tokio::test]
    async fn run_tool_call_loop_returns_structured_error_for_non_vision_provider() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = NonVisionProvider {
            calls: Arc::clone(&calls),
        };

        let mut history = vec![ChatMessage::user(
            "please inspect [IMAGE:data:image/png;base64,iVBORw0KGgo=]".to_string(),
        )];
        let tools_registry: Vec<Box<dyn Tool>> = Vec::new();
        let observer = NoopObserver;

        let err = run_tool_call_loop(
            &provider,
            &mut history,
            &tools_registry,
            &observer,
            &crate::hooks::HookManager::new(std::env::temp_dir()),
            "mock-provider",
            "mock-model",
            0.0,
            true,
            None,
            "cli",
            &crate::config::MultimodalConfig::default(),
            3,
            false,
            2,
            30,
            false,
            Vec::new(),
            ToolConcurrencyGovernanceConfig::default(),
            None,
            None,
            None,
            None, // no scope context
        )
        .await
        .expect_err("provider without vision support should fail");

        assert!(err.to_string().contains("provider_capability_error"));
        assert!(err.to_string().contains("capability=vision"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn run_tool_call_loop_rejects_oversized_image_payload() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = VisionProvider {
            calls: Arc::clone(&calls),
        };

        let oversized_payload = STANDARD.encode(vec![0_u8; (1024 * 1024) + 1]);
        let mut history = vec![ChatMessage::user(format!(
            "[IMAGE:data:image/png;base64,{oversized_payload}]"
        ))];

        let tools_registry: Vec<Box<dyn Tool>> = Vec::new();
        let observer = NoopObserver;
        let multimodal = crate::config::MultimodalConfig {
            max_images: 4,
            max_image_size_mb: 1,
            allow_remote_fetch: false,
        };

        let err = run_tool_call_loop(
            &provider,
            &mut history,
            &tools_registry,
            &observer,
            &crate::hooks::HookManager::new(std::env::temp_dir()),
            "mock-provider",
            "mock-model",
            0.0,
            true,
            None,
            "cli",
            &multimodal,
            3,
            false,
            2,
            30,
            false,
            Vec::new(),
            ToolConcurrencyGovernanceConfig::default(),
            None,
            None,
            None,
            None, // no scope context
        )
        .await
        .expect_err("oversized payload must fail");

        assert!(err
            .to_string()
            .contains("multimodal image size limit exceeded"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn run_tool_call_loop_accepts_valid_multimodal_request_flow() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = VisionProvider {
            calls: Arc::clone(&calls),
        };

        let mut history = vec![ChatMessage::user(
            "Analyze this [IMAGE:data:image/png;base64,iVBORw0KGgo=]".to_string(),
        )];
        let tools_registry: Vec<Box<dyn Tool>> = Vec::new();
        let observer = NoopObserver;

        let result = run_tool_call_loop(
            &provider,
            &mut history,
            &tools_registry,
            &observer,
            &crate::hooks::HookManager::new(std::env::temp_dir()),
            "mock-provider",
            "mock-model",
            0.0,
            true,
            None,
            "cli",
            &crate::config::MultimodalConfig::default(),
            3,
            false,
            2,
            30,
            false,
            Vec::new(),
            ToolConcurrencyGovernanceConfig::default(),
            None,
            None,
            None,
            None, // no scope context
        )
        .await
        .expect("valid multimodal payload should pass");

        assert_eq!(result, "vision-ok");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    struct RecordingTool {
        name: String,
        execution_order: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[derive(Default)]
    struct SchedulerEventObserver {
        batch_events: std::sync::Mutex<Vec<ObserverEvent>>,
    }

    impl Observer for SchedulerEventObserver {
        fn record_event(&self, event: &ObserverEvent) {
            if matches!(event, ObserverEvent::ToolBatch { .. }) {
                self.batch_events
                    .lock()
                    .expect("batch events lock should be valid")
                    .push(event.clone());
            }
        }

        fn record_metric(&self, _metric: &crate::observability::traits::ObserverMetric) {}

        fn name(&self) -> &str {
            "scheduler-events"
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    #[async_trait]
    impl Tool for RecordingTool {
        fn name(&self) -> &str {
            &self.name
        }

        fn description(&self) -> &str {
            "Records execution order"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
        ) -> anyhow::Result<crate::tools::ToolResult> {
            self.execution_order
                .lock()
                .expect("execution order lock should be valid")
                .push(self.name.clone());
            Ok(crate::tools::ToolResult {
                success: true,
                output: self.name.clone(),
                error: None,
            })
        }
    }

    #[test]
    fn classify_tool_call_marks_known_read_only_tools() {
        assert_eq!(
            classify_tool_call("file_read"),
            ToolSchedulingClass::ReadOnly
        );
        assert_eq!(
            classify_tool_call("memory_search"),
            ToolSchedulingClass::ReadOnly
        );
    }

    #[test]
    fn classify_tool_call_marks_mutating_or_unknown_tools_as_stateful() {
        assert_eq!(classify_tool_call("shell"), ToolSchedulingClass::Stateful);
        assert_eq!(
            classify_tool_call("totally_unknown_tool"),
            ToolSchedulingClass::Stateful
        );
    }

    #[test]
    fn resolve_rollout_decision_prioritizes_kill_switch() {
        let decision = resolve_rollout_decision(
            true,
            &ToolConcurrencyGovernanceConfig {
                kill_switch_force_serial: true,
                rollout_stage: "full".to_string(),
                ..ToolConcurrencyGovernanceConfig::default()
            },
            "telegram",
            None,
        );
        assert!(!decision.enabled);
        assert!(decision.kill_switch_applied);
        assert_eq!(decision.reason, "kill_switch_force_serial");
    }

    #[test]
    fn resolve_rollout_decision_stage_sampling_is_deterministic() {
        let governance = ToolConcurrencyGovernanceConfig {
            rollout_stage: "stage_a".to_string(),
            rollout_sample_percent: 5,
            ..ToolConcurrencyGovernanceConfig::default()
        };
        let first = resolve_rollout_decision(true, &governance, "telegram", None);
        let second = resolve_rollout_decision(true, &governance, "telegram", None);
        assert_eq!(first.enabled, second.enabled);
        assert_eq!(first.stage, "stage_a");
    }

    #[tokio::test]
    async fn run_tool_call_loop_executes_read_only_tools_with_bounded_parallelism() {
        let provider = ScriptedProvider::from_text_responses(vec![
            r#"<tool_call>
{"name":"file_read","arguments":{"value":"A"}}
</tool_call>
<tool_call>
{"name":"memory_get","arguments":{"value":"B"}}
</tool_call>"#,
            "done",
        ]);

        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let tools_registry: Vec<Box<dyn Tool>> = vec![
            Box::new(DelayTool::new(
                "file_read",
                200,
                Arc::clone(&active),
                Arc::clone(&max_active),
            )),
            Box::new(DelayTool::new(
                "memory_get",
                200,
                Arc::clone(&active),
                Arc::clone(&max_active),
            )),
        ];

        let approval_cfg = crate::config::AutonomyConfig {
            level: crate::security::AutonomyLevel::Full,
            ..crate::config::AutonomyConfig::default()
        };
        let approval_mgr = ApprovalManager::from_config(&approval_cfg);

        let mut history = vec![
            ChatMessage::system("test-system"),
            ChatMessage::user("run tool calls"),
        ];
        let observer = NoopObserver;

        let started = std::time::Instant::now();
        let result = run_tool_call_loop(
            &provider,
            &mut history,
            &tools_registry,
            &observer,
            &crate::hooks::HookManager::new(std::env::temp_dir()),
            "mock-provider",
            "mock-model",
            0.0,
            true,
            Some(&approval_mgr),
            "telegram",
            &crate::config::MultimodalConfig::default(),
            4,
            true,
            2,
            30,
            false,
            Vec::new(),
            ToolConcurrencyGovernanceConfig {
                rollout_stage: "full".to_string(),
                ..ToolConcurrencyGovernanceConfig::default()
            },
            None,
            None,
            None,
            None, // no scope context
        )
        .await
        .expect("read-only parallel execution should complete");
        let elapsed = started.elapsed();

        assert_eq!(result, "done");
        assert!(
            elapsed < Duration::from_secs(2),
            "parallel execution should complete within a reasonable bound; elapsed={elapsed:?}"
        );
        assert!(
            max_active.load(Ordering::SeqCst) >= 2,
            "both tools should overlap in execution"
        );

        let tool_results_message = history
            .iter()
            .find(|msg| msg.role == "user" && msg.content.starts_with("[Tool results]"))
            .expect("tool results message should be present");
        let idx_a = tool_results_message
            .content
            .find("name=\"file_read\"")
            .expect("file_read result should be present");
        let idx_b = tool_results_message
            .content
            .find("name=\"memory_get\"")
            .expect("memory_get result should be present");
        assert!(
            idx_a < idx_b,
            "tool results should preserve input order for tool call mapping"
        );
    }

    #[tokio::test]
    async fn run_tool_call_loop_keeps_stateful_tools_strictly_serial() {
        let provider = ScriptedProvider::from_text_responses(vec![
            r#"<tool_call>
{"name":"delay_stateful_a","arguments":{"value":"A"}}
</tool_call>
<tool_call>
{"name":"delay_stateful_b","arguments":{"value":"B"}}
</tool_call>"#,
            "done",
        ]);

        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let tools_registry: Vec<Box<dyn Tool>> = vec![
            Box::new(DelayTool::new(
                "delay_stateful_a",
                200,
                Arc::clone(&active),
                Arc::clone(&max_active),
            )),
            Box::new(DelayTool::new(
                "delay_stateful_b",
                200,
                Arc::clone(&active),
                Arc::clone(&max_active),
            )),
        ];

        let approval_cfg = crate::config::AutonomyConfig {
            level: crate::security::AutonomyLevel::Full,
            ..crate::config::AutonomyConfig::default()
        };
        let approval_mgr = ApprovalManager::from_config(&approval_cfg);

        let mut history = vec![
            ChatMessage::system("test-system"),
            ChatMessage::user("run tool calls"),
        ];
        let observer = NoopObserver;

        let started = std::time::Instant::now();
        let result = run_tool_call_loop(
            &provider,
            &mut history,
            &tools_registry,
            &observer,
            &crate::hooks::HookManager::new(std::env::temp_dir()),
            "mock-provider",
            "mock-model",
            0.0,
            true,
            Some(&approval_mgr),
            "telegram",
            &crate::config::MultimodalConfig::default(),
            4,
            true,
            2,
            30,
            false,
            Vec::new(),
            ToolConcurrencyGovernanceConfig {
                rollout_stage: "full".to_string(),
                ..ToolConcurrencyGovernanceConfig::default()
            },
            None,
            None,
            None,
            None, // no scope context
        )
        .await
        .expect("stateful serial execution should complete");
        let elapsed = started.elapsed();

        assert_eq!(result, "done");
        assert!(
            elapsed >= Duration::from_millis(360),
            "stateful tools should execute serially; elapsed={elapsed:?}"
        );
        assert_eq!(
            max_active.load(Ordering::SeqCst),
            1,
            "stateful tools should never overlap in execution"
        );
    }

    #[tokio::test]
    async fn execute_tools_with_policy_prioritizes_foreground_tools() {
        let execution_order = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let tools_registry: Vec<Box<dyn Tool>> = vec![
            Box::new(RecordingTool {
                name: "sessions_spawn".to_string(),
                execution_order: Arc::clone(&execution_order),
            }),
            Box::new(RecordingTool {
                name: "file_read".to_string(),
                execution_order: Arc::clone(&execution_order),
            }),
        ];

        let calls = vec![
            ParsedToolCall {
                name: "sessions_spawn".to_string(),
                arguments: serde_json::json!({}),
            },
            ParsedToolCall {
                name: "file_read".to_string(),
                arguments: serde_json::json!({}),
            },
        ];

        let results = execute_tools_with_policy(
            &calls,
            &tools_registry,
            &NoopObserver,
            None,
            "cli",
            ReadOnlyToolScheduleConfig {
                parallel_enabled: true,
                concurrency_window: 2,
                timeout_secs: 30,
                priority_enabled: true,
                low_priority_tool_names: ["sessions_spawn".to_string()].into_iter().collect(),
                rollout_stage: "full".to_string(),
                kill_switch_applied: false,
                auto_rollback_enabled: true,
                rollback_timeout_rate_threshold: 0.2,
                rollback_cancel_rate_threshold: 0.2,
                rollback_error_rate_threshold: 0.2,
            },
            None,
            None,
        )
        .await
        .expect("priority scheduling should execute successfully");

        assert_eq!(results, vec!["sessions_spawn", "file_read"]);
        let observed_order = execution_order
            .lock()
            .expect("execution order lock should be valid")
            .clone();
        assert_eq!(observed_order, vec!["file_read", "sessions_spawn"]);
    }

    #[tokio::test]
    async fn execute_tools_with_policy_triggers_rollback_and_forces_remaining_serial() {
        let observer = SchedulerEventObserver::default();
        let tools_registry: Vec<Box<dyn Tool>> = vec![
            Box::new(DelayTool::new(
                "file_read",
                150,
                Arc::new(AtomicUsize::new(0)),
                Arc::new(AtomicUsize::new(0)),
            )),
            Box::new(RecordingTool {
                name: "shell".to_string(),
                execution_order: Arc::new(std::sync::Mutex::new(Vec::new())),
            }),
        ];
        let calls = vec![
            ParsedToolCall {
                name: "file_read".to_string(),
                arguments: serde_json::json!({"id": 1}),
            },
            ParsedToolCall {
                name: "file_read".to_string(),
                arguments: serde_json::json!({"id": 2}),
            },
            ParsedToolCall {
                name: "shell".to_string(),
                arguments: serde_json::json!({}),
            },
            ParsedToolCall {
                name: "file_read".to_string(),
                arguments: serde_json::json!({"id": 3}),
            },
        ];

        let _ = execute_tools_with_policy(
            &calls,
            &tools_registry,
            &observer,
            None,
            "cli",
            ReadOnlyToolScheduleConfig {
                parallel_enabled: true,
                concurrency_window: 2,
                timeout_secs: 0,
                priority_enabled: false,
                low_priority_tool_names: std::collections::HashSet::new(),
                rollout_stage: "stage_a".to_string(),
                kill_switch_applied: false,
                auto_rollback_enabled: true,
                rollback_timeout_rate_threshold: 0.10,
                rollback_cancel_rate_threshold: 1.0,
                rollback_error_rate_threshold: 1.0,
            },
            None,
            None,
        )
        .await
        .expect("scheduler should complete with rollback");

        let events = observer
            .batch_events
            .lock()
            .expect("batch events lock should be valid")
            .clone();
        assert_eq!(
            events.len(),
            1,
            "rollback should force subsequent read-only calls into serial lane"
        );
        assert!(matches!(
            events[0],
            ObserverEvent::ToolBatch { rollback: true, .. }
        ));
    }

    #[tokio::test]
    async fn execute_one_tool_applies_barrier_for_shared_resource_tools() {
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(DelayTool::new(
            "file_write",
            120,
            Arc::clone(&active),
            Arc::clone(&max_active),
        ))];

        let first = execute_one_tool(
            "file_write",
            serde_json::json!({"value":"A"}),
            &tools_registry,
            &NoopObserver,
            None,
            None,
        );
        let second = execute_one_tool(
            "file_write",
            serde_json::json!({"value":"B"}),
            &tools_registry,
            &NoopObserver,
            None,
            None,
        );

        let (_a, _b) = tokio::join!(first, second);
        assert_eq!(
            max_active.load(Ordering::SeqCst),
            1,
            "barrier should serialize concurrent file_write calls"
        );
    }

    #[test]
    fn parse_tool_calls_extracts_single_call() {
        let response = r#"Let me check that.
<tool_call>
{"name": "shell", "arguments": {"command": "ls -la"}}
</tool_call>"#;

        let (text, calls) = parse_tool_calls(response);
        assert_eq!(text, "Let me check that.");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "ls -la"
        );
    }

    #[test]
    fn parse_tool_calls_extracts_multiple_calls() {
        let response = r#"<tool_call>
{"name": "file_read", "arguments": {"path": "a.txt"}}
</tool_call>
<tool_call>
{"name": "file_read", "arguments": {"path": "b.txt"}}
</tool_call>"#;

        let (_, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "file_read");
        assert_eq!(calls[1].name, "file_read");
    }

    #[test]
    fn parse_tool_calls_returns_text_only_when_no_calls() {
        let response = "Just a normal response with no tools.";
        let (text, calls) = parse_tool_calls(response);
        assert_eq!(text, "Just a normal response with no tools.");
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_tool_calls_handles_malformed_json() {
        let response = r#"<tool_call>
not valid json
</tool_call>
Some text after."#;

        let (text, calls) = parse_tool_calls(response);
        assert!(calls.is_empty());
        assert!(text.contains("Some text after."));
    }

    #[test]
    fn parse_tool_calls_text_before_and_after() {
        let response = r#"Before text.
<tool_call>
{"name": "shell", "arguments": {"command": "echo hi"}}
</tool_call>
After text."#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.contains("Before text."));
        assert!(text.contains("After text."));
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn parse_tool_calls_handles_openai_format() {
        // OpenAI-style response with tool_calls array
        let response = r#"{"content": "Let me check that for you.", "tool_calls": [{"type": "function", "function": {"name": "shell", "arguments": "{\"command\": \"ls -la\"}"}}]}"#;

        let (text, calls) = parse_tool_calls(response);
        assert_eq!(text, "Let me check that for you.");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "ls -la"
        );
    }

    #[test]
    fn parse_tool_calls_handles_openai_format_multiple_calls() {
        let response = r#"{"tool_calls": [{"type": "function", "function": {"name": "file_read", "arguments": "{\"path\": \"a.txt\"}"}}, {"type": "function", "function": {"name": "file_read", "arguments": "{\"path\": \"b.txt\"}"}}]}"#;

        let (_, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "file_read");
        assert_eq!(calls[1].name, "file_read");
    }

    #[test]
    fn parse_tool_calls_openai_format_without_content() {
        // Some providers don't include content field with tool_calls
        let response = r#"{"tool_calls": [{"type": "function", "function": {"name": "memory_recall", "arguments": "{}"}}]}"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty()); // No content field
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "memory_recall");
    }

    #[test]
    fn parse_tool_calls_handles_markdown_json_inside_tool_call_tag() {
        let response = r#"<tool_call>
```json
{"name": "file_write", "arguments": {"path": "test.py", "content": "print('ok')"}}
```
</tool_call>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "file_write");
        assert_eq!(
            calls[0].arguments.get("path").unwrap().as_str().unwrap(),
            "test.py"
        );
    }

    #[test]
    fn parse_tool_calls_handles_noisy_tool_call_tag_body() {
        let response = r#"<tool_call>
I will now call the tool with this payload:
{"name": "shell", "arguments": {"command": "pwd"}}
</tool_call>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "pwd"
        );
    }

    #[test]
    fn parse_tool_calls_handles_markdown_tool_call_fence() {
        let response = r#"I'll check that.
```tool_call
{"name": "shell", "arguments": {"command": "pwd"}}
```
Done."#;

        let (text, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "pwd"
        );
        assert!(text.contains("I'll check that."));
        assert!(text.contains("Done."));
        assert!(!text.contains("```tool_call"));
    }

    #[test]
    fn parse_tool_calls_handles_markdown_tool_call_hybrid_close_tag() {
        let response = r#"Preface
```tool-call
{"name": "shell", "arguments": {"command": "date"}}
</tool_call>
Tail"#;

        let (text, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "date"
        );
        assert!(text.contains("Preface"));
        assert!(text.contains("Tail"));
        assert!(!text.contains("```tool-call"));
    }

    #[test]
    fn parse_tool_calls_handles_markdown_invoke_fence() {
        let response = r#"Checking.
```invoke
{"name": "shell", "arguments": {"command": "date"}}
```
Done."#;

        let (text, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "date"
        );
        assert!(text.contains("Checking."));
        assert!(text.contains("Done."));
    }

    #[test]
    fn parse_tool_calls_handles_toolcall_tag_alias() {
        let response = r#"<toolcall>
{"name": "shell", "arguments": {"command": "date"}}
</toolcall>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "date"
        );
    }

    #[test]
    fn parse_tool_calls_handles_tool_dash_call_tag_alias() {
        let response = r#"<tool-call>
{"name": "shell", "arguments": {"command": "whoami"}}
</tool-call>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "whoami"
        );
    }

    #[test]
    fn parse_tool_calls_handles_tool_call_tag_with_attributes() {
        let response = r#"<tool_call name="shell" mode="json">
{"name": "shell", "arguments": {"command": "date"}}
</tool_call>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "date"
        );
    }

    #[test]
    fn parse_tool_calls_handles_invoke_tag_alias() {
        let response = r#"<invoke>
{"name": "shell", "arguments": {"command": "uptime"}}
</invoke>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "uptime"
        );
    }

    #[test]
    fn parse_tool_calls_handles_codex_to_shell_code_format() {
        let response = r#"assistant to=shell code
date
"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(calls[0].arguments["command"], "date");
    }

    #[test]
    fn parse_tool_calls_handles_codex_to_shell_code_with_fence() {
        let response = r#"to=shell code
```bash
pwd
```
"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(calls[0].arguments["command"], "pwd");
    }

    #[test]
    fn parse_tool_calls_handles_assistant_recipient_format_with_json_arguments() {
        let response = r#"<assistant recipient="file_read">
{"path":"README.md"}
</assistant>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "file_read");
        assert_eq!(calls[0].arguments["path"], "README.md");
    }

    #[test]
    fn parse_tool_calls_handles_assistant_recipient_format_with_shell_command_body() {
        let response = r#"<assistant recipient="shell">
ls -la
</assistant>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(calls[0].arguments["command"], "ls -la");
    }

    #[test]
    fn parse_tool_calls_recovers_unclosed_tool_call_with_json() {
        let response = r#"I will call the tool now.
<tool_call>
{"name": "shell", "arguments": {"command": "uptime -p"}}"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.contains("I will call the tool now."));
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "uptime -p"
        );
    }

    #[test]
    fn parse_tool_calls_recovers_mismatched_close_tag() {
        let response = r#"<tool_call>
{"name": "shell", "arguments": {"command": "uptime"}}
</arg_value>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "uptime"
        );
    }

    #[test]
    fn parse_tool_calls_recovers_cross_alias_closing_tags() {
        let response = r#"<toolcall>
{"name": "shell", "arguments": {"command": "date"}}
</tool_call>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
    }

    #[test]
    fn parse_tool_calls_rejects_raw_tool_json_without_tags() {
        // SECURITY: Raw JSON without explicit wrappers should NOT be parsed
        // This prevents prompt injection attacks where malicious content
        // could include JSON that mimics a tool call.
        let response = r#"Sure, creating the file now.
{"name": "file_write", "arguments": {"path": "hello.py", "content": "print('hello')"}}"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.contains("Sure, creating the file now."));
        assert_eq!(
            calls.len(),
            0,
            "Raw JSON without wrappers should not be parsed"
        );
    }

    #[test]
    fn build_tool_instructions_includes_all_tools() {
        use crate::security::SecurityPolicy;
        let security = Arc::new(SecurityPolicy::from_config(
            &crate::config::AutonomyConfig::default(),
            std::path::Path::new("/tmp"),
        ));
        let tools = tools::default_tools(security);
        let instructions = build_tool_instructions(&tools);

        assert!(instructions.contains("## Tool Use Protocol"));
        assert!(instructions.contains("<tool_call>"));
        assert!(instructions.contains("shell"));
        assert!(instructions.contains("file_read"));
        assert!(instructions.contains("file_write"));
    }

    #[test]
    fn tools_to_openai_format_produces_valid_schema() {
        use crate::security::SecurityPolicy;
        let security = Arc::new(SecurityPolicy::from_config(
            &crate::config::AutonomyConfig::default(),
            std::path::Path::new("/tmp"),
        ));
        let tools = tools::default_tools(security);
        let formatted = tools_to_openai_format(&tools);

        assert!(!formatted.is_empty());
        for tool_json in &formatted {
            assert_eq!(tool_json["type"], "function");
            assert!(tool_json["function"]["name"].is_string());
            assert!(tool_json["function"]["description"].is_string());
            assert!(!tool_json["function"]["name"].as_str().unwrap().is_empty());
        }
        // Verify known tools are present
        let names: Vec<&str> = formatted
            .iter()
            .filter_map(|t| t["function"]["name"].as_str())
            .collect();
        assert!(names.contains(&"shell"));
        assert!(names.contains(&"file_read"));
    }

    #[test]
    fn trim_history_preserves_system_prompt() {
        let mut history = vec![ChatMessage::system("system prompt")];
        for i in 0..DEFAULT_MAX_HISTORY_MESSAGES + 20 {
            history.push(ChatMessage::user(format!("msg {i}")));
        }
        let original_len = history.len();
        assert!(original_len > DEFAULT_MAX_HISTORY_MESSAGES + 1);

        trim_history(&mut history, DEFAULT_MAX_HISTORY_MESSAGES);

        // System prompt preserved
        assert_eq!(history[0].role, "system");
        assert_eq!(history[0].content, "system prompt");
        // Trimmed to limit
        assert_eq!(history.len(), DEFAULT_MAX_HISTORY_MESSAGES + 1); // +1 for system
                                                                     // Most recent messages preserved
        let last = &history[history.len() - 1];
        assert_eq!(
            last.content,
            format!("msg {}", DEFAULT_MAX_HISTORY_MESSAGES + 19)
        );
    }

    #[test]
    fn trim_history_noop_when_within_limit() {
        let mut history = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("hello"),
            ChatMessage::assistant("hi"),
        ];
        trim_history(&mut history, DEFAULT_MAX_HISTORY_MESSAGES);
        assert_eq!(history.len(), 3);
    }

    #[test]
    fn build_compaction_transcript_formats_roles() {
        let messages = vec![
            ChatMessage::user("I like dark mode"),
            ChatMessage::assistant("Got it"),
        ];
        let transcript = build_compaction_transcript(&messages);
        assert!(transcript.contains("USER: I like dark mode"));
        assert!(transcript.contains("ASSISTANT: Got it"));
    }

    #[test]
    fn apply_compaction_summary_replaces_old_segment() {
        let mut history = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("old 1"),
            ChatMessage::assistant("old 2"),
            ChatMessage::user("recent 1"),
            ChatMessage::assistant("recent 2"),
        ];

        apply_compaction_summary(&mut history, 1, 3, "- user prefers concise replies");

        assert_eq!(history.len(), 4);
        assert!(history[1].content.contains("Compaction summary"));
        assert!(history[2].content.contains("recent 1"));
        assert!(history[3].content.contains("recent 2"));
    }

    #[test]
    fn compaction_trigger_limit_respects_reserve_tokens() {
        let config = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Safeguard,
            reserve_tokens: 100,
            max_context_tokens: 500,
            ..crate::config::AgentCompactionConfig::default()
        };
        assert_eq!(compaction_trigger_limit(&config), Some(400));
    }

    #[tokio::test]
    async fn configurable_compaction_safeguard_inserts_summary_marker() {
        let provider = ScriptedProvider::from_text_responses(vec!["- concise summary"]);
        let mut history = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("u1 ".repeat(120)),
            ChatMessage::assistant("a1 ".repeat(120)),
            ChatMessage::user("u2 ".repeat(120)),
            ChatMessage::assistant("a2 ".repeat(120)),
        ];
        let config = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Safeguard,
            reserve_tokens: 1,
            keep_recent_messages: 1,
            memory_flush: false,
            max_context_tokens: 50,
        };
        let compacted = apply_configurable_compaction(&mut history, &provider, "model", &config)
            .await
            .unwrap();
        assert!(compacted);
        assert!(history.iter().any(|msg| {
            msg.content.contains("[Context compacted at") && msg.content.contains("Summary:")
        }));
    }

    #[tokio::test]
    async fn configurable_compaction_aggressive_adds_memory_flush_note() {
        let provider = ScriptedProvider::from_text_responses(vec!["flush notes"]);
        let mut history = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("u ".repeat(150)),
            ChatMessage::assistant("a ".repeat(150)),
            ChatMessage::user("u2 ".repeat(150)),
            ChatMessage::assistant("a2 ".repeat(150)),
        ];
        let config = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Aggressive,
            reserve_tokens: 1,
            keep_recent_messages: 1,
            memory_flush: true,
            max_context_tokens: 40,
        };
        let compacted = apply_configurable_compaction(&mut history, &provider, "model", &config)
            .await
            .unwrap();
        assert!(compacted);
        assert!(history
            .iter()
            .any(|msg| msg.content.contains("[Memory flush at")));
    }

    #[test]
    fn autosave_memory_key_has_prefix_and_uniqueness() {
        let key1 = autosave_memory_key("user_msg");
        let key2 = autosave_memory_key("user_msg");

        assert!(key1.starts_with("user_msg_"));
        assert!(key2.starts_with("user_msg_"));
        assert_ne!(key1, key2);
    }

    #[tokio::test]
    async fn autosave_memory_keys_preserve_multiple_turns() {
        let tmp = TempDir::new().unwrap();
        let mem = SqliteMemory::new(tmp.path()).unwrap();

        let key1 = autosave_memory_key("user_msg");
        let key2 = autosave_memory_key("user_msg");

        mem.store(&key1, "I'm Paul", MemoryCategory::Conversation, None)
            .await
            .unwrap();
        mem.store(&key2, "I'm 45", MemoryCategory::Conversation, None)
            .await
            .unwrap();

        assert_eq!(mem.count().await.unwrap(), 2);

        let recalled = mem.recall("45", 5, None).await.unwrap();
        assert!(recalled.iter().any(|entry| entry.content.contains("45")));
    }

    #[tokio::test]
    async fn build_context_ignores_legacy_assistant_autosave_entries() {
        let tmp = TempDir::new().unwrap();
        let mem = SqliteMemory::new(tmp.path()).unwrap();
        mem.store(
            "assistant_resp_poisoned",
            "User suffered a fabricated event",
            MemoryCategory::Daily,
            None,
        )
        .await
        .unwrap();
        mem.store(
            "user_msg_real",
            "User asked for concise status updates",
            MemoryCategory::Conversation,
            None,
        )
        .await
        .unwrap();

        let context = build_context(&mem, "status updates", 0.0).await;
        assert!(context.preamble.contains("user_msg_real"));
        assert!(!context.preamble.contains("assistant_resp_poisoned"));
        assert!(!context.preamble.contains("fabricated event"));
        assert_eq!(context.ids.len(), 1);
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Recovery Tests - Tool Call Parsing Edge Cases
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn parse_tool_calls_handles_empty_tool_result() {
        // Recovery: Empty tool_result tag should be handled gracefully
        let response = r#"I'll run that command.
<tool_result name="shell">

</tool_result>
Done."#;
        let (text, calls) = parse_tool_calls(response);
        assert!(text.contains("Done."));
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_arguments_value_handles_null() {
        // Recovery: null arguments are returned as-is (Value::Null)
        let value = serde_json::json!(null);
        let result = parse_arguments_value(Some(&value));
        assert!(result.is_null());
    }

    #[test]
    fn parse_tool_calls_handles_empty_tool_calls_array() {
        // Recovery: Empty tool_calls array returns original response (no tool parsing)
        let response = r#"{"content": "Hello", "tool_calls": []}"#;
        let (text, calls) = parse_tool_calls(response);
        // When tool_calls is empty, the entire JSON is returned as text
        assert!(text.contains("Hello"));
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_tool_calls_handles_whitespace_only_name() {
        // Recovery: Whitespace-only tool name should return None
        let value = serde_json::json!({"function": {"name": "   ", "arguments": {}}});
        let result = parse_tool_call_value(&value);
        assert!(result.is_none());
    }

    #[test]
    fn parse_tool_calls_handles_empty_string_arguments() {
        // Recovery: Empty string arguments should be handled
        let value = serde_json::json!({"name": "test", "arguments": ""});
        let result = parse_tool_call_value(&value);
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "test");
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Recovery Tests - History Management
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn trim_history_with_no_system_prompt() {
        // Recovery: History without system prompt should trim correctly
        let mut history = vec![];
        for i in 0..DEFAULT_MAX_HISTORY_MESSAGES + 20 {
            history.push(ChatMessage::user(format!("msg {i}")));
        }
        trim_history(&mut history, DEFAULT_MAX_HISTORY_MESSAGES);
        assert_eq!(history.len(), DEFAULT_MAX_HISTORY_MESSAGES);
    }

    #[test]
    fn trim_history_preserves_role_ordering() {
        // Recovery: After trimming, role ordering should remain consistent
        let mut history = vec![ChatMessage::system("system")];
        for i in 0..DEFAULT_MAX_HISTORY_MESSAGES + 10 {
            history.push(ChatMessage::user(format!("user {i}")));
            history.push(ChatMessage::assistant(format!("assistant {i}")));
        }
        trim_history(&mut history, DEFAULT_MAX_HISTORY_MESSAGES);
        assert_eq!(history[0].role, "system");
        assert_eq!(history[history.len() - 1].role, "assistant");
    }

    #[test]
    fn trim_history_with_only_system_prompt() {
        // Recovery: Only system prompt should not be trimmed
        let mut history = vec![ChatMessage::system("system prompt")];
        trim_history(&mut history, DEFAULT_MAX_HISTORY_MESSAGES);
        assert_eq!(history.len(), 1);
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Recovery Tests - Arguments Parsing
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn parse_arguments_value_handles_invalid_json_string() {
        // Recovery: Invalid JSON string should return empty object
        let value = serde_json::Value::String("not valid json".to_string());
        let result = parse_arguments_value(Some(&value));
        assert!(result.is_object());
        assert!(result.as_object().unwrap().is_empty());
    }

    #[test]
    fn parse_arguments_value_handles_none() {
        // Recovery: None arguments should return empty object
        let result = parse_arguments_value(None);
        assert!(result.is_object());
        assert!(result.as_object().unwrap().is_empty());
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Recovery Tests - JSON Extraction
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn extract_json_values_handles_empty_string() {
        // Recovery: Empty input should return empty vec
        let result = extract_json_values("");
        assert!(result.is_empty());
    }

    #[test]
    fn extract_json_values_handles_whitespace_only() {
        // Recovery: Whitespace only should return empty vec
        let result = extract_json_values("   \n\t  ");
        assert!(result.is_empty());
    }

    #[test]
    fn extract_json_values_handles_multiple_objects() {
        // Recovery: Multiple JSON objects should all be extracted
        let input = r#"{"a": 1}{"b": 2}{"c": 3}"#;
        let result = extract_json_values(input);
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn extract_json_values_handles_arrays() {
        // Recovery: JSON arrays should be extracted
        let input = r#"[1, 2, 3]{"key": "value"}"#;
        let result = extract_json_values(input);
        assert_eq!(result.len(), 2);
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Recovery Tests - Constants Validation
    // ═══════════════════════════════════════════════════════════════════════

    const _: () = {
        assert!(DEFAULT_MAX_TOOL_ITERATIONS > 0);
        assert!(DEFAULT_MAX_TOOL_ITERATIONS <= 100);
        assert!(DEFAULT_MAX_HISTORY_MESSAGES > 0);
        assert!(DEFAULT_MAX_HISTORY_MESSAGES <= 1000);
    };

    #[test]
    fn constants_bounds_are_compile_time_checked() {
        // Bounds are enforced by the const assertions above.
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Recovery Tests - Tool Call Value Parsing
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn parse_tool_call_value_handles_missing_name_field() {
        // Recovery: Missing name field should return None
        let value = serde_json::json!({"function": {"arguments": {}}});
        let result = parse_tool_call_value(&value);
        assert!(result.is_none());
    }

    #[test]
    fn parse_tool_call_value_handles_top_level_name() {
        // Recovery: Tool call with name at top level (non-OpenAI format)
        let value = serde_json::json!({"name": "test_tool", "arguments": {}});
        let result = parse_tool_call_value(&value);
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "test_tool");
    }

    #[test]
    fn parse_tool_call_value_accepts_top_level_parameters_alias() {
        let value = serde_json::json!({
            "name": "schedule",
            "parameters": {"action": "create", "message": "test"}
        });
        let result = parse_tool_call_value(&value).expect("tool call should parse");
        assert_eq!(result.name, "schedule");
        assert_eq!(
            result.arguments.get("action").and_then(|v| v.as_str()),
            Some("create")
        );
    }

    #[test]
    fn parse_tool_call_value_accepts_function_parameters_alias() {
        let value = serde_json::json!({
            "function": {
                "name": "shell",
                "parameters": {"command": "date"}
            }
        });
        let result = parse_tool_call_value(&value).expect("tool call should parse");
        assert_eq!(result.name, "shell");
        assert_eq!(
            result.arguments.get("command").and_then(|v| v.as_str()),
            Some("date")
        );
    }

    #[test]
    fn parse_tool_calls_from_json_value_handles_empty_array() {
        // Recovery: Empty tool_calls array should return empty vec
        let value = serde_json::json!({"tool_calls": []});
        let result = parse_tool_calls_from_json_value(&value);
        assert!(result.is_empty());
    }

    #[test]
    fn parse_tool_calls_from_json_value_handles_missing_tool_calls() {
        // Recovery: Missing tool_calls field should fall through
        let value = serde_json::json!({"name": "test", "arguments": {}});
        let result = parse_tool_calls_from_json_value(&value);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn parse_tool_calls_from_json_value_handles_top_level_array() {
        // Recovery: Top-level array of tool calls
        let value = serde_json::json!([
            {"name": "tool_a", "arguments": {}},
            {"name": "tool_b", "arguments": {}}
        ]);
        let result = parse_tool_calls_from_json_value(&value);
        assert_eq!(result.len(), 2);
    }

    // ═══════════════════════════════════════════════════════════════════════
    // GLM-Style Tool Call Parsing
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn parse_glm_style_browser_open_url() {
        let response = "browser_open/url>https://example.com";
        let calls = parse_glm_style_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "shell");
        assert!(calls[0].1["command"].as_str().unwrap().contains("curl"));
        assert!(calls[0].1["command"]
            .as_str()
            .unwrap()
            .contains("example.com"));
    }

    #[test]
    fn parse_glm_style_shell_command() {
        let response = "shell/command>ls -la";
        let calls = parse_glm_style_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "shell");
        assert_eq!(calls[0].1["command"], "ls -la");
    }

    #[test]
    fn parse_glm_style_http_request() {
        let response = "http_request/url>https://api.example.com/data";
        let calls = parse_glm_style_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "http_request");
        assert_eq!(calls[0].1["url"], "https://api.example.com/data");
        assert_eq!(calls[0].1["method"], "GET");
    }

    #[test]
    fn parse_glm_style_plain_url() {
        let response = "https://example.com/api";
        let calls = parse_glm_style_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "shell");
        assert!(calls[0].1["command"].as_str().unwrap().contains("curl"));
    }

    #[test]
    fn parse_glm_style_json_args() {
        let response = r#"shell/{"command": "echo hello"}"#;
        let calls = parse_glm_style_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "shell");
        assert_eq!(calls[0].1["command"], "echo hello");
    }

    #[test]
    fn parse_glm_style_multiple_calls() {
        let response = r#"shell/command>ls
browser_open/url>https://example.com"#;
        let calls = parse_glm_style_tool_calls(response);
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn parse_glm_style_tool_call_integration() {
        // Integration test: GLM format should be parsed in parse_tool_calls
        let response = "Checking...\nbrowser_open/url>https://example.com\nDone";
        let (text, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert!(text.contains("Checking"));
        assert!(text.contains("Done"));
    }

    #[test]
    fn parse_glm_style_rejects_non_http_url_param() {
        let response = "browser_open/url>javascript:alert(1)";
        let calls = parse_glm_style_tool_calls(response);
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_tool_calls_handles_unclosed_tool_call_tag() {
        let response = "<tool_call>{\"name\":\"shell\",\"arguments\":{\"command\":\"pwd\"}}\nDone";
        let (text, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(calls[0].arguments["command"], "pwd");
        assert_eq!(text, "Done");
    }

    // ─────────────────────────────────────────────────────────────────────
    // TG4 (inline): parse_tool_calls robustness — malformed/edge-case inputs
    // Prevents: Pattern 4 issues #746, #418, #777, #848
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn parse_tool_calls_empty_input_returns_empty() {
        let (text, calls) = parse_tool_calls("");
        assert!(calls.is_empty(), "empty input should produce no tool calls");
        assert!(text.is_empty(), "empty input should produce no text");
    }

    #[test]
    fn parse_tool_calls_whitespace_only_returns_empty_calls() {
        let (text, calls) = parse_tool_calls("   \n\t  ");
        assert!(calls.is_empty());
        assert!(text.is_empty() || text.trim().is_empty());
    }

    #[test]
    fn parse_tool_calls_nested_xml_tags_handled() {
        // Double-wrapped tool call should still parse the inner call
        let response = r#"<tool_call><tool_call>{"name":"echo","arguments":{"msg":"hi"}}</tool_call></tool_call>"#;
        let (_text, calls) = parse_tool_calls(response);
        // Should find at least one tool call
        assert!(
            !calls.is_empty(),
            "nested XML tags should still yield at least one tool call"
        );
    }

    #[test]
    fn parse_tool_calls_truncated_json_no_panic() {
        // Incomplete JSON inside tool_call tags
        let response = r#"<tool_call>{"name":"shell","arguments":{"command":"ls"</tool_call>"#;
        let (_text, _calls) = parse_tool_calls(response);
        // Should not panic — graceful handling of truncated JSON
    }

    #[test]
    fn looks_like_unparsed_tool_call_syntax_requires_complete_shapes() {
        assert!(!looks_like_unparsed_tool_call_syntax(
            "This text mentions <invoke in docs but has no closing tag."
        ));
        assert!(!looks_like_unparsed_tool_call_syntax(
            "Quoted protocol only: assistant to=shell code"
        ));
        assert!(!looks_like_unparsed_tool_call_syntax(
            "Discussion snippet: <assistant recipient=\"shell\"> without a closing tag."
        ));

        assert!(looks_like_unparsed_tool_call_syntax(
            r#"<invoke>{"name":"shell","arguments":{"command":"pwd"}}</invoke>"#
        ));
        assert!(looks_like_unparsed_tool_call_syntax(
            r#"assistant to=shell code
{"command":"pwd"}"#
        ));
        assert!(looks_like_unparsed_tool_call_syntax(
            r#"<assistant recipient="file_read">{"path":"README.md"}</assistant>"#
        ));
    }

    #[test]
    fn sanitize_tool_parse_log_preview_redacts_and_truncates() {
        let input = format!(
            "Bearer abcdefghijklmnopqrstuvwxyz sk-1234567890abcdef key=ABCDEF0123456789 {}",
            "x".repeat(260)
        );

        let preview = sanitize_tool_parse_log_preview(&input);
        assert!(preview.chars().count() <= TOOL_PARSE_LOG_PREVIEW_CHARS);
        assert!(!preview.contains("abcdefghijklmnopqrstuvwxyz"));
        assert!(!preview.contains("1234567890abcdef"));
        assert!(!preview.contains("ABCDEF0123456789"));
        assert!(preview.contains("Bearer [REDACTED]"));
        assert!(preview.contains("key=[REDACTED]"));
    }

    #[test]
    fn parse_tool_calls_empty_json_object_in_tag() {
        let response = "<tool_call>{}</tool_call>";
        let (_text, calls) = parse_tool_calls(response);
        // Empty JSON object has no name field — should not produce valid tool call
        assert!(
            calls.is_empty(),
            "empty JSON object should not produce a tool call"
        );
    }

    #[test]
    fn parse_tool_calls_closing_tag_only_returns_text() {
        let response = "Some text </tool_call> more text";
        let (text, calls) = parse_tool_calls(response);
        assert!(
            calls.is_empty(),
            "closing tag only should not produce calls"
        );
        assert!(
            !text.is_empty(),
            "text around orphaned closing tag should be preserved"
        );
    }

    #[test]
    fn parse_tool_calls_very_large_arguments_no_panic() {
        let large_arg = "x".repeat(100_000);
        let response = format!(
            r#"<tool_call>{{"name":"echo","arguments":{{"message":"{}"}}}}</tool_call>"#,
            large_arg
        );
        let (_text, calls) = parse_tool_calls(&response);
        assert_eq!(calls.len(), 1, "large arguments should still parse");
        assert_eq!(calls[0].name, "echo");
    }

    #[test]
    fn parse_tool_calls_special_characters_in_arguments() {
        let response = r#"<tool_call>{"name":"echo","arguments":{"message":"hello \"world\" <>&'\n\t"}}</tool_call>"#;
        let (_text, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "echo");
    }

    #[test]
    fn parse_tool_calls_text_with_embedded_json_not_extracted() {
        // Raw JSON without any tags should NOT be extracted as a tool call
        let response = r#"Here is some data: {"name":"echo","arguments":{"message":"hi"}} end."#;
        let (_text, calls) = parse_tool_calls(response);
        assert!(
            calls.is_empty(),
            "raw JSON in text without tags should not be extracted"
        );
    }

    #[test]
    fn parse_tool_calls_multiple_formats_mixed() {
        // Mix of text and properly tagged tool call
        let response = r#"I'll help you with that.

<tool_call>
{"name":"shell","arguments":{"command":"echo hello"}}
</tool_call>

Let me check the result."#;
        let (text, calls) = parse_tool_calls(response);
        assert_eq!(
            calls.len(),
            1,
            "should extract one tool call from mixed content"
        );
        assert_eq!(calls[0].name, "shell");
        assert!(
            text.contains("help you"),
            "text before tool call should be preserved"
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // TG4 (inline): scrub_credentials edge cases
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn scrub_credentials_empty_input() {
        let result = scrub_credentials("");
        assert_eq!(result, "");
    }

    #[test]
    fn scrub_credentials_no_sensitive_data() {
        let input = "normal text without any secrets";
        let result = scrub_credentials(input);
        assert_eq!(
            result, input,
            "non-sensitive text should pass through unchanged"
        );
    }

    #[test]
    fn scrub_credentials_short_values_not_redacted() {
        // Values shorter than 8 chars should not be redacted
        let input = r#"api_key="short""#;
        let result = scrub_credentials(input);
        assert_eq!(result, input, "short values should not be redacted");
    }

    // ─────────────────────────────────────────────────────────────────────
    // TG4 (inline): trim_history edge cases
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn trim_history_empty_history() {
        let mut history: Vec<crate::providers::ChatMessage> = vec![];
        trim_history(&mut history, 10);
        assert!(history.is_empty());
    }

    #[test]
    fn trim_history_system_only() {
        let mut history = vec![crate::providers::ChatMessage::system("system prompt")];
        trim_history(&mut history, 10);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role, "system");
    }

    #[test]
    fn trim_history_exactly_at_limit() {
        let mut history = vec![
            crate::providers::ChatMessage::system("system"),
            crate::providers::ChatMessage::user("msg 1"),
            crate::providers::ChatMessage::assistant("reply 1"),
        ];
        trim_history(&mut history, 2); // 2 non-system messages = exactly at limit
        assert_eq!(history.len(), 3, "should not trim when exactly at limit");
    }

    #[test]
    fn trim_history_removes_oldest_non_system() {
        let mut history = vec![
            crate::providers::ChatMessage::system("system"),
            crate::providers::ChatMessage::user("old msg"),
            crate::providers::ChatMessage::assistant("old reply"),
            crate::providers::ChatMessage::user("new msg"),
            crate::providers::ChatMessage::assistant("new reply"),
        ];
        trim_history(&mut history, 2);
        assert_eq!(history.len(), 3); // system + 2 kept
        assert_eq!(history[0].role, "system");
        assert_eq!(history[1].content, "new msg");
    }
}
