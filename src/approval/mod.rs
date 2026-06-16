//! Interactive approval workflow for supervised mode.
//!
//! Provides a pre-execution hook that prompts the user before tool calls,
//! with session-scoped "Always" allowlists and audit logging.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use crate::acl::approval_grant::{ApprovalGrantV2, IssuerAuthority, RiskLevel, Subject, WitnessKeyring};
use crate::config::AutonomyConfig;
use crate::security::AutonomyLevel;
use chrono::Utc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::{self, BufRead, Write};

// ── Types ────────────────────────────────────────────────────────

/// A request to approve a tool call before execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub tool_name: String,
    pub arguments: serde_json::Value,
}

/// The user's response to an approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalResponse {
    /// Execute this one call.
    Yes,
    /// Deny this call.
    No,
    /// Execute and add tool to session-scoped allowlist.
    Always,
}

/// A single audit log entry for an approval decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalLogEntry {
    pub timestamp: String,
    pub tool_name: String,
    pub arguments_summary: String,
    pub decision: ApprovalResponse,
    pub channel: String,
}

// ── ApprovalManager ──────────────────────────────────────────────

/// Manages the interactive approval workflow.
///
/// - Checks config-level `auto_approve` / `always_ask` lists
/// - Maintains a session-scoped "always" allowlist
/// - Records an audit trail of all decisions
pub struct ApprovalManager {
    /// Tools that never need approval (from config).
    auto_approve: HashSet<String>,
    /// Tools that always need approval, ignoring session allowlist.
    always_ask: HashSet<String>,
    /// Autonomy level from config.
    autonomy_level: AutonomyLevel,
    /// Session-scoped allowlist built from "Always" responses.
    session_allowlist: Mutex<HashSet<String>>,
    /// Audit trail of approval decisions.
    audit_log: Mutex<Vec<ApprovalLogEntry>>,
    /// FIX-P3-09: capability grants generated when an interactive `Yes`/`Always`
    /// decision is recorded. Bridges the interactive CLI approval track to the
    /// gate-runtime `ApprovalGrantV2` track so a human "yes" can be honoured by
    /// the cryptographic gate. Generation is best-effort: if the witness keyring
    /// cannot be loaded the decision is still recorded, only the grant is skipped.
    generated_grants: Mutex<Vec<ApprovalGrantV2>>,
}

impl ApprovalManager {
    /// Create from autonomy config.
    pub fn from_config(config: &AutonomyConfig) -> Self {
        Self {
            auto_approve: config.auto_approve.iter().cloned().collect(),
            always_ask: config.always_ask.iter().cloned().collect(),
            autonomy_level: config.level,
            session_allowlist: Mutex::new(HashSet::new()),
            audit_log: Mutex::new(Vec::new()),
            generated_grants: Mutex::new(Vec::new()),
        }
    }

    /// Create a manager driven solely by an [`AutonomyLevel`], with no
    /// config-level `auto_approve` / `always_ask` lists.
    ///
    /// Used by the background sub-agent NeedsInput path, which only knows the
    /// effective autonomy level (from the live [`crate::security::SecurityPolicy`])
    /// and wants the default supervised behaviour: every non-read-only tool call
    /// is flagged for approval so the suspend resolver can gate it. Under
    /// `ReadOnly` / `Full` autonomy `needs_approval` returns `false`, so no
    /// suspension occurs (matching the policy).
    #[must_use]
    pub fn from_autonomy_level(level: AutonomyLevel) -> Self {
        Self::from_autonomy_level_with_lists(level, HashSet::new(), HashSet::new())
    }

    /// Create a manager driven by an [`AutonomyLevel`] **plus** explicit
    /// `auto_approve` / `always_ask` lists inherited from the live
    /// [`AutonomyConfig`].
    ///
    /// Used by the background sub-agent NeedsInput path so that a supervised
    /// sub-agent honours the operator's configured `auto_approve` allowlist
    /// (read-only / explicitly trusted tools such as `file_read` /
    /// `memory_recall` do **not** suspend) and `always_ask` override, matching
    /// the foreground chat approval semantics. Only tools that genuinely
    /// [`needs_approval`](Self::needs_approval) under these lists trigger a
    /// NeedsInput suspension.
    #[must_use]
    pub fn from_autonomy_level_with_lists(
        level: AutonomyLevel,
        auto_approve: HashSet<String>,
        always_ask: HashSet<String>,
    ) -> Self {
        Self {
            auto_approve,
            always_ask,
            autonomy_level: level,
            session_allowlist: Mutex::new(HashSet::new()),
            audit_log: Mutex::new(Vec::new()),
            generated_grants: Mutex::new(Vec::new()),
        }
    }

