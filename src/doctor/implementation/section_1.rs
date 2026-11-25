use crate::config::Config;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::collections::HashSet;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub use ingress::{diagnose_ingress, run_ingress};
const DAEMON_STALE_SECONDS: i64 = 30;
const SCHEDULER_STALE_SECONDS: i64 = 120;
const CHANNEL_STALE_SECONDS: i64 = 300;
const COMMAND_VERSION_PREVIEW_CHARS: usize = 60;

// ── Diagnostic item ──────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Ok,
    Warn,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiagnosticState {
    Declared,
    Configured,
    Ready,
    Healthy,
    Disabled,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct DiagItem {
    pub severity: Severity,
    pub state: DiagnosticState,
    pub category: &'static str,
    pub message: String,
}

impl DiagItem {
    fn ok(category: &'static str, msg: impl Into<String>) -> Self {
        Self::with_state(Severity::Ok, DiagnosticState::Healthy, category, msg)
    }
    fn healthy(category: &'static str, msg: impl Into<String>) -> Self {
        Self::ok(category, msg)
    }
    fn declared(category: &'static str, msg: impl Into<String>) -> Self {
        Self::with_state(Severity::Ok, DiagnosticState::Declared, category, msg)
    }
    fn configured(category: &'static str, msg: impl Into<String>) -> Self {
        Self::with_state(Severity::Ok, DiagnosticState::Configured, category, msg)
    }
    fn ready(category: &'static str, msg: impl Into<String>) -> Self {
        Self::with_state(Severity::Ok, DiagnosticState::Ready, category, msg)
    }
    fn disabled(category: &'static str, msg: impl Into<String>) -> Self {
        Self::with_state(Severity::Ok, DiagnosticState::Disabled, category, msg)
    }
    fn unknown(category: &'static str, msg: impl Into<String>) -> Self {
        Self::with_state(Severity::Warn, DiagnosticState::Unknown, category, msg)
    }
    /// Every item is built here, so this is the one place a message is made
    /// safe to print: doctor quotes configured provider labels and endpoint
    /// URLs, and a `custom:` URL may carry `user:password@`.
    fn with_state(severity: Severity, state: DiagnosticState, category: &'static str, msg: impl Into<String>) -> Self {
        let message = crate::security::redaction::redact_secrets(&msg.into());
        let message = match crate::security::redaction::redact_url_components(&message) {
            std::borrow::Cow::Borrowed(_) => message,
            std::borrow::Cow::Owned(redacted) => redacted,
        };
        Self {
            severity,
            state,
            category,
            message,
        }
    }
    fn warn(category: &'static str, msg: impl Into<String>) -> Self {
        Self::with_state(Severity::Warn, DiagnosticState::Unknown, category, msg)
    }
    fn error(category: &'static str, msg: impl Into<String>) -> Self {
        Self::with_state(Severity::Error, DiagnosticState::Unknown, category, msg)
    }

    const fn icon(&self) -> &'static str {
        match self.severity {
            Severity::Ok => "✅",
            Severity::Warn => "⚠️ ",
            Severity::Error => "❌",
        }
    }

    const fn state_label(&self) -> &'static str {
        match self.state {
            DiagnosticState::Declared => "DECLARED",
            DiagnosticState::Configured => "CONFIGURED",
            DiagnosticState::Ready => "READY",
            DiagnosticState::Healthy => "HEALTHY",
            DiagnosticState::Disabled => "DISABLED",
            DiagnosticState::Unknown => "UNKNOWN",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DoctorReport {
    pub title: &'static str,
    pub items: Vec<DiagItem>,
    pub ok_count: usize,
    pub warning_count: usize,
    pub error_count: usize,
}

impl DoctorReport {
    fn new(title: &'static str, items: Vec<DiagItem>) -> Self {
        let ok_count = items.iter().filter(|item| item.severity == Severity::Ok).count();
        let warning_count = items.iter().filter(|item| item.severity == Severity::Warn).count();
        let error_count = items.iter().filter(|item| item.severity == Severity::Error).count();
        Self {
            title,
            items,
            ok_count,
            warning_count,
            error_count,
        }
    }

    pub const fn has_errors(&self) -> bool {
        self.error_count > 0
    }

    fn print(&self) {
        println!("🩺 {}", self.title);
        println!();

        let mut current_cat = "";
        for item in &self.items {
            if item.category != current_cat {
                current_cat = item.category;
                println!("  [{current_cat}]");
            }
            println!("    {} [{}] {}", item.icon(), item.state_label(), item.message);
        }

        println!();
        println!(
            "  Summary: {} ok, {} warnings, {} errors",
            self.ok_count, self.warning_count, self.error_count
        );
        if self.has_errors() {
            println!("  💡 Fix the errors above, then run `prx doctor` again.");
        }
    }

    fn ensure_success(&self) -> Result<()> {
        if self.has_errors() {
            anyhow::bail!("doctor found {} error(s)", self.error_count);
        }
        Ok(())
    }
}

// ── Public entry point ───────────────────────────────────────────

pub fn run(config: &Config) -> Result<()> {
    let report = diagnose(config);
    report.print();
    report.ensure_success()
}

pub fn diagnose(config: &Config) -> DoctorReport {
    let mut items: Vec<DiagItem> = Vec::new();

    check_config_semantics(config, &mut items);
    check_model_selection(config, &mut items);
    check_persisted_selections(config, &mut items);
    #[cfg(feature = "llm-router")]
    check_router_candidates(config, &mut items);
    check_decision_plane(config, &mut items);
    check_workspace(config, &mut items);
    ingress::check_recovery_authority(config, &mut items);
    workers::check_retained_worker_folders(config, &mut items);
    check_legacy_fitness_telemetry(config, &mut items);
    check_workspace_mcp_servers(config, &mut items);
    check_workspace_hooks(config, &mut items);
    check_unfinished_config_changes(config, &mut items);
    check_daemon_state(config, &mut items);
    prompt_cache::check_prompt_cache(config, &mut items);
    decision_engine::check(config, &mut items);
    provider_circuits::check_provider_circuits(config, &mut items);
    check_daemon_process_path(config, &mut items);
    check_environment(&mut items);

    DoctorReport::new("OpenPRX Doctor", items)
}

pub fn run_memory(config: &Config) -> Result<()> {
    let report = diagnose_memory(config);
    report.print();
    report.ensure_success()
}

pub fn diagnose_memory(config: &Config) -> DoctorReport {
    let mut items: Vec<DiagItem> = Vec::new();
    check_memory_diagnostics(config, &mut items);
    check_legacy_fitness_telemetry(config, &mut items);
    DoctorReport::new("OpenPRX Doctor - Memory", items)
}

/// Workspace command hooks (HOOKS-ENV): a `hooks.json` whose commands do
/// not run because `security.workspace_hooks` is off is named, with the one
/// line that allows them. Reads the file only.
fn check_workspace_hooks(config: &Config, items: &mut Vec<DiagItem>) {
    let category = "hooks";
    match crate::hooks::workspace_hook_counts(&config.workspace_dir) {
        Ok(None | Some((0, _))) => items.push(DiagItem::healthy(category, "no workspace hooks")),
        Ok(Some((actions, webhooks))) if config.security.workspace_hooks == crate::config::WorkspaceHooks::On => {
            let webhook_note = if webhooks.is_empty() {
                String::new()
            } else {
                format!("; {} webhook(s): {}", webhooks.len(), webhooks.join(", "))
            };
            items.push(DiagItem::ok(
                category,
                format!(
                    "{actions} workspace hook(s) run (security.workspace_hooks = \"on\"); command hooks in a \
                     cleared environment and a dedicated directory beside config.toml{webhook_note}"
                ),
            ))
        }
        Ok(Some((actions, _))) => {
            if let Some(note) = crate::hooks::blocked_hooks_note(actions) {
                items.push(DiagItem::warn(category, note));
            }
        }
        Err(error) => items.push(DiagItem::warn(
            category,
            format!("the workspace hooks.json is not valid: {error:#}"),
        )),
    }
}

/// Unfinished configuration changes (SP-PMM §2.6, D-095 L-3): a `pending`
/// history record (the process stopped between commit and activation) or a
/// `not_restored` one (a refused change whose restore failed). Nothing is
/// applied automatically; the operator decides. Reads the history only.
fn check_unfinished_config_changes(config: &Config, items: &mut Vec<DiagItem>) {
    use crate::config::manage::{ConfigHistory, RecordStatus};
    let category = "config";
    let records = match ConfigHistory::for_config(&config.config_path).and_then(|history| history.list()) {
        Ok(records) => records,
        Err(error) => {
            items.push(DiagItem::warn(
                category,
                format!("configuration history could not be read: {error:#}"),
            ));
            return;
        }
    };
    // A change `prx` wrote that a later write replaced before a running PRX
    // settled it (D-101 F-7): superseded, never activated.
    let current = crate::config::files::current_config_revision(&config.config_path).ok();
    for record in records.iter().filter(|record| {
        record.status == RecordStatus::Superseded
            || (record.status == RecordStatus::ActivationDeferred
                && current.is_some()
                && record.committed_revision != current)
    }) {
        items.push(DiagItem::warn(
            category,
            format!(
                "configuration change {} written by `prx` was replaced by a later write before a running PRX \
                 settled it (superseded); inspect it with `prx config history show {}`",
                record.id, record.id
            ),
        ));
    }
    let unfinished = records
        .iter()
        .filter(|record| matches!(record.status, RecordStatus::Pending | RecordStatus::NotRestored))
        .collect::<Vec<_>>();
    if unfinished.is_empty() {
        items.push(DiagItem::healthy(category, "no unfinished configuration change"));
        return;
    }
    for record in unfinished {
        items.push(DiagItem::warn(
            category,
            format!(
                "configuration change {} is {} (the process stopped, or a restore failed); it is never applied \
                 automatically: inspect it with `prx config history show {}` and restore the configuration from \
                 before it with `prx config rollback {} --yes`",
                record.id,
                record.status.as_str(),
                record.id,
                record.id
            ),
        ));
    }
}

/// Every workspace `mcp.json` entry that did not start as written (blocked
/// by `mcp.workspace_servers`, disabled, or trimmed), with its one-line
/// remedy (D-088 C-7). Reads the file only; starts nothing.
fn check_workspace_mcp_servers(config: &Config, items: &mut Vec<DiagItem>) {
    let category = "mcp";
    let notes = crate::tools::mcp::workspace_server_notes(&config.mcp, &config.workspace_dir, config.autonomy.level);
    if notes.is_empty() {
        items.push(DiagItem::healthy(
            category,
            "no workspace mcp.json server is blocked or disabled",
        ));
    }
    for note in notes {
        items.push(DiagItem::warn(category, note));
    }
    // An operator's `working_dir` inside the agent's workspace lets the
    // workspace file tools plant files the server loads at start (a
    // `.npmrc`, a shadowing module): allowed, but worth saying (D-095 N-4).
    let mut servers = config.mcp.servers.iter().collect::<Vec<_>>();
    servers.sort_by(|left, right| left.0.cmp(right.0));
    for (name, server) in servers {
        if let Some(dir) = server.working_dir.as_deref() {
            if std::path::Path::new(dir.trim()).starts_with(&config.workspace_dir) {
                items.push(DiagItem::warn(
                    category,
                    format!(
                        "server '{}' starts in {}, inside the agent's workspace: the workspace file tools can \
                         change what it loads there; prefer a directory outside the workspace, or leave \
                         `working_dir` unset for its dedicated one",
                        name.escape_default(),
                        dir.trim()
                    ),
                ));
            }
        }
    }
}

fn check_legacy_fitness_telemetry(config: &Config, items: &mut Vec<DiagItem>) {
    const PREFIX: &str = "self/fitness/daily/";
    let category = "fitness";
    match legacy_fitness_counts(config, PREFIX) {
        Ok((database, memory_projection, snapshot_projection)) => {
            let total = database + memory_projection + snapshot_projection;
            if total == 0 {
                items.push(DiagItem::healthy(
                    category,
                    "legacy fitness telemetry absent from memory storage and projections",
                ));
            } else {
                items.push(DiagItem::warn(
                    category,
                    format!(
                        "legacy fitness telemetry detected: database={database}, MEMORY.md={memory_projection}, \
                         MEMORY_SNAPSHOT.md={snapshot_projection}; run `prx fitness migrate-legacy --apply`"
                    ),
                ));
            }
        }
        Err(error) => items.push(DiagItem::warn(
            category,
            format!("legacy fitness telemetry inspection failed: {error}"),
        )),
    }
}

fn legacy_fitness_counts(config: &Config, prefix: &str) -> Result<(usize, usize, usize)> {
    let backend =
        crate::memory::effective_memory_backend_name(&config.memory.backend, Some(&config.storage.provider.config));
    let database = match crate::memory::classify_memory_backend(&backend) {
        crate::memory::MemoryBackendKind::Sqlite | crate::memory::MemoryBackendKind::Lucid => {
            read_only_sqlite_key_prefix_count(&crate::schema_migration::memory_db_path(config), prefix)?
        }
        crate::memory::MemoryBackendKind::Postgres => {
            let storage = &config.storage.provider.config;
            let db_url = storage
                .db_url
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .context("postgres fitness inspection requires storage.provider.config.db_url")?;
            crate::memory::PostgresMemory::read_key_prefix_read_only(
                db_url,
                &storage.schema,
                &storage.table,
                storage.connect_timeout_secs,
                prefix,
            )?
            .len()
        }
        crate::memory::MemoryBackendKind::Markdown | crate::memory::MemoryBackendKind::None => 0,
        crate::memory::MemoryBackendKind::Unknown => {
            anyhow::bail!("unsupported configured memory backend '{backend}'")
        }
    };
    let memory_projection = count_projection_lines(&config.workspace_dir.join("MEMORY.md"), |line| {
        line.strip_prefix("- [")
            .and_then(|rest| rest.split_once("] **"))
            .and_then(|(_, rest)| rest.split_once("**:"))
            .is_some_and(|(key, _)| key.starts_with(prefix))
    })?;
    let snapshot_projection = count_projection_lines(
        &config.workspace_dir.join(crate::memory::snapshot::SNAPSHOT_FILENAME),
        |line| {
            line.strip_prefix("### 🔑 `")
                .and_then(|rest| rest.strip_suffix('`'))
                .is_some_and(|key| key.starts_with(prefix))
        },
    )?;
    Ok((database, memory_projection, snapshot_projection))
}

fn read_only_sqlite_key_prefix_count(db_path: &Path, prefix: &str) -> Result<usize> {
    if !db_path.is_file() {
        return Ok(0);
    }
    let connection = rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let table_exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'memories')",
        [],
        |row| row.get(0),
    )?;
    if !table_exists {
        return Ok(0);
    }
    let escaped = prefix.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
    let pattern = format!("{escaped}%");
    let count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM memories WHERE key LIKE ?1 ESCAPE '\\'",
        [pattern],
        |row| row.get(0),
    )?;
    usize::try_from(count).context("SQLite returned a negative legacy fitness count")
}

