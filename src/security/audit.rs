//! Audit logging for security events

use crate::config::AuditConfig;
use anyhow::Result;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use uuid::Uuid;

/// Audit event types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditEventType {
    CommandExecution,
    FileAccess,
    ConfigChange,
    AuthSuccess,
    AuthFailure,
    PolicyViolation,
    SecurityEvent,
    ToolGate,
}

/// Actor information (who performed the action)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Actor {
    pub channel: String,
    pub user_id: Option<String>,
    pub username: Option<String>,
}

/// Action information (what was done)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Action {
    pub command: Option<String>,
    pub risk_level: Option<String>,
    pub approved: bool,
    pub allowed: bool,
}

/// Execution result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionResult {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<u64>,
    pub error: Option<String>,
}

/// Security context
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityContext {
    pub policy_violation: bool,
    pub rate_limit_remaining: Option<u32>,
    pub sandbox_backend: Option<String>,
}

/// Complete audit event
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub timestamp: DateTime<Utc>,
    pub event_id: String,
    pub event_type: AuditEventType,
    pub actor: Option<Actor>,
    pub action: Option<Action>,
    pub result: Option<ExecutionResult>,
    pub security: SecurityContext,
}

impl AuditEvent {
    /// Create a new audit event
    pub fn new(event_type: AuditEventType) -> Self {
        Self {
            timestamp: Utc::now(),
            event_id: Uuid::new_v4().to_string(),
            event_type,
            actor: None,
            action: None,
            result: None,
            security: SecurityContext {
                policy_violation: false,
                rate_limit_remaining: None,
                sandbox_backend: None,
            },
        }
    }

    /// Set the actor
    pub fn with_actor(mut self, channel: String, user_id: Option<String>, username: Option<String>) -> Self {
        self.actor = Some(Actor {
            channel,
            user_id,
            username,
        });
        self
    }

    /// Set the action
    pub fn with_action(mut self, command: String, risk_level: String, approved: bool, allowed: bool) -> Self {
        self.action = Some(Action {
            command: Some(command),
            risk_level: Some(risk_level),
            approved,
            allowed,
        });
        self
    }

    /// Set the result
    pub fn with_result(
        mut self,
        success: bool,
        exit_code: Option<i32>,
        duration_ms: u64,
        error: Option<String>,
    ) -> Self {
        self.result = Some(ExecutionResult {
            success,
            exit_code,
            duration_ms: Some(duration_ms),
            error,
        });
        self
    }

    /// Set security context
    pub fn with_security(mut self, sandbox_backend: Option<String>) -> Self {
        self.security.sandbox_backend = sandbox_backend;
        self
    }
}

/// Audit logger
pub struct AuditLogger {
    log_path: PathBuf,
    /// FIX-P1-31 (alloc): the logger only ever reads `enabled` and `max_size_mb`
    /// from the audit config (both `Copy`); `log_path` is consumed once in `new`.
    /// Storing just these `Copy` scalars lets `new` borrow `&AuditConfig` instead
    /// of owning a clone, removing the per-decision `String` allocation that the
    /// side-effect gate audit hook previously paid on every authorized decision.
    enabled: bool,
    max_size_mb: u32,
    _buffer: Mutex<Vec<AuditEvent>>,
}

/// Structured command execution details for audit logging.
#[derive(Debug, Clone)]
pub struct CommandExecutionLog<'a> {
    pub channel: &'a str,
    pub command: &'a str,
    pub risk_level: &'a str,
    pub approved: bool,
    pub allowed: bool,
    pub success: bool,
    pub duration_ms: u64,
}

/// Structured side-effect gate decision details for audit logging.
///
/// Carries the EU AI Act Art.12 traceability fields for every authorization
/// decision: subject (`principal_id`), operation (`tool_name` + `operation_name`),
/// decision (`allowed`), deny `error` reason, and the `grant_id` correlation
/// handle. The event `timestamp` is stamped by [`AuditEvent::new`].
#[derive(Debug, Clone)]
pub struct SideEffectDecisionLog<'a> {
    pub tool_name: &'a str,
    pub operation_name: &'a str,
    pub risk_level: &'a str,
    pub approved: bool,
    pub allowed: bool,
    pub error: Option<&'a str>,
    /// Trusted caller principal id (audit subject / actor). `None` when no
    /// principal context is available (e.g. background runners).
    pub principal_id: Option<&'a str>,
    /// Stable approval-grant identifier for audit correlation, when a grant was
    /// presented. `None` when the decision was made without any grant.
    pub grant_id: Option<&'a str>,
}