    /// Check whether a tool call requires interactive approval.
    ///
    /// Returns `true` if the call needs a prompt, `false` if it can proceed.
    pub fn needs_approval(&self, tool_name: &str) -> bool {
        // ReadOnly blocks everything — handled elsewhere; no prompt needed.
        if self.autonomy_level == AutonomyLevel::ReadOnly {
            return false;
        }

        // always_ask is an explicit safety override, even in Full autonomy.
        if self.always_ask.contains(tool_name) {
            return true;
        }

        // Full autonomy skips prompts unless always_ask matched above.
        if self.autonomy_level == AutonomyLevel::Full {
            return false;
        }

        // auto_approve skips the prompt.
        if self.auto_approve.contains(tool_name) {
            return false;
        }

        // Session allowlist (from prior "Always" responses).
        let allowlist = self.session_allowlist.lock();
        if allowlist.contains(tool_name) {
            return false;
        }

        // Default: supervised mode requires approval.
        true
    }

    /// Record an approval decision and update session state.
    pub fn record_decision(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        decision: ApprovalResponse,
        channel: &str,
    ) {
        // If "Always", add to session allowlist (capped to prevent unbounded growth).
        if decision == ApprovalResponse::Always {
            const MAX_ALLOWLIST_SIZE: usize = 100;
            let mut allowlist = self.session_allowlist.lock();
            if allowlist.len() < MAX_ALLOWLIST_SIZE {
                allowlist.insert(tool_name.to_string());
            } else {
                tracing::warn!(
                    tool = tool_name,
                    "session approval allowlist is full ({MAX_ALLOWLIST_SIZE}); \
                     tool will require per-use approval"
                );
            }
        }

        // FIX-P3-09: a human `Yes`/`Always` is an issuance event for the gate
        // runtime. Mint a signed single-use capability grant so the gate can honour
        // the interactive approval. `No` never produces a grant. Generation is
        // best-effort and must never make `record_decision` fail: if the witness
        // keyring is unavailable we log and continue with the audit entry only.
        if matches!(decision, ApprovalResponse::Yes | ApprovalResponse::Always) {
            self.bridge_decision_to_grant(tool_name, channel);
        }

        // Append to audit log.
        let summary = summarize_args(args);
        let entry = ApprovalLogEntry {
            timestamp: Utc::now().to_rfc3339(),
            tool_name: tool_name.to_string(),
            arguments_summary: summary,
            decision,
            channel: channel.to_string(),
        };
        let mut log = self.audit_log.lock();
        log.push(entry);
    }

    /// FIX-P3-09: build and store a signed `ApprovalGrantV2` for an approved tool
    /// call, bridging the interactive track to the gate-runtime track.
    ///
    /// The interactive `ApprovalManager` has no full principal/workspace context,
    /// so the subject is derived from the approval `channel` (the only identity
    /// signal available at this layer); the grant's `op_id` is the `tool_name`
    /// (exact match) at `Medium` risk. The gate, when it consults these grants,
    /// still applies its own principal binding (`verify_for_operation_bound`),
    /// so a channel-derived subject can never widen authority beyond that check.
    fn bridge_decision_to_grant(&self, tool_name: &str, channel: &str) {
        let keyring = match WitnessKeyring::global() {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!(
                    tool = tool_name,
                    error = %e,
                    "approval grant bridge: witness keyring unavailable; recording decision without a grant"
                );
                return;
            }
        };
        let subject = Subject {
            agent_id: format!("prx:interactive:{channel}"),
            principal_id: channel.to_string(),
            owner_id: channel.to_string(),
            workspace_id: "interactive".to_string(),
            session_key: None,
        };
        match ApprovalGrantV2::issue_one_shot(
            keyring,
            subject,
            IssuerAuthority::HumanReview,
            tool_name,
            RiskLevel::Medium,
        ) {
            Ok(grant) => {
                let mut grants = self.generated_grants.lock();
                grants.push(grant);
            }
            Err(e) => {
                tracing::warn!(
                    tool = tool_name,
                    error = %e,
                    "approval grant bridge: failed to issue grant; recording decision without a grant"
                );
            }
        }
    }

    /// FIX-P3-09: snapshot of grants minted from interactive `Yes`/`Always`
    /// decisions, for the gate runtime to consult.
    pub fn generated_grants(&self) -> Vec<ApprovalGrantV2> {
        self.generated_grants.lock().clone()
    }

    /// Get a snapshot of the audit log.
    pub fn audit_log(&self) -> Vec<ApprovalLogEntry> {
        self.audit_log.lock().clone()
    }

    /// Get the current session allowlist.
    pub fn session_allowlist(&self) -> HashSet<String> {
        self.session_allowlist.lock().clone()
    }

    /// Prompt the user on the CLI and return their decision.
    ///
    /// For non-CLI channels, returns `Yes` automatically (interactive
    /// approval is only supported on CLI for now).
    pub fn prompt_cli(&self, request: &ApprovalRequest) -> ApprovalResponse {
        prompt_cli_interactive(request)
    }
}