fn count_projection_lines(path: &Path, matches: impl Fn(&str) -> bool) -> Result<usize> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(content.lines().filter(|line| matches(line)).count()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

pub async fn run_runtime(config: &Config) -> Result<()> {
    let report = diagnose_runtime(config).await;
    report.print();
    report.ensure_success()
}

pub async fn diagnose_runtime(config: &Config) -> DoctorReport {
    let mut items: Vec<DiagItem> = Vec::new();
    check_deployed_binary(&mut items);
    check_runtime_config(config, &mut items);
    check_daemon_state(config, &mut items);
    prompt_cache::check_prompt_cache(config, &mut items);
    decision_engine::check(config, &mut items);
    provider_circuits::check_provider_circuits(config, &mut items);
    check_daemon_process_path(config, &mut items);
    check_gateway_runtime(config, &mut items);
    check_channel_runtime(config, &mut items);
    check_console_runtime(config, &mut items);
    check_runtime_memory_health(config, &mut items);
    check_runtime_readiness(config, &mut items);
    check_postgres_health(config, &mut items).await;
    check_embedding_endpoint(config, &mut items).await;
    DoctorReport::new("OpenPRX Doctor - Runtime Matrix", items)
}

fn check_deployed_binary(items: &mut Vec<DiagItem>) {
    check_deployed_binary_paths(items, deployed_binary_from_path(), std::env::current_exe());
}

fn deployed_binary_from_path() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let binary_name = format!("prx{}", std::env::consts::EXE_SUFFIX);
    std::env::split_paths(&path)
        .map(|directory| directory.join(&binary_name))
        .find(|candidate| candidate.is_file())
}