impl AuditLogger {
    /// Create a new audit logger.
    ///
    /// Borrows the config (no clone): only the `Copy` scalars `enabled` and
    /// `max_size_mb` are retained, plus the resolved `log_path`.
    pub fn new(config: &AuditConfig, openprx_dir: PathBuf) -> Result<Self> {
        let configured = std::path::Path::new(&config.log_path);
        let log_path = if configured.is_absolute() {
            tracing::error!(
                path = %config.log_path,
                "Audit log path must be relative, got absolute path — using safe default"
            );
            openprx_dir.join("audit.log")
        } else {
            let joined = openprx_dir.join(configured);
            // Verify the resolved path does not escape the base directory via ../ traversal.
            // canonicalize may fail if the file does not yet exist, so fall back to the joined path.
            let canonical = joined.canonicalize().unwrap_or_else(|_| joined.clone());
            if !canonical.starts_with(&openprx_dir) {
                tracing::error!(
                    path = %config.log_path,
                    "Audit log path escapes base directory — using safe default"
                );
                openprx_dir.join("audit.log")
            } else {
                joined
            }
        };
        Ok(Self {
            log_path,
            enabled: config.enabled,
            max_size_mb: config.max_size_mb,
            _buffer: Mutex::new(Vec::new()),
        })
    }

    /// Log an event
    pub fn log(&self, event: &AuditEvent) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        // Check log size and rotate if needed
        self.rotate_if_needed()?;

        // Serialize and write
        let line = serde_json::to_string(event)?;
        let mut file = OpenOptions::new().create(true).append(true).open(&self.log_path)?;

        writeln!(file, "{}", line)?;
        file.sync_all()?;

        Ok(())
    }

    /// Log a command execution event.
    ///
    /// The command string is redacted to remove common secret patterns
    /// (tokens, passwords, API keys) before writing to the audit log.
    pub fn log_command_event(&self, entry: CommandExecutionLog<'_>) -> Result<()> {
        let redacted_command = redact_secrets(entry.command);
        let event = AuditEvent::new(AuditEventType::CommandExecution)
            .with_actor(entry.channel.to_string(), None, None)
            .with_action(
                redacted_command,
                entry.risk_level.to_string(),
                entry.approved,
                entry.allowed,
            )
            .with_result(entry.success, None, entry.duration_ms, None);

        self.log(&event)
    }

    /// Log a SideEffectGate decision for command or resource operations.
    ///
    /// Records the full EU AI Act Art.12 field set: timestamp (stamped by
    /// [`AuditEvent::new`]), subject (`principal_id` as the actor `user_id`),
    /// operation (`tool_name:operation_name`), decision (`allowed`), deny reason
    /// (`error`), and the `grant_id` correlation handle (folded into the action
    /// string so it survives in the structured `command` field).
    pub fn log_side_effect_decision(&self, entry: SideEffectDecisionLog<'_>) -> Result<()> {
        let mut action = format!("{}:{}", entry.tool_name, entry.operation_name);
        if let Some(grant_id) = entry.grant_id {
            // Append the grant correlation handle so a single structured field
            // ties the decision back to the authorizing grant.
            action.push_str(" grant_id=");
            action.push_str(grant_id);
        }
        let event = AuditEvent::new(AuditEventType::ToolGate)
            .with_actor(
                "side_effect_gate".to_string(),
                entry.principal_id.map(ToString::to_string),
                None,
            )
            .with_action(action, entry.risk_level.to_string(), entry.approved, entry.allowed)
            .with_result(entry.allowed, None, 0, entry.error.map(ToString::to_string));

        self.log(&event)
    }

    /// Backward-compatible helper to log a command execution event.
    #[allow(clippy::too_many_arguments)]
    pub fn log_command(
        &self,
        channel: &str,
        command: &str,
        risk_level: &str,
        approved: bool,
        allowed: bool,
        success: bool,
        duration_ms: u64,
    ) -> Result<()> {
        self.log_command_event(CommandExecutionLog {
            channel,
            command,
            risk_level,
            approved,
            allowed,
            success,
            duration_ms,
        })
    }

    /// Rotate log if it exceeds max size
    fn rotate_if_needed(&self) -> Result<()> {
        if let Ok(metadata) = std::fs::metadata(&self.log_path) {
            let current_size_mb = metadata.len() / (1024 * 1024);
            if current_size_mb >= u64::from(self.max_size_mb) {
                self.rotate()?;
            }
        }
        Ok(())
    }

    /// Rotate the log file
    fn rotate(&self) -> Result<()> {
        for i in (1..10).rev() {
            let old_name = format!("{}.{}.log", self.log_path.display(), i);
            let new_name = format!("{}.{}.log", self.log_path.display(), i + 1);
            let _ = std::fs::rename(&old_name, &new_name);
        }

        let rotated = format!("{}.1.log", self.log_path.display());
        std::fs::rename(&self.log_path, &rotated)?;
        Ok(())
    }
}