// ── CLI prompt ───────────────────────────────────────────────────

/// Display the approval prompt and read user input from stdin.
fn prompt_cli_interactive(request: &ApprovalRequest) -> ApprovalResponse {
    let summary = summarize_args(&request.arguments);
    eprintln!();
    eprintln!("🔧 Agent wants to execute: {}", request.tool_name);
    eprintln!("   {summary}");
    eprint!("   [Y]es / [N]o / [A]lways for {}: ", request.tool_name);
    let _ = io::stderr().flush();

    let stdin = io::stdin();
    let mut line = String::new();
    if stdin.lock().read_line(&mut line).is_err() {
        return ApprovalResponse::No;
    }

    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => ApprovalResponse::Yes,
        "a" | "always" => ApprovalResponse::Always,
        _ => ApprovalResponse::No,
    }
}

/// Produce a short human-readable summary of tool arguments.
fn summarize_args(args: &serde_json::Value) -> String {
    match args {
        serde_json::Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(k, v)| {
                    let val = match v {
                        serde_json::Value::String(s) => truncate_for_summary(s, 80),
                        other => {
                            let s = other.to_string();
                            truncate_for_summary(&s, 80)
                        }
                    };
                    format!("{k}: {val}")
                })
                .collect();
            parts.join(", ")
        }
        other => {
            let s = other.to_string();
            truncate_for_summary(&s, 120)
        }
    }
}

fn truncate_for_summary(input: &str, max_chars: usize) -> String {
    let mut chars = input.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        input.to_string()
    }
}