fn same_executable_path(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn check_deployed_binary_paths(
    items: &mut Vec<DiagItem>,
    deployed: Option<PathBuf>,
    current: std::io::Result<PathBuf>,
) {
    let cat = "deployed";
    let Some(deployed) = deployed else {
        items.push(DiagItem::error(cat, "deployed prx binary not found on PATH"));
        if let Ok(current) = current {
            items.push(DiagItem::warn(
                cat,
                format!(
                    "doctor is running from {}, but no deployed prx binary was found on PATH",
                    current.display()
                ),
            ));
        }
        return;
    };

    if deployed.is_file() {
        items.push(DiagItem::ok(
            cat,
            format!("deployed binary exists: {}", deployed.display()),
        ));
    } else {
        items.push(DiagItem::error(
            cat,
            format!("deployed binary missing: {}", deployed.display()),
        ));
    }

    match current {
        Ok(current) if same_executable_path(&current, &deployed) => {
            items.push(DiagItem::ok(cat, "doctor is running from deployed binary"));
        }
        Ok(current) => items.push(DiagItem::warn(
            cat,
            format!("doctor is running from {}, not deployed binary", current.display()),
        )),
        Err(error) => items.push(DiagItem::warn(
            cat,
            format!("cannot resolve current executable: {error}"),
        )),
    }
}

fn check_runtime_config(config: &Config, items: &mut Vec<DiagItem>) {
    let cat = "config";
    if config.config_path.exists() {
        items.push(DiagItem::configured(
            cat,
            format!("config path exists: {}", config.config_path.display()),
        ));
    } else {
        items.push(DiagItem::error(
            cat,
            format!("config path missing: {}", config.config_path.display()),
        ));
    }
    if config.workspace_dir.exists() {
        items.push(DiagItem::configured(
            cat,
            format!("workspace exists: {}", config.workspace_dir.display()),
        ));
    } else {
        items.push(DiagItem::error(
            cat,
            format!("workspace missing: {}", config.workspace_dir.display()),
        ));
    }
}

fn check_gateway_runtime(config: &Config, items: &mut Vec<DiagItem>) {
    let cat = "gateway";
    items.push(DiagItem::configured(
        cat,
        format!(
            "configured bind {}:{} (pairing required: {})",
            config.gateway.host, config.gateway.port, config.gateway.require_pairing
        ),
    ));

    let address = format!("{}:{}", config.gateway.host, config.gateway.port);
    match crate::tools::http_request::lookup_host_blocking(&config.gateway.host, config.gateway.port)
        .ok()
        .and_then(|addrs| addrs.into_iter().next())
    {
        Some(socket) => match TcpStream::connect_timeout(&socket, Duration::from_millis(300)) {
            Ok(_) => items.push(DiagItem::ok(cat, format!("gateway port reachable at {socket}"))),
            Err(error) => items.push(DiagItem::warn(
                cat,
                format!("gateway port not reachable at {socket}: {error}"),
            )),
        },
        None => items.push(DiagItem::warn(
            cat,
            format!("cannot resolve gateway bind address {address}"),
        )),
    }
}

fn check_channel_runtime(config: &Config, items: &mut Vec<DiagItem>) {
    let cat = "channels";
    let configured = configured_channel_names(config);
    if configured.is_empty() {
        items.push(DiagItem::warn(cat, "no IM channels are configured"));
    } else {
        items.push(DiagItem::configured(
            cat,
            format!("configured channels: {}", configured.join(", ")),
        ));
    }
}

fn configured_channel_names(config: &Config) -> Vec<&'static str> {
    let mut names = Vec::new();
    if config.channels_config.cli {
        names.push("cli");
    }
    if config.channels_config.telegram.is_some() {
        names.push("telegram");
    }
    if config.channels_config.discord.is_some() {
        names.push("discord");
    }
    if config.channels_config.slack.is_some() {
        names.push("slack");
    }
    if config.channels_config.webhook.is_some() {
        names.push("webhook");
    }
    if config.channels_config.signal.is_some() {
        names.push("signal");
    }
    if config.channels_config.whatsapp.is_some() {
        names.push("whatsapp");
    }
    if config.channels_config.wacli.is_some() {
        names.push("wacli");
    }
    if config.channels_config.nextcloud_talk.is_some() {
        names.push("nextcloud-talk");
    }
    if config.channels_config.email.is_some() {
        names.push("email");
    }
    if config.channels_config.irc.is_some() {
        names.push("irc");
    }
    if config.channels_config.lark.is_some() {
        names.push("lark");
    }
    names
}