/// Best-effort production audit hook for SideEffectGate decisions.
///
/// Default/unit policies often use `.` as workspace; skip those to avoid writing
/// audit.log into the repo during tests. Runtime policies built from config have
/// an explicit workspace.
///
/// FIX-P1-31: the caller threads the real `security.audit` configuration through
/// `config` (previously this hard-coded `AuditConfig::default()`, so user config
/// was ignored). When `config.enabled` is false this returns immediately without
/// constructing a logger, writing a line, or issuing an `fsync` — disabling
/// audit now genuinely removes the per-decision sync cost on the gate path.
pub fn record_side_effect_decision_best_effort(
    workspace_dir: &Path,
    config: &AuditConfig,
    entry: SideEffectDecisionLog<'_>,
) {
    if !config.enabled {
        return;
    }
    if workspace_dir.as_os_str().is_empty() || workspace_dir == Path::new(".") {
        return;
    }
    let Ok(logger) = AuditLogger::new(config, workspace_dir.to_path_buf()) else {
        return;
    };
    if let Err(error) = logger.log_side_effect_decision(entry) {
        tracing::debug!(error = %error, "failed to write side-effect audit event");
    }
}

/// Redact common secret patterns from a command string before audit logging.
///
/// Replaces values that look like API keys, tokens, passwords, and credential
/// URLs with `[REDACTED]` to prevent accidental secret exposure in logs.
fn redact_secrets(command: &str) -> String {
    #[allow(clippy::expect_used)]
    static SECRET_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
        vec![
            // key=value and key:value patterns for known secret names
            Regex::new(r"(?i)((?:token|key|secret|password|passwd|api[_-]?key|auth)\s*[=:]\s*)\S+")
                .expect("BUG: invalid hardcoded secret-value regex"),
            // Bearer tokens in headers / arguments
            Regex::new(r"(?i)(Bearer\s+)\S+").expect("BUG: invalid hardcoded bearer regex"),
            // URLs with embedded credentials  (user:pass@host)
            Regex::new(r"(https?://)[^\s@]+@").expect("BUG: invalid hardcoded cred-url regex"),
        ]
    });

    let mut result = command.to_string();
    for re in SECRET_PATTERNS.iter() {
        result = re.replace_all(&result, "${1}[REDACTED]").to_string();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn audit_event_new_creates_unique_id() {
        let event1 = AuditEvent::new(AuditEventType::CommandExecution);
        let event2 = AuditEvent::new(AuditEventType::CommandExecution);
        assert_ne!(event1.event_id, event2.event_id);
    }

    #[test]
    fn audit_event_with_actor() {
        let event = AuditEvent::new(AuditEventType::CommandExecution).with_actor(
            "telegram".to_string(),
            Some("123".to_string()),
            Some("@alice".to_string()),
        );

        assert!(event.actor.is_some());
        let actor = event.actor.as_ref().unwrap();
        assert_eq!(actor.channel, "telegram");
        assert_eq!(actor.user_id, Some("123".to_string()));
        assert_eq!(actor.username, Some("@alice".to_string()));
    }

    #[test]
    fn audit_event_with_action() {
        let event = AuditEvent::new(AuditEventType::CommandExecution).with_action(
            "ls -la".to_string(),
            "low".to_string(),
            false,
            true,
        );

        assert!(event.action.is_some());
        let action = event.action.as_ref().unwrap();
        assert_eq!(action.command, Some("ls -la".to_string()));
        assert_eq!(action.risk_level, Some("low".to_string()));
    }

    #[test]
    fn audit_event_serializes_to_json() {
        let event = AuditEvent::new(AuditEventType::CommandExecution)
            .with_actor("telegram".to_string(), None, None)
            .with_action("ls".to_string(), "low".to_string(), false, true)
            .with_result(true, Some(0), 15, None);

        let json = serde_json::to_string(&event);
        assert!(json.is_ok());
        let json = json.expect("serialize");
        let parsed: AuditEvent = serde_json::from_str(json.as_str()).expect("parse");
        assert!(parsed.actor.is_some());
        assert!(parsed.action.is_some());
        assert!(parsed.result.is_some());
    }

    #[test]
    fn audit_side_effect_decision_uses_tool_gate_event_type() -> Result<()> {
        let tmp = TempDir::new()?;
        let logger = AuditLogger::new(&AuditConfig::default(), tmp.path().to_path_buf())?;

        logger.log_side_effect_decision(SideEffectDecisionLog {
            tool_name: "file_write",
            operation_name: "file_write:write:abc",
            risk_level: "medium",
            approved: false,
            allowed: false,
            error: Some("requires approval"),
            principal_id: None,
            grant_id: None,
        })?;

        let log = std::fs::read_to_string(tmp.path().join("audit.log"))?;
        assert!(log.contains(r#""event_type":"tool_gate""#));
        assert!(log.contains("file_write:file_write:write:abc"));
        Ok(())
    }

    #[test]
    fn audit_side_effect_decision_records_compliance_fields() -> Result<()> {
        // EU AI Act Art.12: every gate decision audit entry must carry the
        // subject (principal_id), operation, decision, deny reason, and grant_id.
        let tmp = TempDir::new()?;
        let logger = AuditLogger::new(&AuditConfig::default(), tmp.path().to_path_buf())?;

        logger.log_side_effect_decision(SideEffectDecisionLog {
            tool_name: "file_write",
            operation_name: "file_write:write:abc",
            risk_level: "medium",
            approved: true,
            allowed: true,
            error: None,
            principal_id: Some("telegram:alice"),
            grant_id: Some("grant-123"),
        })?;

        let raw = std::fs::read_to_string(tmp.path().join("audit.log"))?;
        let event: AuditEvent = serde_json::from_str(raw.trim()).expect("parse audit event");
        assert!(event.timestamp <= Utc::now());
        let actor = event.actor.expect("actor present");
        assert_eq!(actor.user_id.as_deref(), Some("telegram:alice"));
        let action = event.action.expect("action present");
        let command = action.command.expect("command present");
        assert!(command.contains("file_write:file_write:write:abc"));
        assert!(command.contains("grant_id=grant-123"));
        assert!(action.allowed);
        Ok(())
    }

    #[test]
    fn audit_logger_disabled_does_not_create_file() -> Result<()> {
        let tmp = TempDir::new()?;
        let config = AuditConfig {
            enabled: false,
            ..Default::default()
        };
        let logger = AuditLogger::new(&config, tmp.path().to_path_buf())?;
        let event = AuditEvent::new(AuditEventType::CommandExecution);

        logger.log(&event)?;

        // File should not exist since logging is disabled
        assert!(!tmp.path().join("audit.log").exists());
        Ok(())
    }

    // ── §8.1 Log rotation tests ─────────────────────────────

    #[tokio::test]
    async fn audit_logger_writes_event_when_enabled() -> Result<()> {
        let tmp = TempDir::new()?;
        let config = AuditConfig {
            enabled: true,
            max_size_mb: 10,
            ..Default::default()
        };
        let logger = AuditLogger::new(&config, tmp.path().to_path_buf())?;
        let event = AuditEvent::new(AuditEventType::CommandExecution)
            .with_actor("cli".to_string(), None, None)
            .with_action("ls".to_string(), "low".to_string(), false, true);

        logger.log(&event)?;

        let log_path = tmp.path().join("audit.log");
        assert!(log_path.exists(), "audit log file must be created");

        let content = tokio::fs::read_to_string(&log_path).await?;
        assert!(!content.is_empty(), "audit log must not be empty");

        let parsed: AuditEvent = serde_json::from_str(content.trim())?;
        assert!(parsed.action.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn audit_log_command_event_writes_structured_entry() -> Result<()> {
        let tmp = TempDir::new()?;
        let config = AuditConfig {
            enabled: true,
            max_size_mb: 10,
            ..Default::default()
        };
        let logger = AuditLogger::new(&config, tmp.path().to_path_buf())?;

        logger.log_command_event(CommandExecutionLog {
            channel: "telegram",
            command: "echo test",
            risk_level: "low",
            approved: false,
            allowed: true,
            success: true,
            duration_ms: 42,
        })?;

        let log_path = tmp.path().join("audit.log");
        let content = tokio::fs::read_to_string(&log_path).await?;
        let parsed: AuditEvent = serde_json::from_str(content.trim())?;

        let action = parsed.action.unwrap();
        assert_eq!(action.command, Some("echo test".to_string()));
        assert_eq!(action.risk_level, Some("low".to_string()));
        assert!(action.allowed);

        let result = parsed.result.unwrap();
        assert!(result.success);
        assert_eq!(result.duration_ms, Some(42));
        Ok(())
    }

    #[test]
    fn audit_rotation_creates_numbered_backup() -> Result<()> {
        let tmp = TempDir::new()?;
        let config = AuditConfig {
            enabled: true,
            max_size_mb: 0, // Force rotation on first write
            ..Default::default()
        };
        let logger = AuditLogger::new(&config, tmp.path().to_path_buf())?;

        // Write initial content that triggers rotation
        let log_path = tmp.path().join("audit.log");
        std::fs::write(&log_path, "initial content\n")?;

        let event = AuditEvent::new(AuditEventType::CommandExecution);
        logger.log(&event)?;

        let rotated = format!("{}.1.log", log_path.display());
        assert!(
            std::path::Path::new(&rotated).exists(),
            "rotation must create .1.log backup"
        );
        Ok(())
    }
}