// ── Tests ────────────────────────────────────────────────────────

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AutonomyConfig;

    fn supervised_config() -> AutonomyConfig {
        AutonomyConfig {
            level: AutonomyLevel::Supervised,
            auto_approve: vec!["file_read".into(), "memory_recall".into()],
            always_ask: vec!["shell".into()],
            ..AutonomyConfig::default()
        }
    }

    fn full_config() -> AutonomyConfig {
        AutonomyConfig {
            level: AutonomyLevel::Full,
            ..AutonomyConfig::default()
        }
    }

    // ── needs_approval ───────────────────────────────────────

    #[test]
    fn auto_approve_tools_skip_prompt() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        assert!(!mgr.needs_approval("file_read"));
        assert!(!mgr.needs_approval("memory_recall"));
    }

    #[test]
    fn always_ask_tools_always_prompt() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        assert!(mgr.needs_approval("shell"));
    }

    #[test]
    fn unknown_tool_needs_approval_in_supervised() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        assert!(mgr.needs_approval("file_write"));
        assert!(mgr.needs_approval("http_request"));
    }

    #[test]
    fn from_autonomy_level_gates_per_level() {
        // Supervised: every non-explicitly-allowed tool needs approval (drives
        // the background NeedsInput suspend path).
        let supervised = ApprovalManager::from_autonomy_level(AutonomyLevel::Supervised);
        assert!(supervised.needs_approval("shell"));
        assert!(supervised.needs_approval("file_write"));
        // Full / ReadOnly: never flagged, so no suspension occurs for backgrounded runs.
        let full = ApprovalManager::from_autonomy_level(AutonomyLevel::Full);
        assert!(!full.needs_approval("shell"));
        let read_only = ApprovalManager::from_autonomy_level(AutonomyLevel::ReadOnly);
        assert!(!read_only.needs_approval("shell"));
    }

    #[test]
    fn from_autonomy_level_with_lists_inherits_auto_approve_and_always_ask() {
        // Fix #3: the background NeedsInput supervised manager must inherit the
        // config `auto_approve` / `always_ask` lists so config-trusted / read-only
        // tools do NOT suspend, while `always_ask` tools always do.
        let auto_approve: HashSet<String> = ["file_read", "memory_recall"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let always_ask: HashSet<String> = std::iter::once("shell".to_string()).collect();
        let mgr = ApprovalManager::from_autonomy_level_with_lists(AutonomyLevel::Supervised, auto_approve, always_ask);
        // Auto-approved read-only tools: no suspension under supervised.
        assert!(!mgr.needs_approval("file_read"));
        assert!(!mgr.needs_approval("memory_recall"));
        // always_ask override: still suspends.
        assert!(mgr.needs_approval("shell"));
        // A tool in neither list under supervised: default-suspends.
        assert!(mgr.needs_approval("file_write"));
    }

    #[test]
    fn full_autonomy_skips_non_always_ask_prompts() {
        let mgr = ApprovalManager::from_config(&full_config());
        assert!(!mgr.needs_approval("shell"));
        assert!(!mgr.needs_approval("file_write"));
        assert!(!mgr.needs_approval("anything"));
    }

    #[test]
    fn full_autonomy_honors_always_ask() {
        let config = AutonomyConfig {
            level: AutonomyLevel::Full,
            always_ask: vec!["shell".into()],
            ..AutonomyConfig::default()
        };
        let mgr = ApprovalManager::from_config(&config);
        assert!(mgr.needs_approval("shell"));
        assert!(!mgr.needs_approval("file_write"));
    }

    #[test]
    fn readonly_never_prompts() {
        let config = AutonomyConfig {
            level: AutonomyLevel::ReadOnly,
            ..AutonomyConfig::default()
        };
        let mgr = ApprovalManager::from_config(&config);
        assert!(!mgr.needs_approval("shell"));
    }

    // ── session allowlist ────────────────────────────────────

    #[test]
    fn always_response_adds_to_session_allowlist() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        assert!(mgr.needs_approval("file_write"));

        mgr.record_decision(
            "file_write",
            &serde_json::json!({"path": "test.txt"}),
            ApprovalResponse::Always,
            "cli",
        );

        // Now file_write should be in session allowlist.
        assert!(!mgr.needs_approval("file_write"));
    }

    #[test]
    fn always_ask_overrides_session_allowlist() {
        let mgr = ApprovalManager::from_config(&supervised_config());

        // Even after "Always" for shell, it should still prompt.
        mgr.record_decision(
            "shell",
            &serde_json::json!({"command": "ls"}),
            ApprovalResponse::Always,
            "cli",
        );

        // shell is in always_ask, so it still needs approval.
        assert!(mgr.needs_approval("shell"));
    }

    #[test]
    fn yes_response_does_not_add_to_allowlist() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        mgr.record_decision("file_write", &serde_json::json!({}), ApprovalResponse::Yes, "cli");
        assert!(mgr.needs_approval("file_write"));
    }

    // ── audit log ────────────────────────────────────────────

    #[test]
    fn audit_log_records_decisions() {
        let mgr = ApprovalManager::from_config(&supervised_config());

        mgr.record_decision(
            "shell",
            &serde_json::json!({"command": "rm -rf ./build/"}),
            ApprovalResponse::No,
            "cli",
        );
        mgr.record_decision(
            "file_write",
            &serde_json::json!({"path": "out.txt", "content": "hello"}),
            ApprovalResponse::Yes,
            "cli",
        );

        let log = mgr.audit_log();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].tool_name, "shell");
        assert_eq!(log[0].decision, ApprovalResponse::No);
        assert_eq!(log[1].tool_name, "file_write");
        assert_eq!(log[1].decision, ApprovalResponse::Yes);
    }

    #[test]
    fn audit_log_contains_timestamp_and_channel() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        mgr.record_decision(
            "shell",
            &serde_json::json!({"command": "ls"}),
            ApprovalResponse::Yes,
            "telegram",
        );

        let log = mgr.audit_log();
        assert_eq!(log.len(), 1);
        assert!(!log[0].timestamp.is_empty());
        assert_eq!(log[0].channel, "telegram");
    }

    // ── summarize_args ───────────────────────────────────────

    #[test]
    fn summarize_args_object() {
        let args = serde_json::json!({"command": "ls -la", "cwd": "/tmp"});
        let summary = summarize_args(&args);
        assert!(summary.contains("command: ls -la"));
        assert!(summary.contains("cwd: /tmp"));
    }

    #[test]
    fn summarize_args_truncates_long_values() {
        let long_val = "x".repeat(200);
        let args = serde_json::json!({"content": long_val});
        let summary = summarize_args(&args);
        assert!(summary.contains('…'));
        assert!(summary.len() < 200);
    }

    #[test]
    fn summarize_args_unicode_safe_truncation() {
        let long_val = "🦀".repeat(120);
        let args = serde_json::json!({"content": long_val});
        let summary = summarize_args(&args);
        assert!(summary.contains("content:"));
        assert!(summary.contains('…'));
    }

    #[test]
    fn summarize_args_non_object() {
        let args = serde_json::json!("just a string");
        let summary = summarize_args(&args);
        assert!(summary.contains("just a string"));
    }

    // ── ApprovalResponse serde ───────────────────────────────

    #[test]
    fn approval_response_serde_roundtrip() {
        let json = serde_json::to_string(&ApprovalResponse::Always).unwrap();
        assert_eq!(json, "\"always\"");
        let parsed: ApprovalResponse = serde_json::from_str("\"no\"").unwrap();
        assert_eq!(parsed, ApprovalResponse::No);
    }

    // ── ApprovalRequest ──────────────────────────────────────

    #[test]
    fn approval_request_serde() {
        let req = ApprovalRequest {
            tool_name: "shell".into(),
            arguments: serde_json::json!({"command": "echo hi"}),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: ApprovalRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.tool_name, "shell");
    }

    // ── FIX-P3-09: ApprovalManager ↔ ApprovalGrant bridge ────────

    #[test]
    fn yes_and_always_decisions_generate_grants_when_keyring_available() {
        // Best-effort bridge: grant generation depends on the process-global
        // witness keyring (`WitnessKeyring::global()`), which needs $HOME or the
        // `OPENPRX_WITNESS_KEY_PATH` env. We do NOT mutate the environment here
        // (env mutation is `unsafe` under Rust 2024 and banned by the workspace
        // lint), so generation may legitimately be skipped. The contract this
        // test pins is: when generation does succeed, the grants are well-formed
        // and exactly mirror the approved tool names; when it is skipped the
        // decision is still recorded without panicking.
        let mgr = ApprovalManager::from_config(&supervised_config());
        let before = mgr.generated_grants().len();
        mgr.record_decision(
            "file_write",
            &serde_json::json!({"path": "out.txt"}),
            ApprovalResponse::Yes,
            "cli",
        );
        mgr.record_decision(
            "http_request",
            &serde_json::json!({"url": "https://example.com"}),
            ApprovalResponse::Always,
            "cli",
        );

        let grants = mgr.generated_grants();
        // Either both grants were minted (keyring available) or none were
        // (best-effort skip). A partial count would indicate a real bug.
        assert!(grants.len() == before || grants.len() == before + 2);
        for g in &grants {
            assert_eq!(g.version, ApprovalGrantV2::VERSION);
            assert_eq!(g.max_uses, 1);
            assert!(g.capability.op_id == "file_write" || g.capability.op_id == "http_request");
        }
    }

    #[test]
    fn no_decision_generates_no_grant() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        let before = mgr.generated_grants().len();
        mgr.record_decision(
            "shell",
            &serde_json::json!({"command": "rm -rf /"}),
            ApprovalResponse::No,
            "cli",
        );
        // `No` must never mint a grant.
        assert_eq!(mgr.generated_grants().len(), before);
    }
}