fn check_console_runtime(config: &Config, items: &mut Vec<DiagItem>) {
    let cat = "console";
    let backend =
        crate::memory::effective_memory_backend_name(&config.memory.backend, Some(&config.storage.provider.config));
    match crate::memory::classify_memory_backend(&backend) {
        crate::memory::MemoryBackendKind::Sqlite | crate::memory::MemoryBackendKind::Lucid => {
            let db_path = crate::schema_migration::memory_db_path(config);
            match read_only_sqlite_session_count(&db_path) {
                Ok(Some(0)) => items.push(DiagItem::ready(cat, "no persisted console/channel sessions found")),
                Ok(Some(count)) => items.push(DiagItem::healthy(
                    cat,
                    format!("persisted conversation sessions visible: {count}"),
                )),
                Ok(None) => items.push(DiagItem::unknown(
                    cat,
                    "sessions table is not present; session visibility is unknown",
                )),
                Err(error) => items.push(DiagItem::error(
                    cat,
                    format!("read-only console session probe failed: {error}"),
                )),
            }
        }
        crate::memory::MemoryBackendKind::Postgres => items.push(DiagItem::unknown(
            cat,
            "PostgreSQL console session count is not inferred; backend health is probed separately",
        )),
        crate::memory::MemoryBackendKind::Markdown | crate::memory::MemoryBackendKind::None => {
            items.push(DiagItem::disabled(
                cat,
                format!("backend '{backend}' does not provide durable conversation sessions"),
            ));
        }
        crate::memory::MemoryBackendKind::Unknown if backend.starts_with("wasm:") => items.push(DiagItem::unknown(
            cat,
            format!("WASM backend '{backend}' conversation persistence is defined by the plugin"),
        )),
        crate::memory::MemoryBackendKind::Unknown => items.push(DiagItem::error(
            cat,
            format!("unknown configured memory backend '{backend}'"),
        )),
    }
}

fn check_runtime_memory_health(config: &Config, items: &mut Vec<DiagItem>) {
    let cat = "memory";
    let backend =
        crate::memory::effective_memory_backend_name(&config.memory.backend, Some(&config.storage.provider.config));
    match crate::memory::classify_memory_backend(&backend) {
        crate::memory::MemoryBackendKind::None => {
            items.push(DiagItem::disabled(cat, "memory backend explicitly disabled"));
        }
        crate::memory::MemoryBackendKind::Markdown => {
            if config.workspace_dir.is_dir() {
                items.push(DiagItem::ready(
                    cat,
                    "markdown memory workspace is readable; no write probe performed",
                ));
            } else {
                items.push(DiagItem::error(cat, "markdown memory workspace is missing"));
            }
        }
        crate::memory::MemoryBackendKind::Sqlite
        | crate::memory::MemoryBackendKind::Lucid
        | crate::memory::MemoryBackendKind::Postgres => {
            match crate::schema_migration::inspect_configured_backend(config) {
                Ok(report) if report.status.pending.is_empty() => items.push(DiagItem::healthy(
                    cat,
                    format!(
                        "{} backend authoritative ledger verified ({} migrations)",
                        report.backend,
                        report.status.applied.len()
                    ),
                )),
                Ok(report) => items.push(DiagItem::with_state(
                    Severity::Warn,
                    DiagnosticState::Ready,
                    cat,
                    format!(
                        "{} backend ledger is readable with {} pending migration(s)",
                        report.backend,
                        report.status.pending.len()
                    ),
                )),
                Err(error) => items.push(DiagItem::error(
                    cat,
                    format!("read-only {backend} backend probe failed: {error}"),
                )),
            }
        }
        crate::memory::MemoryBackendKind::Unknown if backend.starts_with("wasm:") => {
            items.push(DiagItem::ready(
                cat,
                format!("WASM memory backend '{backend}' will be health-checked by the live plugin runtime"),
            ));
        }
        crate::memory::MemoryBackendKind::Unknown => {
            items.push(DiagItem::error(
                cat,
                format!("unknown configured memory backend '{backend}'"),
            ));
        }
    }
}

fn read_only_sqlite_session_count(db_path: &Path) -> Result<Option<i64>> {
    if !db_path.is_file() {
        anyhow::bail!("memory database is missing at {}", db_path.display());
    }
    let conn = rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'sessions')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(None);
    }
    conn.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
        .map(Some)
        .map_err(Into::into)
}

/// FIX-P2-06: Read-only runtime readiness sub-checks.
///
/// Each verifies that the configuration is internally consistent enough for the
/// corresponding subsystem to operate: owner ACL, topic scoping, task lineage,
/// document ingest, vector index, and the runtime-control ladder. These are
/// config-derived probes only -- they do not open the database or touch the
/// network.
fn check_runtime_readiness(config: &Config, items: &mut Vec<DiagItem>) {
    let cat = "readiness";
    let mem = &config.memory;
    let backend = mem.backend.trim().to_ascii_lowercase();
    let persistent = !matches!(backend.as_str(), "none" | "markdown");
    let embeddings_enabled = !mem.embedding_provider.trim().eq_ignore_ascii_case("none");

    // 1. owner readiness: ACL enforcement requires a persistent backend.
    if !mem.acl_enabled {
        items.push(DiagItem::warn(
            cat,
            "owner: memory.acl_enabled is false; owner/topic scoping is not enforced",
        ));
    } else if persistent {
        items.push(DiagItem::ready(cat, "owner: ACL enforced on a persistent backend"));
    } else {
        items.push(DiagItem::warn(
            cat,
            format!("owner: ACL enabled but backend '{backend}' does not persist owner scopes"),
        ));
    }

    // 2. topic readiness: similarity/semantic scoping needs a persistent
    // backend and embeddings.
    if persistent && embeddings_enabled && mem.embedding_dimensions > 0 {
        let mode = if mem.embedding_provider.trim().eq_ignore_ascii_case("local") {
            "local similarity"
        } else {
            "semantic"
        };
        items.push(DiagItem::ready(
            cat,
            format!("topic: {mode} topic scoping ready (dim {})", mem.embedding_dimensions),
        ));
    } else if !persistent {
        items.push(DiagItem::warn(
            cat,
            format!("topic: backend '{backend}' does not persist topic scopes"),
        ));
    } else {
        items.push(DiagItem::warn(
            cat,
            "topic: embeddings disabled; topic/semantic scoping falls back to keyword search",
        ));
    }

    items.push(DiagItem::ready(cat, "task: lifecycle/task event recording always on"));

    // 4. document readiness: document ingest needs a persistent backend.
    if persistent {
        items.push(DiagItem::ready(cat, "document: ingest backed by a persistent store"));
    } else {
        items.push(DiagItem::warn(
            cat,
            format!("document: backend '{backend}' does not persist ingested documents"),
        ));
    }

    // 5. vector readiness: vector recall needs embeddings + a sane dimension.
    if !persistent {
        items.push(DiagItem::warn(
            cat,
            format!("vector: backend '{backend}' does not persist a vector index"),
        ));
    } else if !embeddings_enabled {
        items.push(DiagItem::warn(
            cat,
            "vector: embeddings disabled; vector recall unavailable (keyword/FTS only)",
        ));
    } else if mem.embedding_dimensions == 0 {
        items.push(DiagItem::error(
            cat,
            "vector: embedding_dimensions is zero while embeddings are enabled",
        ));
    } else {
        items.push(DiagItem::ready(
            cat,
            format!("vector: index ready (dim {})", mem.embedding_dimensions),
        ));
    }

    // 6. runtime-control readiness: autonomy posture is resolvable.
    items.push(DiagItem::ready(
        cat,
        format!(
            "runtime-control: control ladder ready (autonomy {:?})",
            config.autonomy.level
        ),
    ));
}

/// FIX-P2-07: Checks Postgres backend health when a Postgres backend is
/// selected: validates the DSN, runs `SELECT 1` under a 5s timeout, verifies the
/// `vector` (pgvector) extension is installed, and performs a lightweight column
/// parity probe against `information_schema`. Skips cleanly (no error) when
/// Postgres is not configured.
async fn check_postgres_health(config: &Config, items: &mut Vec<DiagItem>) {
    let cat = "postgres";
    let storage = &config.storage.provider.config;
    let selects_postgres = storage.provider.trim().eq_ignore_ascii_case("postgres")
        || config.memory.backend.trim().eq_ignore_ascii_case("postgres");
    if !selects_postgres {
        // Not configured for Postgres -- nothing to check.
        return;
    }

    let Some(db_url) = storage.db_url.clone() else {
        items.push(DiagItem::error(
            cat,
            "postgres backend selected but [storage.provider.config] db_url is unset",
        ));
        return;
    };

    // Validate the DSN scheme before attempting a connection.
    let trimmed = db_url.trim();
    if !(trimmed.starts_with("postgres://") || trimmed.starts_with("postgresql://")) {
        items.push(DiagItem::error(
            cat,
            "db_url must start with postgres:// or postgresql://",
        ));
        return;
    }

    // The `postgres` crate is synchronous and blocking; run the probe on a
    // blocking thread and bound the whole thing with a 5s timeout.
    let probe_url = db_url.clone();
    let probe = crate::runtime::blocking::spawn_blocking(move || postgres_probe(&probe_url));
    let outcome = match tokio::time::timeout(Duration::from_secs(5), probe).await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(join_err)) => {
            items.push(DiagItem::error(cat, format!("postgres probe task failed: {join_err}")));
            return;
        }
        Err(_) => {
            items.push(DiagItem::error(cat, "postgres health probe timed out after 5s"));
            return;
        }
    };

    for item in outcome {
        items.push(item);
    }
}

/// Result of the blocking Postgres probe, mapped to diagnostic items by the
/// caller. Runs `SELECT 1`, checks pgvector, and a column-parity probe.
fn postgres_probe(db_url: &str) -> Vec<DiagItem> {
    let cat = "postgres";
    let mut out = Vec::new();

    let mut cfg = match db_url.parse::<postgres::Config>() {
        Ok(cfg) => cfg,
        Err(e) => {
            out.push(DiagItem::error(cat, format!("invalid db_url: {e}")));
            return out;
        }
    };
    cfg.connect_timeout(Duration::from_secs(4));

    let mut client = match cfg.connect(postgres::NoTls) {
        Ok(client) => client,
        Err(e) => {
            out.push(DiagItem::error(cat, format!("could not connect to postgres: {e}")));
            return out;
        }
    };

    // SELECT 1 connectivity probe.
    match client.query_one("SELECT 1", &[]) {
        Ok(_) => out.push(DiagItem::ok(cat, "SELECT 1 succeeded")),
        Err(e) => {
            out.push(DiagItem::error(cat, format!("SELECT 1 failed: {e}")));
            return out;
        }
    }

    // pgvector extension presence + version.
    match client.query_opt("SELECT extversion FROM pg_extension WHERE extname = 'vector'", &[]) {
        Ok(Some(row)) => {
            let version: String = row.try_get(0).unwrap_or_else(|_| "unknown".to_string());
            out.push(DiagItem::ok(
                cat,
                format!("pgvector extension installed (version {version})"),
            ));
        }
        Ok(None) => out.push(DiagItem::error(
            cat,
            "pgvector extension not installed (run: CREATE EXTENSION vector;)",
        )),
        Err(e) => out.push(DiagItem::warn(cat, format!("could not query pg_extension: {e}"))),
    }

    // Column-parity probe: the ACL layer relies on memories.owner_id. A missing
    // column indicates schema drift between the expected and deployed schema.
    match client.query_opt(
        "SELECT column_name FROM information_schema.columns \
         WHERE table_name = 'memories' AND column_name = 'owner_id'",
        &[],
    ) {
        Ok(Some(_)) => out.push(DiagItem::ok(cat, "schema parity: memories.owner_id present")),
        Ok(None) => out.push(DiagItem::warn(
            cat,
            "schema parity: memories.owner_id missing or table not yet migrated",
        )),
        Err(e) => out.push(DiagItem::warn(
            cat,
            format!("could not inspect information_schema: {e}"),
        )),
    }

    out
}

/// FIX-P2-08: Probes the configured custom embedding endpoint with a
/// short-timeout HEAD request (falling back to OPTIONS), so `prx doctor`
/// surfaces an unreachable embedding service before recall silently degrades.
/// Only `custom:<url>` providers expose a probe-able HTTP endpoint; other
/// providers (none/openai/built-in) are skipped.
async fn check_embedding_endpoint(config: &Config, items: &mut Vec<DiagItem>) {
    let cat = "embedding";
    let provider = config.memory.embedding_provider.trim();
    let Some(url) = provider.strip_prefix("custom:").map(str::trim) else {
        // No custom endpoint configured -- nothing to probe here. The provider
        // validity itself is reported by check_memory_diagnostics.
        return;
    };
    if url.is_empty() {
        items.push(DiagItem::error(cat, "custom embedding endpoint URL is empty"));
        return;
    }

    let client = match reqwest::Client::builder().timeout(Duration::from_secs(2)).build() {
        Ok(client) => client,
        Err(e) => {
            items.push(DiagItem::warn(cat, format!("could not build HTTP probe client: {e}")));
            return;
        }
    };

    // Try HEAD first; fall back to OPTIONS for gateways that omit HEAD. Any HTTP
    // response (including 4xx/405) proves reachability -- we check connectivity,
    // not authorization.
    let mut reachable = client.head(url).send().await.ok().map(|r| r.status());
    if reachable.is_none() {
        reachable = client
            .request(reqwest::Method::OPTIONS, url)
            .send()
            .await
            .ok()
            .map(|r| r.status());
    }

    match reachable {
        Some(status) => items.push(DiagItem::ok(
            cat,
            format!("embedding endpoint reachable (HTTP {status})"),
        )),
        None => items.push(DiagItem::warn(
            cat,
            format!("embedding endpoint unreachable within 2s: {url}"),
        )),
    }
}

fn check_memory_diagnostics(config: &Config, items: &mut Vec<DiagItem>) {
    let cat = "memory";
    items.push(DiagItem::declared(cat, "memory capability available"));

    let backend =
        crate::memory::effective_memory_backend_name(&config.memory.backend, Some(&config.storage.provider.config));
    items.push(DiagItem::configured(cat, format!("backend configured as {backend}")));
    items.push(DiagItem::configured(
        cat,
        format!("workspace path {}", config.workspace_dir.display()),
    ));

    let embedder = crate::memory::create_embedder_from_config(config);
    if embedder.name() == "none" || embedder.dimensions() == 0 {
        items.push(DiagItem::warn(
            "embedding",
            "embedding provider disabled or unresolved; keyword/FTS search remains the fallback",
        ));
    } else {
        items.push(DiagItem::configured(
            "embedding",
            format!(
                "embedding provider={} model={} dimensions={}",
                embedder.name(),
                embedder.model(),
                embedder.dimensions()
            ),
        ));
    }

    if let Some(notice) = config.embedding_default_upgrade_notice() {
        items.push(DiagItem::warn("embedding", notice));
    }

    let provider = config.memory.embedding_provider.trim();
    if provider.starts_with("custom:") {
        let url = provider.strip_prefix("custom:").unwrap_or("").trim();
        match reqwest::Url::parse(url) {
            Ok(parsed) if matches!(parsed.scheme(), "http" | "https") => {
                items.push(DiagItem::configured(
                    "embedding",
                    format!("custom embedding endpoint {url}"),
                ));
            }
            _ => items.push(DiagItem::error(
                "embedding",
                format!("custom embedding endpoint is invalid: {url}"),
            )),
        }
    }

    if config.memory.embedding_dimensions == 0 {
        items.push(DiagItem::error(
            "embedding",
            "memory.embedding_dimensions must be greater than zero when embeddings are enabled",
        ));
    }

    items.push(DiagItem::declared(
        "maintenance",
        "run `prx memory reindex` to rebuild SQLite/Lucid memory and document chunk vectors",
    ));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelProbeVerdict {
    Skipped,
    AuthOrAccess,
    Error,
}

fn classify_model_probe_error(err_message: &str) -> ModelProbeVerdict {
    let lower = err_message.to_lowercase();

    if lower.contains("does not support live model discovery") {
        return ModelProbeVerdict::Skipped;
    }

    if [
        "401",
        "403",
        "429",
        "unauthorized",
        "forbidden",
        "api key",
        "token",
        "insufficient balance",
        "insufficient quota",
        "plan does not include",
        "rate limit",
    ]
    .iter()
    .any(|hint| lower.contains(hint))
    {
        return ModelProbeVerdict::AuthOrAccess;
    }

    ModelProbeVerdict::Error
}

fn doctor_model_targets(provider_override: Option<&str>) -> Vec<String> {
    if let Some(provider) = provider_override.map(str::trim).filter(|p| !p.is_empty()) {
        return vec![provider.to_string()];
    }

    crate::providers::list_providers()
        .into_iter()
        .map(|provider| provider.name.to_string())
        .collect()
}

pub fn run_models(config: &Config, provider_override: Option<&str>, use_cache: bool) -> Result<()> {
    let targets = doctor_model_targets(provider_override);

    if targets.is_empty() {
        anyhow::bail!("No providers available for model probing");
    }

    println!("🩺 OpenPRX Doctor — Model Catalog Probe");
    println!("  Providers to probe: {}", targets.len());
    println!(
        "  Mode: {}",
        if use_cache { "cache-first" } else { "force live refresh" }
    );
    println!();

    let mut ok_count = 0usize;
    let mut skipped_count = 0usize;
    let mut auth_count = 0usize;
    let mut error_count = 0usize;

    for provider_name in &targets {
        println!(
            "  [{}]",
            crate::security::redaction::redact_provider_text(provider_name)
        );

        match crate::providers::model_discovery::run_models_probe_read_only(config, Some(provider_name), use_cache) {
            Ok(()) => {
                ok_count += 1;
                println!("    ✅ model catalog check passed");
            }
            Err(error) => {
                let error_text = crate::security::redaction::redact_provider_text(&format_error_chain(&error));
                match classify_model_probe_error(&error_text) {
                    ModelProbeVerdict::Skipped => {
                        skipped_count += 1;
                        println!("    ⚪ skipped: {}", truncate_for_display(&error_text, 160));
                    }
                    ModelProbeVerdict::AuthOrAccess => {
                        auth_count += 1;
                        println!("    ⚠️  auth/access: {}", truncate_for_display(&error_text, 160));
                    }
                    ModelProbeVerdict::Error => {
                        error_count += 1;
                        println!("    ❌ error: {}", truncate_for_display(&error_text, 160));
                    }
                }
            }
        }

        println!();
    }

    println!(
        "  Summary: {} ok, {} skipped, {} auth/access, {} errors",
        ok_count, skipped_count, auth_count, error_count
    );

    if auth_count > 0 {
        println!("  💡 Some providers need valid API keys/plan access before `/models` can be fetched.");
    }

    if provider_override.is_some() && ok_count == 0 {
        anyhow::bail!("Model probe failed for target provider")
    }

    if error_count > 0 {
        anyhow::bail!("Model probe found {error_count} provider error(s)")
    }

    Ok(())
}

// ── Config semantic validation ───────────────────────────────────

/// Resolve the configured default for chat the way every surface does:
/// refused is an error (it will not start), id-only is a warning (it runs,
/// unverified), verified is fine.
fn check_model_selection(config: &Config, items: &mut Vec<DiagItem>) {
    let cat = "models";
    if let Ok(resolved) = crate::providers::selection::check(
        config,
        crate::providers::selection::default_provider(config),
        crate::providers::selection::default_model(config),
        &crate::providers::catalog::Requirement::chat(),
        crate::providers::catalog::UnknownPolicy::Invocation,
    ) {
        if let Some(note) = crate::providers::selection::pricing_note(&resolved, config) {
            items.push(match note {
                crate::providers::selection::PricingNote::Refused(text) => DiagItem::warn(cat, text),
                crate::providers::selection::PricingNote::Untracked(text) => DiagItem::ok(cat, text),
            });
        } else if let Some(notice) = crate::providers::selection::subscription_billing_notice(
            resolved.provider_label(),
            resolved.model(),
            &config.cost,
        ) {
            items.push(DiagItem::ok(cat, notice));
        }
    }
    for warning in crate::config::schema::unmatched_cost_price_key_warnings(config) {
        items.push(DiagItem::warn("cost", warning));
    }

    match crate::config::model_settings::warnings(config) {
        Ok(warnings) => items.extend(warnings.into_iter().map(|warning| DiagItem::warn(cat, warning))),
        Err(error) => items.push(DiagItem::error(cat, error.to_string())),
    }
    match crate::config::manage::lifecycle::diagnostics(config) {
        Ok(notes) => items.extend(notes.into_iter().map(|note| {
            if note.warning {
                DiagItem::warn(cat, note.message)
            } else {
                DiagItem::with_state(Severity::Ok, DiagnosticState::Unknown, cat, note.message)
            }
        })),
        Err(error) => items.push(DiagItem::error(cat, error.to_string())),
    }
    match crate::providers::selection::default_status(config) {
        Ok((true, line)) => items.push(DiagItem::ok(cat, line)),
        Ok((false, line)) => items.push(DiagItem::warn(cat, line)),
        Err(line) => items.push(DiagItem::error(cat, line)),
    }
}

/// Every persisted selection besides the default (routes, agents,
/// fallbacks, embeddings, ...) resolved as an existing invocation (SP-PMM
/// H-4): one warning per selection the evidence does not verify, and an
/// error when one no longer resolves. The default is reported by
/// [`check_model_selection`]. Local only; no credential is read.
fn check_persisted_selections(config: &Config, items: &mut Vec<DiagItem>) {
    let cat = "models";
    // Two embedding routes sharing a hint load, but only the first is used
    // (D-104 C-1 ruling): name each with the fix.
    for note in crate::config::schema::duplicate_embedding_route_hints(config) {
        items.push(DiagItem::warn(cat, note));
    }
    match crate::config::manage::validate::check_existing_selections(config) {
        Ok(warnings) => {
            let unverified = warnings
                .into_iter()
                .filter(|warning| !warning.starts_with("default model "))
                .collect::<Vec<_>>();
            if unverified.is_empty() {
                items.push(DiagItem::healthy(cat, "every persisted selection is verified"));
            }
            for warning in unverified {
                items.push(DiagItem::warn(cat, warning));
            }
        }
        Err(refused) => items.push(DiagItem::error(
            cat,
            format!("a persisted selection no longer resolves: {refused}"),
        )),
    }
}

/// The decision plane reports refused backends and orphaned references.
fn check_decision_plane(config: &Config, items: &mut Vec<DiagItem>) {
    let cat = "decision";
    if let Some(warning) = crate::config::manage::validate::decision_startup_check(config) {
        items.push(DiagItem::warn(cat, warning));
    }
    if config.decision_engine.enabled
        && let Some(warning) = crate::config::manage::validate::decision_orphans_warning(config)
    {
        items.push(DiagItem::warn(cat, warning));
    }
}

/// Use the same registry expansion and provider filter as `RouterEngine::new`.
#[cfg(feature = "llm-router")]
fn check_router_candidates(config: &Config, items: &mut Vec<DiagItem>) {
    let reachable = crate::router::capability::reachable_provider_names(
        crate::providers::selection::default_provider(config),
        &config.model_routes,
    );
    let candidates = if config.router.models.is_empty() {
        crate::router::models::builtin_model_capabilities(&config.cost)
    } else {
        config.router.models.clone()
    };
    let has_reachable_model = crate::router::capability::filter_models_by_providers(candidates, &reachable)
        .into_iter()
        .any(|model| {
            crate::router::capability::is_model_reachable(
                crate::providers::selection::default_provider(config),
                &config.model_routes,
                &model.provider,
                &model.model_id,
            )
        });
    if !has_reachable_model {
        items.push(DiagItem::warn(
            "router",
            "Router has no reachable candidates after provider/model filtering; configure reachable [[router.models]] entries; the selected default is unchanged",
        ));
    }
}
