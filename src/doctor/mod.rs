#![allow(clippy::print_stdout, clippy::print_stderr)]

use crate::config::Config;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::collections::HashSet;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

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
        let message = msg.into();
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
    check_decision_plane(config, &mut items);
    check_workspace(config, &mut items);
    check_legacy_fitness_telemetry(config, &mut items);
    check_workspace_mcp_servers(config, &mut items);
    check_workspace_hooks(config, &mut items);
    check_unowned_shell_jobs(config, &mut items);
    check_unfinished_config_changes(config, &mut items);
    check_daemon_state(config, &mut items);
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
    match crate::hooks::workspace_hook_actions(&config.workspace_dir) {
        Ok(None | Some(0)) => items.push(DiagItem::healthy(category, "no workspace command hooks")),
        Ok(Some(actions)) if config.security.workspace_hooks == crate::config::WorkspaceHooks::On => {
            items.push(DiagItem::ok(
                category,
                format!(
                    "{actions} workspace command hook(s) run (security.workspace_hooks = \"on\"), in a cleared \
                     environment and a dedicated directory beside config.toml"
                ),
            ))
        }
        Ok(Some(actions)) => {
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

/// Shell jobs stored before PRX recorded who creates them (D-107
/// amendment): they count as agent-created, so a command that runs git is
/// refused. Each is named with the one command that vouches for it. Opens
/// the stores read-only and runs no migration: a store whose table predates
/// the `created_via` column has only such rows.
fn check_unowned_shell_jobs(config: &Config, items: &mut Vec<DiagItem>) {
    let category = "git";
    let cron_in_postgres = config
        .storage
        .provider
        .config
        .provider
        .trim()
        .eq_ignore_ascii_case("postgres");
    let mut sources = vec![(
        "xin shell task",
        "prx xin mark-operator",
        config.workspace_dir.join("xin").join("tasks.db"),
        "xin_tasks",
        "SELECT id, payload FROM xin_tasks
         WHERE execution_mode = 'shell' AND kind <> 'system' AND created_via IS NULL ORDER BY id",
        "SELECT id, payload FROM xin_tasks WHERE execution_mode = 'shell' AND kind <> 'system' ORDER BY id",
    )];
    if cron_in_postgres {
        items.push(DiagItem::ok(
            category,
            "cron jobs are stored in Postgres and not inspected here; a shell job created before PRX recorded its \
             creator runs no git until `prx cron mark-operator <id>`",
        ));
    } else {
        sources.push((
            "cron shell job",
            "prx cron mark-operator",
            config.workspace_dir.join("cron").join("jobs.db"),
            "cron_jobs",
            "SELECT id, command FROM cron_jobs WHERE job_type = 'shell' AND created_via IS NULL ORDER BY id",
            "SELECT id, command FROM cron_jobs WHERE job_type = 'shell' ORDER BY id",
        ));
    }
    let mut found = 0_usize;
    for (what, mark, db_path, table, unowned_sql, legacy_sql) in sources {
        match read_only_unowned_rows(&db_path, table, unowned_sql, legacy_sql) {
            Ok(rows) => {
                for (id, _) in rows {
                    found += 1;
                    items.push(DiagItem::warn(
                        category,
                        format!(
                            "{what} {id} predates creator tracking and counts as agent-created, so git in it is \
                             refused; if you created it: `{mark} {id}`"
                        ),
                    ));
                }
            }
            Err(error) => items.push(DiagItem::warn(
                category,
                format!("the {what}s could not be read: {error:#}"),
            )),
        }
    }
    if found == 0 {
        items.push(DiagItem::healthy(category, "every shell job records who created it"));
    }
}

/// `(id, command)` of the shell rows of `table` whose `created_via` is NULL
/// (`unowned_sql`), or all of them (`legacy_sql`) when the column does not
/// exist yet; read-only, and empty when the database or the table is
/// missing.
fn read_only_unowned_rows(
    db_path: &Path,
    table: &str,
    unowned_sql: &str,
    legacy_sql: &str,
) -> Result<Vec<(String, String)>> {
    if !db_path.is_file() {
        return Ok(Vec::new());
    }
    let connection = rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let table_exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [table],
        |row| row.get(0),
    )?;
    if !table_exists {
        return Ok(Vec::new());
    }
    let has_creator: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name = 'created_via')",
        [table],
        |row| row.get(0),
    )?;
    let mut statement = connection.prepare(if has_creator { unowned_sql } else { legacy_sql })?;
    let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
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
    let notes = crate::tools::mcp::workspace_server_notes(&config.mcp, &config.workspace_dir);
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
    #[cfg(not(feature = "channel-matrix"))]
    if config.channels_config.matrix.is_some() {
        items.push(DiagItem::warn(
            cat,
            "Matrix is configured but unavailable in this build; rebuild with `--features channel-matrix`",
        ));
    }
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
    #[cfg(feature = "channel-matrix")]
    if config.channels_config.matrix.is_some() {
        names.push("matrix");
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
enum ModelProbeOutcome {
    Skipped,
    AuthOrAccess,
    Error,
}

fn classify_model_probe_error(err_message: &str) -> ModelProbeOutcome {
    let lower = err_message.to_lowercase();

    if lower.contains("does not support live model discovery") {
        return ModelProbeOutcome::Skipped;
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
        return ModelProbeOutcome::AuthOrAccess;
    }

    ModelProbeOutcome::Error
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

        match crate::onboard::run_models_probe_read_only(config, Some(provider_name), use_cache) {
            Ok(()) => {
                ok_count += 1;
                println!("    ✅ model catalog check passed");
            }
            Err(error) => {
                let error_text =
                    crate::security::redaction::redact_url_components(&format_error_chain(&error)).into_owned();
                match classify_model_probe_error(&error_text) {
                    ModelProbeOutcome::Skipped => {
                        skipped_count += 1;
                        println!("    ⚪ skipped: {}", truncate_for_display(&error_text, 160));
                    }
                    ModelProbeOutcome::AuthOrAccess => {
                        auth_count += 1;
                        println!("    ⚠️  auth/access: {}", truncate_for_display(&error_text, 160));
                    }
                    ModelProbeOutcome::Error => {
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
    match crate::providers::selection::default_status(config) {
        Ok((true, line)) => items.push(DiagItem::ok(cat, line)),
        Ok((false, line)) => items.push(DiagItem::warn(cat, line)),
        Err(line) => items.push(DiagItem::error(cat, line)),
    }
}

/// Every persisted selection besides the default (routes, agents,
/// fallbacks, embeddings, ...) resolved as an operator's selection (SP-PMM
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
    match crate::config::manage::validate::check_affected_selections(
        None,
        config,
        crate::providers::catalog::UnknownPolicy::OperatorSelection,
    ) {
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
    if config.causal_tree.enabled
        && let Some(warning) = crate::config::manage::validate::decision_orphans_warning(config)
    {
        items.push(DiagItem::warn(cat, warning));
    }
}

fn check_config_semantics(config: &Config, items: &mut Vec<DiagItem>) {
    let cat = "config";

    if !config.autonomy.acknowledge_unrestricted_profile
        && !config.autonomy.workspace_only
        && config.autonomy.forbidden_paths.is_empty()
        && config.autonomy.max_cost_per_day_cents == u32::MAX
    {
        items.push(DiagItem::warn(
            cat,
            "explicit unrestricted autonomy profile: host-wide paths and an unbounded cost ceiling",
        ));
    }

    // Config file exists
    if config.config_path.exists() {
        items.push(DiagItem::configured(
            cat,
            format!("config file: {}", config.config_path.display()),
        ));
    } else {
        items.push(DiagItem::error(
            cat,
            format!("config file not found: {}", config.config_path.display()),
        ));
    }

    // Provider validity
    if let Some(ref provider) = config.default_provider {
        if let Some(reason) = provider_validation_error(
            config,
            provider,
            crate::providers::selection::default_model(config),
            crate::providers::selection::credential_for(config, provider),
        ) {
            items.push(DiagItem::error(
                cat,
                format!("default provider \"{provider}\" is invalid: {reason}"),
            ));
        } else {
            items.push(DiagItem::configured(cat, format!("provider \"{provider}\" is valid")));
        }
    } else {
        items.push(DiagItem::error(cat, "no default_provider configured"));
    }

    // API key / auth profile presence
    if config.default_provider.as_deref() != Some("ollama") {
        if config.api_key.is_some() {
            items.push(DiagItem::configured(cat, "API key configured"));
        } else if let Some(provider) = config.default_provider.as_deref() {
            match active_auth_profile_credential_status(config, provider) {
                AuthProfileCredentialStatus::Present { profile_id } => {
                    items.push(DiagItem::configured(
                        cat,
                        format!("auth profile credential configured for \"{provider}\" ({profile_id})"),
                    ));
                }
                AuthProfileCredentialStatus::Unreadable(error) => {
                    items.push(DiagItem::warn(
                        cat,
                        format!("auth profile check failed for \"{provider}\": {error}"),
                    ));
                }
                AuthProfileCredentialStatus::Missing => {
                    items.push(DiagItem::warn(cat, "no api_key or active auth profile credential set"));
                }
            }
        } else {
            items.push(DiagItem::warn(cat, "no api_key or default provider set"));
        }
    }

    // Model configured
    if config.default_model.is_some() {
        items.push(DiagItem::configured(
            cat,
            format!("default model: {}", config.default_model.as_deref().unwrap_or("?")),
        ));
    } else {
        items.push(DiagItem::warn(cat, "no default_model configured"));
    }

    // Temperature range
    if config.default_temperature >= 0.0 && config.default_temperature <= 2.0 {
        items.push(DiagItem::configured(
            cat,
            format!("temperature {:.1} (valid range 0.0–2.0)", config.default_temperature),
        ));
    } else {
        items.push(DiagItem::error(
            cat,
            format!(
                "temperature {:.1} is out of range (expected 0.0–2.0)",
                config.default_temperature
            ),
        ));
    }

    // Gateway port range
    let port = config.gateway.port;
    if port > 0 {
        items.push(DiagItem::configured(cat, format!("gateway port: {port}")));
    } else {
        items.push(DiagItem::error(cat, "gateway port is 0 (invalid)"));
    }

    // Reliability: fallback providers
    for fb in &config.reliability.fallback_providers {
        if let Some(reason) = provider_validation_error(
            config,
            fb,
            crate::providers::selection::default_model(config),
            crate::providers::selection::credential_for(config, fb),
        ) {
            items.push(DiagItem::warn(
                cat,
                format!("fallback provider \"{fb}\" is invalid: {reason}"),
            ));
        }
        // A self-hosted fallback answers only `model_fallbacks` entries that
        // name a model repository (D-076 N-1); without one it is never used.
        let default_is_same = config
            .default_provider
            .as_deref()
            .is_some_and(|default| default.trim().eq_ignore_ascii_case(fb.trim()));
        if crate::providers::is_self_hosted_model_server(fb)
            && !default_is_same
            && !config
                .reliability
                .model_fallbacks
                .values()
                .flatten()
                .any(|model| crate::providers::names_model_repository(model))
        {
            items.push(DiagItem::warn(
                cat,
                format!(
                    "fallback provider \"{fb}\" is never used: as a fallback it answers only \
                     reliability.model_fallbacks entries that name one of its model repositories \
                     (org/repository), and none does"
                ),
            ));
        }
    }
    let route_providers: HashSet<String> = config
        .model_routes
        .iter()
        .map(|route| route.provider.trim().to_string())
        .filter(|provider| !provider.is_empty())
        .collect();
    for (source_model, fallback_chain) in &config.reliability.model_fallbacks {
        if fallback_chain.is_empty() {
            items.push(DiagItem::warn(
                cat,
                format!(
                    "model_fallbacks entry \"{source_model}\" has an empty fallback chain; it will never take effect"
                ),
            ));
            continue;
        }

        for fallback_model in fallback_chain {
            if !model_has_reachable_provider(
                crate::providers::selection::default_provider(config),
                &config.reliability.fallback_providers,
                &route_providers,
                fallback_model,
            ) {
                items.push(DiagItem::warn(
                    cat,
                    format!(
                        "model_fallbacks route mismatch: fallback model \"{fallback_model}\" has no compatible provider in default/fallback/model_routes"
                    ),
                ));
            }
        }
    }

    let availability = crate::providers::summarize_provider_availability(
        crate::providers::selection::default_provider(config),
        config.api_key.as_deref(),
        &config.reliability,
        &crate::providers::provider_runtime_options_from_config(config),
    );
    if availability.degraded && !config.reliability.acknowledge_single_provider_risk {
        items.push(DiagItem::warn(
            cat,
            format!(
                "provider resilience degraded: only {} configured+available provider(s) (need >=2). available=[{}]",
                availability.available.len(),
                availability.available.join(", ")
            ),
        ));
    } else {
        items.push(DiagItem::ready(
            cat,
            format!(
                "provider resilience ready: {} configured+available providers [{}]",
                availability.available.len(),
                availability.available.join(", ")
            ),
        ));
    }
    if !availability.unavailable.is_empty() {
        items.push(DiagItem::warn(
            cat,
            format!(
                "unavailable providers: {}",
                availability
                    .unavailable
                    .iter()
                    .map(|(name, reason)| format!("{name}: {reason}"))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        ));
    }

    // Model routes validation
    for route in &config.model_routes {
        if route.hint.is_empty() {
            items.push(DiagItem::warn(cat, "model route with empty hint"));
        }
        let credential = route.credential_ref.as_deref().map_or_else(
            || {
                if route.api_key.as_deref().is_some_and(|key| !key.trim().is_empty()) {
                    crate::providers::credentials::CredentialRef::LegacyInline(
                        crate::providers::credentials::LegacySlot::ModelRoute(route.hint.clone()),
                    )
                } else {
                    crate::providers::selection::credential_for(config, &route.provider)
                }
            },
            |id| crate::providers::credentials::CredentialRef::Profile { id: id.to_string() },
        );
        if let Some(reason) = provider_validation_error(config, &route.provider, &route.model, credential) {
            items.push(DiagItem::warn(
                cat,
                format!(
                    "model route \"{}\" uses invalid provider \"{}\": {}",
                    route.hint, route.provider, reason
                ),
            ));
        }
        if route.model.is_empty() {
            items.push(DiagItem::warn(
                cat,
                format!("model route \"{}\" has empty model", route.hint),
            ));
        }
    }

    // Embedding routes validation
    for route in &config.embedding_routes {
        if route.hint.trim().is_empty() {
            items.push(DiagItem::warn(cat, "embedding route with empty hint"));
        }
        if let Some(reason) = embedding_provider_validation_error(&route.provider) {
            items.push(DiagItem::warn(
                cat,
                format!(
                    "embedding route \"{}\" uses invalid provider \"{}\": {}",
                    route.hint, route.provider, reason
                ),
            ));
        }
        if route.model.trim().is_empty() {
            items.push(DiagItem::warn(
                cat,
                format!("embedding route \"{}\" has empty model", route.hint),
            ));
        }
        if route.dimensions.is_some_and(|value| value == 0) {
            items.push(DiagItem::warn(
                cat,
                format!("embedding route \"{}\" has invalid dimensions=0", route.hint),
            ));
        }
    }

    if let Some(hint) = config
        .memory
        .embedding_model
        .strip_prefix("hint:")
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if !config.embedding_routes.iter().any(|route| route.hint.trim() == hint) {
            items.push(DiagItem::warn(
                cat,
                format!(
                    "memory.embedding_model uses hint \"{hint}\" but no matching [[embedding_routes]] entry exists"
                ),
            ));
        }
    }

    // Channel: at least one configured
    let cc = &config.channels_config;
    let has_channel = cc.telegram.is_some()
        || cc.discord.is_some()
        || cc.slack.is_some()
        || cc.imessage.is_some()
        || cc.matrix.is_some()
        || cc.whatsapp.is_some()
        || cc.wacli.is_some()
        || cc.nextcloud_talk.is_some()
        || cc.email.is_some()
        || cc.irc.is_some()
        || cc.lark.is_some()
        || cc.webhook.is_some();

    if has_channel {
        items.push(DiagItem::configured(cat, "at least one channel configured"));
    } else {
        items.push(DiagItem::warn(
            cat,
            "no channels configured — run `prx onboard` to set one up",
        ));
    }

    // Delegate agents: provider validity
    let mut agent_names: Vec<_> = config.agents.keys().collect();
    agent_names.sort();
    for name in agent_names {
        let Some(agent) = config.agents.get(name) else {
            continue;
        };
        let credential = agent.credential_ref.as_deref().map_or_else(
            || {
                if agent.api_key.as_deref().is_some_and(|key| !key.trim().is_empty()) {
                    crate::providers::credentials::CredentialRef::LegacyInline(
                        crate::providers::credentials::LegacySlot::Agent(name.clone()),
                    )
                } else {
                    crate::providers::selection::credential_for(config, &agent.provider)
                }
            },
            |id| crate::providers::credentials::CredentialRef::Profile { id: id.to_string() },
        );
        if let Some(reason) = provider_validation_error(config, &agent.provider, &agent.model, credential) {
            items.push(DiagItem::warn(
                cat,
                format!(
                    "agent \"{name}\" uses invalid provider \"{}\": {}",
                    agent.provider, reason
                ),
            ));
        }
        if agent.agentic && agent.allowed_tools.iter().any(|tool| tool.trim() == "*") {
            items.push(DiagItem::warn(
                cat,
                format!("agent \"{name}\" explicitly inherits all eligible parent tools"),
            ));
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AuthProfileCredentialStatus {
    Present { profile_id: String },
    Missing,
    Unreadable(String),
}

fn active_auth_profile_credential_status(config: &Config, provider: &str) -> AuthProfileCredentialStatus {
    let Some(openprx_dir) = config.config_path.parent() else {
        return AuthProfileCredentialStatus::Missing;
    };
    let canonical_provider = crate::providers::canonical_china_provider_name(provider)
        .unwrap_or(provider)
        .to_string();
    // Read-only: a diagnosis never rewrites the profiles file (D-084 E-3).
    let store = crate::auth::profiles::AuthProfilesStore::new(openprx_dir, config.secrets.encrypt);
    let data = match store.load_read_only() {
        Ok(data) => data,
        Err(error) => return AuthProfileCredentialStatus::Unreadable(error.to_string()),
    };
    let Some(profile_id) = data.active_profiles.get(&canonical_provider) else {
        return AuthProfileCredentialStatus::Missing;
    };
    let Some(profile) = data.profiles.get(profile_id) else {
        return AuthProfileCredentialStatus::Missing;
    };
    let credential = profile
        .token
        .as_deref()
        .or_else(|| profile.token_set.as_ref().map(|tokens| tokens.access_token.as_str()))
        .map(str::trim);
    if credential.is_some_and(|token| !token.is_empty()) {
        AuthProfileCredentialStatus::Present {
            profile_id: profile_id.clone(),
        }
    } else {
        AuthProfileCredentialStatus::Missing
    }
}

fn provider_validation_error(
    config: &Config,
    name: &str,
    model: &str,
    credential: crate::providers::credentials::CredentialRef,
) -> Option<String> {
    let catalog = crate::providers::selection::catalog_for(config);
    let resolved = match crate::providers::selection::check_with(
        config,
        &catalog,
        name,
        model,
        credential,
        &crate::providers::catalog::Requirement::chat(),
        crate::providers::catalog::UnknownPolicy::Invocation,
    ) {
        Ok(resolved) => resolved,
        Err(error @ crate::providers::catalog::ResolutionError::UnknownProvider { .. }) => {
            return Some(format!("Unknown provider: {error}"));
        }
        Err(error) => return Some(error.to_string()),
    };
    let mut reliability = config.reliability.clone();
    reliability.fallback_providers.clear();
    reliability.api_keys.clear();
    reliability.model_fallbacks.clear();
    match crate::providers::create_provider_for(
        &resolved,
        &catalog,
        &reliability,
        &config.cost,
        &crate::providers::provider_runtime_options_from_config(config),
    ) {
        Ok(_) => None,
        Err(error) if error.to_string().contains("missing credential/api key") => None,
        Err(error) => Some(error.to_string().lines().next().unwrap_or("invalid provider").into()),
    }
}

fn model_has_reachable_provider(
    default_provider: &str,
    fallback_providers: &[String],
    route_providers: &HashSet<String>,
    model: &str,
) -> bool {
    // As the failover chain decides (D-075): a self-hosted model server
    // serves any repository on its own route (the default provider, a model
    // route) and never answers as a fallback for another provider's route.
    let serves_own_route = |provider: &str| {
        if crate::providers::is_self_hosted_model_server(provider) {
            crate::providers::provider_serves_configured_model(provider, model)
        } else {
            crate::providers::provider_matches_model_prefix(provider, model)
        }
    };
    if serves_own_route(default_provider) {
        return true;
    }
    if fallback_providers.iter().any(|provider| {
        if crate::providers::is_self_hosted_model_server(provider) {
            // A fallback model is a `model_fallbacks` entry (D-076 N-1).
            crate::providers::names_model_repository(model)
        } else {
            crate::providers::provider_matches_model_prefix(provider, model)
        }
    }) {
        return true;
    }
    route_providers.iter().any(|provider| serves_own_route(provider))
}

fn embedding_provider_validation_error(name: &str) -> Option<String> {
    let normalized = name.trim();
    if normalized.eq_ignore_ascii_case("local")
        || normalized.eq_ignore_ascii_case("none")
        || normalized.eq_ignore_ascii_case("openai")
    {
        return None;
    }

    let Some(url) = normalized.strip_prefix("custom:") else {
        return Some("supported values: local, none, openai, custom:<url>".into());
    };

    let url = url.trim();
    if url.is_empty() {
        return Some("custom provider requires a non-empty URL after 'custom:'".into());
    }

    match reqwest::Url::parse(url) {
        Ok(parsed) if matches!(parsed.scheme(), "http" | "https") => None,
        Ok(parsed) => Some(format!(
            "custom provider URL must use http/https, got '{}'",
            parsed.scheme()
        )),
        Err(err) => Some(format!("invalid custom provider URL: {err}")),
    }
}

// ── Workspace integrity ──────────────────────────────────────────

fn check_workspace(config: &Config, items: &mut Vec<DiagItem>) {
    let cat = "workspace";
    let ws = &config.workspace_dir;

    if ws.exists() {
        items.push(DiagItem::ok(cat, format!("directory exists: {}", ws.display())));
    } else {
        items.push(DiagItem::error(cat, format!("directory missing: {}", ws.display())));
        return;
    }

    // Doctor is diagnostic-only: prove readability and report declared write
    // permission without creating a probe file.
    match std::fs::read_dir(ws) {
        Ok(_) => items.push(DiagItem::ready(cat, "directory is readable; no write probe performed")),
        Err(error) => items.push(DiagItem::error(cat, format!("directory is not readable: {error}"))),
    }
    #[cfg(unix)]
    if let Ok(metadata) = std::fs::metadata(ws) {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o222 == 0 {
            items.push(DiagItem::with_state(
                Severity::Warn,
                DiagnosticState::Configured,
                cat,
                "directory has no declared write permission bits; write capability was not exercised",
            ));
        } else {
            items.push(DiagItem::configured(
                cat,
                "directory declares write permission; capability was not exercised",
            ));
        }
    }

    // Disk space (best-effort via `df`)
    if let Some(avail_mb) = disk_available_mb(ws) {
        if avail_mb >= 100 {
            items.push(DiagItem::ok(cat, format!("disk space: {avail_mb} MB available")));
        } else {
            items.push(DiagItem::warn(
                cat,
                format!("low disk space: only {avail_mb} MB available"),
            ));
        }
    }

    // Key workspace files
    check_file_exists(ws, "SOUL.md", false, cat, items);
    check_file_exists(ws, "AGENTS.md", false, cat, items);
}

fn check_file_exists(base: &Path, name: &str, required: bool, cat: &'static str, items: &mut Vec<DiagItem>) {
    let path = base.join(name);
    if path.is_file() {
        items.push(DiagItem::ok(cat, format!("{name} present")));
    } else if required {
        items.push(DiagItem::error(cat, format!("{name} missing")));
    } else {
        items.push(DiagItem::warn(cat, format!("{name} not found (optional)")));
    }
}

fn disk_available_mb(path: &Path) -> Option<u64> {
    let output = std::process::Command::new("df").arg("-m").arg(path).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_df_available_mb(&stdout)
}

fn parse_df_available_mb(stdout: &str) -> Option<u64> {
    let line = stdout.lines().rev().find(|line| !line.trim().is_empty())?;
    let avail = line.split_whitespace().nth(3)?;
    avail.parse::<u64>().ok()
}

// ── Daemon state (original logic, preserved) ─────────────────────

fn check_daemon_state(config: &Config, items: &mut Vec<DiagItem>) {
    let cat = "daemon";
    let state_file = crate::daemon::state_file_path(config);

    if !state_file.exists() {
        items.push(DiagItem::error(
            cat,
            format!(
                "state file not found: {} — is the daemon running?",
                state_file.display()
            ),
        ));
        return;
    }

    let raw = match std::fs::read_to_string(&state_file) {
        Ok(r) => r,
        Err(e) => {
            items.push(DiagItem::error(cat, format!("cannot read state file: {e}")));
            return;
        }
    };

    let snapshot: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            items.push(DiagItem::error(cat, format!("invalid state JSON: {e}")));
            return;
        }
    };

    // Daemon heartbeat freshness
    let updated_at = snapshot
        .get("updated_at")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");

    if let Ok(ts) = DateTime::parse_from_rfc3339(updated_at) {
        let age = Utc::now().signed_duration_since(ts.with_timezone(&Utc)).num_seconds();
        if age <= DAEMON_STALE_SECONDS {
            items.push(DiagItem::ok(cat, format!("heartbeat fresh ({age}s ago)")));
        } else {
            items.push(DiagItem::error(cat, format!("heartbeat stale ({age}s ago)")));
        }
    } else {
        items.push(DiagItem::error(cat, format!("invalid daemon timestamp: {updated_at}")));
    }

    if let Some(generation) = snapshot.get("config_generation") {
        let active = generation.get("active_generation").and_then(serde_json::Value::as_u64);
        let desired_revision = generation
            .get("desired_source_revision")
            .and_then(|revision| revision.get("fingerprint_sha256"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let in_progress = generation
            .get("reload_in_progress")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        items.push(DiagItem::ok(
            "config-generation",
            format!(
                "active generation {} (desired revision {}, reload_in_progress={in_progress})",
                active.map_or_else(|| "unknown".to_string(), |value| value.to_string()),
                desired_revision
            ),
        ));

        if let Some(failure) = generation.get("last_failure").filter(|failure| !failure.is_null()) {
            let error = failure
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown reload failure");
            items.push(DiagItem::error(
                "config-generation",
                format!("last config reload failed: {error}"),
            ));
        } else if let Some(restart_required) = generation
            .get("last_report")
            .and_then(|report| report.get("restart_required"))
            .and_then(serde_json::Value::as_array)
            .filter(|fields| !fields.is_empty())
        {
            let fields = restart_required
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            items.push(DiagItem::warn(
                "config-generation",
                format!("desired config requires process restart for: {fields}"),
            ));
        }
    } else {
        items.push(DiagItem::unknown(
            "config-generation",
            "daemon state does not expose config generation status",
        ));
    }

    // Components
    if let Some(components) = snapshot.get("components").and_then(serde_json::Value::as_object) {
        // Do not hard-code runtime truth to scheduler and channel:* only. Any
        // tracked active owner that reports unhealthy must be visible in doctor.
        for (name, component) in components {
            if name == "scheduler" || name.starts_with("channel:") {
                continue;
            }
            let status = component
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            if status == "ok" || status == "disabled" {
                continue;
            }
            let detail = component
                .get("last_error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("runtime component is not healthy");
            items.push(DiagItem::error(cat, format!("{name} unhealthy ({status}): {detail}")));
        }

        // Scheduler
        if let Some(scheduler) = components.get("scheduler") {
            {
                let scheduler_ok = scheduler
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|s| s == "ok");
                let scheduler_age = scheduler
                    .get("last_ok")
                    .and_then(serde_json::Value::as_str)
                    .and_then(parse_rfc3339)
                    .map_or(i64::MAX, |dt| Utc::now().signed_duration_since(dt).num_seconds());

                if scheduler_ok && scheduler_age <= SCHEDULER_STALE_SECONDS {
                    items.push(DiagItem::ok(
                        cat,
                        format!("scheduler healthy (last ok {scheduler_age}s ago)"),
                    ));
                } else {
                    items.push(DiagItem::error(
                        cat,
                        format!("scheduler unhealthy (ok={scheduler_ok}, age={scheduler_age}s)"),
                    ));
                }
            }
        } else {
            items.push(DiagItem::unknown(cat, "scheduler component not tracked yet"));
        }

        // Channels
        let mut channel_count = 0u32;
        let mut stale = 0u32;
        for (name, component) in components {
            if !name.starts_with("channel:") {
                continue;
            }
            channel_count += 1;
            let status_ok = component
                .get("status")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|s| s == "ok");
            let age = component
                .get("last_ok")
                .and_then(serde_json::Value::as_str)
                .and_then(parse_rfc3339)
                .map_or(i64::MAX, |dt| Utc::now().signed_duration_since(dt).num_seconds());

            // Measured listener activity, when the channel publishes it. The
            // supervisor derives health from real traffic rather than from its own
            // heartbeat timer, so a wedged listener shows up here as a stall with
            // the silence duration attached. Purely informational: `prx doctor`
            // reports, it never restarts anything.
            let activity = component.get("activity");
            let stalled = activity
                .and_then(|activity| activity.get("stalled"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let idle_seconds = activity
                .and_then(|activity| activity.get("idle_seconds"))
                .and_then(serde_json::Value::as_u64);
            let liveness = activity
                .and_then(|activity| activity.get("liveness"))
                .and_then(serde_json::Value::as_str);

            if stalled {
                stale += 1;
                let idle = idle_seconds.unwrap_or_default();
                items.push(DiagItem::error(
                    cat,
                    format!("{name} listener stalled (no receive activity for {idle}s)"),
                ));
            } else if status_ok && age <= CHANNEL_STALE_SECONDS {
                let detail = match (liveness, idle_seconds) {
                    (Some("passive"), Some(idle)) => {
                        // No cadence to measure against, so "fresh" here means the
                        // supervisor is alive, not that the channel proved itself.
                        format!("{name} fresh ({age}s ago, push-only, idle {idle}s)")
                    }
                    (_, Some(idle)) => format!("{name} fresh ({age}s ago, idle {idle}s)"),
                    (_, None) => format!("{name} fresh ({age}s ago)"),
                };
                items.push(DiagItem::ok(cat, detail));
            } else {
                stale += 1;
                items.push(DiagItem::error(
                    cat,
                    format!("{name} stale (ok={status_ok}, age={age}s)"),
                ));
            }
        }

        if channel_count == 0 {
            items.push(DiagItem::unknown(cat, "no channel components tracked yet"));
        } else if stale > 0 {
            items.push(DiagItem::warn(cat, format!("{channel_count} channels, {stale} stale")));
        }
    }
}

// ── Daemon process PATH ──────────────────────────────────────────
//
// A hand-written unit without `Environment="PATH=…"` leaves the daemon with
// systemd's default PATH, which does not contain `~/.cargo/bin`. The model
// then gets `sh: prx: not found` from the shell tool while `prx doctor`, run
// from an interactive shell, sees a perfectly healthy PATH. The daemon's own
// environment is the only place that truth is visible, so read it from
// `/proc/<pid>/environ` instead of trusting the doctor process.

const DAEMON_PATH_CATEGORY: &str = "daemon";

/// Non-failing outcome: the daemon PATH could not be inspected at all.
/// Reported as `Ok`/`Unknown` so an uninspectable daemon never turns
/// `prx doctor` red.
fn daemon_path_skipped(msg: impl Into<String>) -> DiagItem {
    DiagItem::with_state(Severity::Ok, DiagnosticState::Unknown, DAEMON_PATH_CATEGORY, msg)
}

#[cfg(any(target_os = "linux", test))]
fn canonical_or_owned(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// True for a Cargo build output directory (`…/target/debug`,
/// `…/target/release`, `…/target/<triple>/debug/deps`, …).
///
/// Such a directory must never enter the required set: `cargo run -- doctor`,
/// `./target/release/prx doctor` and one-shot CI copies all run from one, and
/// the repair text would then tell the operator to put a build directory on the
/// daemon's `PATH` — an operational step that outlives the build tree.
#[cfg(any(target_os = "linux", test))]
fn is_cargo_build_dir(path: &Path) -> bool {
    let parts: Vec<&std::ffi::OsStr> = path.components().map(std::path::Component::as_os_str).collect();
    let Some(target_at) = parts.iter().rposition(|part| *part == "target") else {
        return false;
    };
    parts
        .iter()
        .skip(target_at + 1)
        .any(|part| *part == "debug" || *part == "release")
}

/// The deployed daemon binary as the daemon itself recorded it in
/// `daemon_state.json`, when that field is present.
///
/// This is the authoritative answer to "which directory must the daemon's PATH
/// contain": it is the daemon's own binary, not the doctor's.
#[cfg(any(target_os = "linux", test))]
fn daemon_binary_from_state(config: &Config) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(crate::daemon::state_file_path(config)).ok()?;
    let snapshot: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let recorded = snapshot.get("binary_path").and_then(serde_json::Value::as_str)?;
    let trimmed = recorded.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

/// Directories the daemon PATH must contain for a shell tool to resolve `prx`.
///
/// The daemon's own binary directory (from `daemon_state.json`) when it is
/// recorded, otherwise this doctor's directory as a best-effort stand-in — and
/// then only when it is not a build output directory. Plus `~/.cargo/bin` when
/// it exists, the default `cargo install` target.
#[cfg(any(target_os = "linux", test))]
fn required_daemon_path_dirs(daemon_binary: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let recorded_parent = daemon_binary.and_then(Path::parent).map(canonical_or_owned);
    if let Some(parent) = recorded_parent {
        dirs.push(parent);
    } else if let Ok(current) = std::env::current_exe() {
        if let Some(parent) = current.parent().map(canonical_or_owned) {
            if !is_cargo_build_dir(&parent) {
                dirs.push(parent);
            }
        }
    }
    if let Some(home) = crate::config::user_home_dir() {
        let cargo_bin = home.join(".cargo").join("bin");
        if cargo_bin.is_dir() {
            dirs.push(canonical_or_owned(&cargo_bin));
        }
    }
    let mut unique: Vec<PathBuf> = Vec::with_capacity(dirs.len());
    for dir in dirs {
        if !unique.contains(&dir) {
            unique.push(dir);
        }
    }
    unique
}

/// True when a process identity (its `exe` link and/or its `cmdline` payload)
/// belongs to a prx binary.
///
/// Only the *file name* is matched, never the whole path: a prx checkout lives
/// in a directory called `prx`, so a path match would call every process
/// started from that tree a daemon.
#[cfg(any(target_os = "linux", test))]
fn process_identity_looks_like_prx(exe: Option<&Path>, cmdline: &[u8]) -> bool {
    let named_prx = |path: &Path| {
        path.file_name()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|name| name.contains("prx"))
    };
    if exe.is_some_and(named_prx) {
        return true;
    }
    cmdline
        .split(|byte| *byte == 0)
        .find(|entry| !entry.is_empty())
        .and_then(|argv0| std::str::from_utf8(argv0).ok())
        .is_some_and(|argv0| named_prx(Path::new(argv0)))
}

/// Extract `PATH` from a NUL-separated `/proc/<pid>/environ` payload.
#[cfg(any(target_os = "linux", test))]
fn daemon_path_from_environ(bytes: &[u8]) -> Option<String> {
    bytes
        .split(|byte| *byte == 0)
        .filter_map(|entry| std::str::from_utf8(entry).ok())
        .find_map(|entry| entry.strip_prefix("PATH=").map(str::to_owned))
}

#[cfg(any(target_os = "linux", test))]
fn path_contains_dir(path: &str, dir: &Path) -> bool {
    std::env::split_paths(path)
        .filter(|entry| !entry.as_os_str().is_empty())
        .any(|entry| same_executable_path(&entry, dir))
}

/// Pure judgement over a daemon PATH value, so the decision is testable
/// without a live daemon.
#[cfg(any(target_os = "linux", test))]
fn evaluate_daemon_path(path: &str, required_dirs: &[PathBuf]) -> DiagItem {
    let missing: Vec<String> = required_dirs
        .iter()
        .filter(|dir| !path_contains_dir(path, dir))
        .map(|dir| dir.display().to_string())
        .collect();

    if missing.is_empty() {
        return DiagItem::ok(
            DAEMON_PATH_CATEGORY,
            format!("daemon process PATH resolves the deployed prx binary: {path}"),
        );
    }

    let missing_list = missing.join(", ");
    let suggested = if path.is_empty() {
        missing.join(":")
    } else {
        format!("{}:{path}", missing.join(":"))
    };
    DiagItem::warn(
        DAEMON_PATH_CATEGORY,
        format!(
            "daemon PATH does not include {missing_list}; shell tool cannot find prx — \
             add Environment=\"PATH={suggested}\" to the unit or re-run `prx service install` \
             (daemon PATH: {path})"
        ),
    )
}

#[cfg(any(target_os = "linux", test))]
fn evaluate_daemon_environ(bytes: &[u8], required_dirs: &[PathBuf]) -> DiagItem {
    daemon_path_from_environ(bytes).map_or_else(
        || {
            DiagItem::warn(
                DAEMON_PATH_CATEGORY,
                "daemon process environment carries no PATH entry; shell tool cannot find prx — \
                 add Environment=\"PATH=…\" to the unit or re-run `prx service install`",
            )
        },
        |path| evaluate_daemon_path(&path, required_dirs),
    )
}

fn daemon_pid_from_state(config: &Config) -> Option<u32> {
    let raw = std::fs::read_to_string(crate::daemon::state_file_path(config)).ok()?;
    let snapshot: serde_json::Value = serde_json::from_str(&raw).ok()?;
    snapshot
        .get("pid")
        .and_then(serde_json::Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
}

#[cfg(target_os = "linux")]
fn read_daemon_environ(pid: u32) -> std::io::Result<Vec<u8>> {
    std::fs::read(format!("/proc/{pid}/environ"))
}

/// Best-effort ownership check for the pid recorded in `daemon_state.json`.
///
/// A pid outlives the daemon that wrote it: after a crash the number is reused
/// by an unrelated process, and reading *that* process' environment would make
/// `prx doctor` judge the daemon PATH from a stranger's `PATH`. Unreadable
/// `/proc` entries (the process is gone, or is owned by another user) answer
/// `false`, which routes the check to its skip branch rather than a verdict.
#[cfg(target_os = "linux")]
fn pid_belongs_to_prx(pid: u32) -> bool {
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok();
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    process_identity_looks_like_prx(exe.as_deref(), &cmdline)
}

fn check_daemon_process_path(config: &Config, items: &mut Vec<DiagItem>) {
    let Some(pid) = daemon_pid_from_state(config) else {
        items.push(daemon_path_skipped(
            "daemon state exposes no pid; skipped daemon PATH inspection",
        ));
        return;
    };

    #[cfg(target_os = "linux")]
    if pid_belongs_to_prx(pid) {
        let required = required_daemon_path_dirs(daemon_binary_from_state(config).as_deref());
        match read_daemon_environ(pid) {
            Ok(bytes) => items.push(evaluate_daemon_environ(&bytes, &required)),
            Err(error) => items.push(daemon_path_skipped(format!(
                "cannot read /proc/{pid}/environ ({error}); skipped daemon PATH inspection"
            ))),
        }
    } else {
        items.push(daemon_path_skipped(format!(
            "daemon state pid {pid} is not a live prx process (pid reuse or a stopped daemon); \
             skipped reading /proc/{pid}/environ"
        )));
    }

    #[cfg(not(target_os = "linux"))]
    items.push(daemon_path_skipped(format!(
        "daemon PATH inspection is only supported on Linux; skipped for daemon pid {pid}"
    )));
}

// ── Environment checks ───────────────────────────────────────────

fn check_environment(items: &mut Vec<DiagItem>) {
    let cat = "environment";

    // git
    check_command_available("git", &["--version"], cat, items);

    // Shell
    let shell = std::env::var("SHELL").unwrap_or_default();
    if shell.is_empty() {
        items.push(DiagItem::warn(cat, "$SHELL not set"));
    } else {
        items.push(DiagItem::ok(cat, format!("shell: {shell}")));
    }

    // HOME
    if std::env::var("HOME").is_ok() || std::env::var("USERPROFILE").is_ok() {
        items.push(DiagItem::ok(cat, "home directory env set"));
    } else {
        items.push(DiagItem::error(cat, "neither $HOME nor $USERPROFILE is set"));
    }

    // Optional tools
    check_command_available("curl", &["--version"], cat, items);
}

fn check_command_available(cmd: &str, args: &[&str], cat: &'static str, items: &mut Vec<DiagItem>) {
    match std::process::Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
    {
        Ok(output) if output.status.success() => {
            let ver = String::from_utf8_lossy(&output.stdout);
            let first_line = ver.lines().next().unwrap_or("").trim();
            let display = truncate_for_display(first_line, COMMAND_VERSION_PREVIEW_CHARS);
            items.push(DiagItem::ok(cat, format!("{cmd}: {display}")));
        }
        Ok(_) => {
            items.push(DiagItem::warn(cat, format!("{cmd} found but returned non-zero")));
        }
        Err(_) => {
            items.push(DiagItem::warn(cat, format!("{cmd} not found in PATH")));
        }
    }
}

fn format_error_chain(error: &anyhow::Error) -> String {
    let mut parts = Vec::new();
    for cause in error.chain() {
        let message = cause.to_string();
        if !message.is_empty() {
            parts.push(message);
        }
    }

    if parts.is_empty() {
        return String::new();
    }

    parts.join(": ")
}

fn truncate_for_display(input: &str, max_chars: usize) -> String {
    let mut chars = input.chars();
    let preview: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{preview}…")
    } else {
        preview
    }
}

// ── Helpers ──────────────────────────────────────────────────────

fn parse_rfc3339(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw).ok().map(|dt| dt.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
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
    use tempfile::TempDir;

    /// D-094 C-1, C-5: the doctor names a refused decision backend (which
    /// runs builtin-only) and every decision credential nothing references,
    /// by id, never by value.
    #[test]
    fn the_doctor_reports_a_refused_decision_backend_and_orphaned_credentials() {
        let temp = TempDir::new().unwrap();
        let mut config = Config {
            config_path: temp.path().join("config.toml"),
            workspace_dir: temp.path().join("workspace"),
            ..Config::default()
        };
        config.causal_tree.enabled = true;
        config.causal_tree.backend.kind =
            crate::causal_tree::config::BackendSelection::Fixed(crate::decision::BackendKind::CompatibleHttp);
        config.causal_tree.backend.endpoint = "http://127.0.0.1:8900/v1".to_string();
        config.causal_tree.backend.model = "undeclared-model".to_string();
        let orphan = crate::config::manage::credentials::CredentialReferences::for_config(&config)
            .unwrap()
            .set(
                "custom:http://127.0.0.1:8900/v1",
                "decision-compatible-http-0badc0de",
                "sk-DOCTOR-ORPHAN-SENTINEL-01",
            )
            .unwrap();
        let mut items = Vec::new();
        check_decision_plane(&config, &mut items);
        let text = items
            .iter()
            .map(|item| item.message.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(items.len(), 2, "{text}");
        assert!(text.contains("builtin-only") && text.contains(&orphan.id), "{text}");
        assert!(text.contains("prx providers credential remove <id>"), "{text}");
        // Disabled, the credential store is not opened: a held lock does not
        // delay the doctor, and nothing is reported from the store.
        config.causal_tree.enabled = false;
        std::fs::write(temp.path().join("auth-profiles.lock"), "pid=1\n").unwrap();
        let started = std::time::Instant::now();
        let mut items = Vec::new();
        check_decision_plane(&config, &mut items);
        assert!(
            items.is_empty(),
            "{:?}",
            items.iter().map(|item| &item.message).collect::<Vec<_>>()
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(!text.contains("SENTINEL"), "{text}");
    }

    #[test]
    fn memory_diagnostics_flag_vector_recall_inherited_from_the_flipped_default() {
        let temp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.workspace_dir = temp.path().to_path_buf();
        config.memory.embedding_provider = "local".to_string();
        config.memory.embedding_provider_explicit = false;

        let mut items = Vec::new();
        check_memory_diagnostics(&config, &mut items);
        let notice = items
            .iter()
            .find(|item| item.message.contains("embedding_provider not set"))
            .expect("doctor must report an inherited local vector recall default");
        assert_eq!(notice.severity, Severity::Warn);
        assert_eq!(notice.category, "embedding");

        config.memory.embedding_provider_explicit = true;
        let mut items = Vec::new();
        check_memory_diagnostics(&config, &mut items);
        assert!(
            !items
                .iter()
                .any(|item| item.message.contains("embedding_provider not set")),
            "an explicit provider must not be reported as an inherited default"
        );
    }

    /// D-088 C-7: `prx doctor` names each blocked workspace MCP server with
    /// the one-line opt-in, instead of leaving it to a log line.
    #[test]
    fn doctor_reports_a_workspace_mcp_server_blocked_by_the_opt_in() {
        let temp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.workspace_dir = temp.path().to_path_buf();
        std::fs::write(
            temp.path().join("mcp.json"),
            r#"{"mcpServers": {"legacy": {"command": "npx", "args": ["some-mcp"]}}}"#,
        )
        .unwrap();
        let mut items = Vec::new();
        check_workspace_mcp_servers(&config, &mut items);
        let blocked = items
            .iter()
            .find(|item| {
                item.message
                    .contains("workspace server 'legacy' (stdio) is not started")
            })
            .expect("doctor must report the blocked workspace server");
        assert_eq!(blocked.severity, Severity::Warn);
        assert!(
            blocked.message.contains("workspace_servers = \"stdio\""),
            "{}",
            blocked.message
        );

        std::fs::remove_file(temp.path().join("mcp.json")).unwrap();
        let mut items = Vec::new();
        check_workspace_mcp_servers(&config, &mut items);
        assert!(items.iter().all(|item| item.severity != Severity::Warn));
    }

    /// D-095 N-4: an operator `working_dir` inside the workspace is named.
    #[test]
    fn doctor_warns_about_a_working_dir_inside_the_workspace() {
        let temp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.workspace_dir = temp.path().to_path_buf();
        config.mcp.servers.insert(
            "inside".into(),
            crate::config::McpServerConfig {
                command: Some("npx".into()),
                working_dir: Some(temp.path().join("tools").display().to_string()),
                ..crate::config::McpServerConfig::default()
            },
        );
        let mut items = Vec::new();
        check_workspace_mcp_servers(&config, &mut items);
        assert!(
            items
                .iter()
                .any(|item| item.severity == Severity::Warn && item.message.contains("inside the agent's workspace")),
            "{:?}",
            items.iter().map(|item| &item.message).collect::<Vec<_>>()
        );
    }

    /// RED before PMME-h (D-095 L-3): a `pending` configuration record was
    /// listed by `prx config status` but not by `prx doctor`.
    #[test]
    fn doctor_reports_an_unfinished_configuration_change() {
        let temp = TempDir::new().unwrap();
        let config = Config {
            config_path: temp.path().join("config.toml"),
            workspace_dir: temp.path().join("workspace"),
            ..Config::default()
        };
        let record_dir = temp
            .path()
            .join(".config-history")
            .join("0190a000-0000-7000-8000-000000000001");
        std::fs::create_dir_all(&record_dir).unwrap();
        let summary = crate::config::manage::ConfigChangeSummary::between(Some(&config), &config).unwrap();
        let now = chrono::Utc::now();
        let record = crate::config::manage::HistoryRecord {
            version: 1,
            id: "0190a000-0000-7000-8000-000000000001".into(),
            created_at: now,
            updated_at: now,
            actor: crate::config::manage::ConfigActor::Cli,
            status: crate::config::manage::RecordStatus::Pending,
            base_revision: None,
            committed_revision: None,
            restores: None,
            summary,
            active_generation: None,
            restart_required: Vec::new(),
            reason: None,
        };
        std::fs::write(record_dir.join("record.json"), serde_json::to_vec(&record).unwrap()).unwrap();

        let mut items = Vec::new();
        check_unfinished_config_changes(&config, &mut items);
        let pending = items
            .iter()
            .find(|item| item.message.contains("0190a000-0000-7000-8000-000000000001"))
            .unwrap_or_else(|| {
                panic!(
                    "doctor must report the pending record: {:?}",
                    items.iter().map(|item| &item.message).collect::<Vec<_>>()
                )
            });
        assert_eq!(pending.severity, Severity::Warn);
        assert!(pending.message.contains("prx config rollback"), "{}", pending.message);
    }

    /// D-107 amendment: a shell job stored before PRX recorded its creator
    /// is named with the command that vouches for it; one with a recorded
    /// creator is not; the check writes nothing.
    #[test]
    fn doctor_names_shell_jobs_without_a_recorded_creator() {
        let temp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.workspace_dir = temp.path().to_path_buf();
        let legacy = crate::cron::add_shell_job(
            &config,
            None,
            crate::cron::Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "git pull project_payload_marker",
        )
        .unwrap();
        let recorded = crate::cron::add_shell_job(
            &config,
            None,
            crate::cron::Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "echo recorded",
        )
        .unwrap();
        let db_path = temp.path().join("cron").join("jobs.db");
        rusqlite::Connection::open(&db_path)
            .unwrap()
            .execute(
                "UPDATE cron_jobs SET created_via = NULL WHERE id = ?1",
                [legacy.id.as_str()],
            )
            .unwrap();
        let before = std::fs::read(&db_path).unwrap();

        let mut items = Vec::new();
        check_unowned_shell_jobs(&config, &mut items);
        let warnings = items
            .iter()
            .filter(|item| item.category == "git" && item.severity == Severity::Warn)
            .collect::<Vec<_>>();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let warning = warnings.first().map(|item| item.message.as_str()).unwrap_or_default();
        assert!(
            warning.contains(&format!("prx cron mark-operator {}", legacy.id)),
            "{warning}"
        );
        assert!(!warning.contains(&recorded.id), "{warning}");
        assert!(!warning.contains("project_payload_marker"), "{warning}");
        assert_eq!(
            std::fs::read(&db_path).unwrap(),
            before,
            "doctor wrote to the cron store"
        );

        crate::cron::mark_job_operator(&config, &legacy.id).unwrap();
        let mut items = Vec::new();
        check_unowned_shell_jobs(&config, &mut items);
        assert!(
            !items
                .iter()
                .any(|item| item.category == "git" && item.severity == Severity::Warn),
            "{items:?}"
        );
    }

    /// SP-PMM H-4: a `pending` history record is reported with its
    /// recovery commands and never applied (loading the configuration and
    /// running the checks leave the files and the record as they were); a
    /// persisted selection the evidence does not verify is listed by name.
    #[tokio::test]
    async fn a_pending_record_is_reported_never_applied_and_unverified_selections_are_listed() {
        let temp = TempDir::new().unwrap();
        let mut config = Config {
            config_path: temp.path().join("config.toml"),
            workspace_dir: temp.path().join("workspace"),
            default_provider: Some("openai".into()),
            default_model: Some("gpt-4o".into()),
            ..Config::default()
        };
        config.model_routes.push(crate::config::ModelRouteConfig {
            hint: "fast".into(),
            provider: "openai".into(),
            model: "pmmh-unlisted-route-model".into(),
            api_key: None,
            credential_ref: None,
        });
        config.save().await.unwrap();
        let history = crate::config::manage::ConfigHistory::for_config(&config.config_path).unwrap();
        let mut other = config.clone();
        other.default_temperature = 0.1;
        let before = crate::config::files::current_before_image(&config.config_path).unwrap();
        let record = history
            .create_pending(
                crate::config::manage::history::NewRecord {
                    actor: crate::config::manage::ConfigActor::Cli,
                    base_revision: None,
                    restores: None,
                    summary: crate::config::manage::ConfigChangeSummary::between(Some(&config), &other).unwrap(),
                },
                &before,
            )
            .unwrap();
        let config_bytes = std::fs::read(&config.config_path).unwrap();
        let record_bytes = std::fs::read(history.dir().join(&record.id).join("record.json")).unwrap();

        let loaded = Config::load_from_path(&config.config_path, config.workspace_dir.clone()).unwrap();
        let mut items = Vec::new();
        check_unfinished_config_changes(&loaded, &mut items);
        check_persisted_selections(&loaded, &mut items);

        let pending = items
            .iter()
            .find(|item| item.message.contains(&record.id))
            .expect("the pending record is reported");
        assert!(
            pending.message.contains("is pending") && pending.message.contains("never applied"),
            "{}",
            pending.message
        );
        let unverified = items
            .iter()
            .find(|item| item.message.contains("model route `hint:fast`"))
            .expect("the unverified route is reported");
        assert_eq!(unverified.severity, Severity::Warn);
        assert!(
            unverified.message.contains("pmmh-unlisted-route-model"),
            "{}",
            unverified.message
        );
        assert_eq!(
            std::fs::read(&config.config_path).unwrap(),
            config_bytes,
            "a pending record was applied"
        );
        assert_eq!(
            std::fs::read(history.dir().join(&record.id).join("record.json")).unwrap(),
            record_bytes,
            "the pending record was rewritten"
        );
        assert_eq!(
            history.get(&record.id).unwrap().status,
            crate::config::manage::RecordStatus::Pending
        );
    }

    /// D-104 C-1 ruling: `prx doctor` names two embedding routes sharing a
    /// hint, with the fix.
    #[test]
    fn doctor_reports_duplicate_embedding_route_hints() {
        let route = |model: &str| crate::config::EmbeddingRouteConfig {
            hint: "semantic".into(),
            provider: "openai".into(),
            model: model.into(),
            dimensions: None,
            api_key: None,
            credential_ref: None,
        };
        let mut config = Config::default();
        config.embedding_routes = vec![route("text-embedding-3-small"), route("text-embedding-3-large")];
        let mut items = Vec::new();
        check_persisted_selections(&config, &mut items);
        let item = items
            .iter()
            .find(|item| item.message.contains("both use hint 'semantic'"))
            .expect("the duplicate is reported");
        assert_eq!(item.severity, Severity::Warn);
        assert!(
            item.message.contains("Give each route its own hint"),
            "{}",
            item.message
        );
    }

    /// HOOKS-ENV: `prx doctor` names blocked workspace hooks with the
    /// one-line opt-in, and says when they run.
    #[test]
    fn doctor_reports_blocked_workspace_hooks_with_the_opt_in() {
        let temp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.workspace_dir = temp.path().to_path_buf();
        std::fs::write(
            temp.path().join("hooks.json"),
            r#"{"hooks": {"turn_complete": [{"command": "true"}]}}"#,
        )
        .unwrap();
        let mut items = Vec::new();
        check_workspace_hooks(&config, &mut items);
        let blocked = items
            .iter()
            .find(|item| item.category == "hooks")
            .expect("test: a hooks item");
        assert_eq!(blocked.severity, Severity::Warn);
        assert!(
            blocked.message.contains(crate::hooks::WORKSPACE_HOOKS_OPT_IN),
            "{}",
            blocked.message
        );

        config.security.workspace_hooks = crate::config::WorkspaceHooks::On;
        let mut items = Vec::new();
        check_workspace_hooks(&config, &mut items);
        assert!(items.iter().all(|item| item.severity != Severity::Warn));
    }

    #[test]
    fn deployed_binary_check_accepts_the_runtime_path() {
        let temp = TempDir::new().unwrap();
        let deployed = temp.path().join("prx");
        std::fs::write(&deployed, b"binary").unwrap();
        let mut items = Vec::new();

        check_deployed_binary_paths(&mut items, Some(deployed.clone()), Ok(deployed));

        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|item| item.severity == Severity::Ok));
        assert!(
            items
                .iter()
                .any(|item| item.message == "doctor is running from deployed binary")
        );
    }

    #[test]
    fn deployed_binary_check_reports_missing_path_lookup_without_linux_hardcode() {
        let mut items = Vec::new();

        check_deployed_binary_paths(&mut items, None, Ok(PathBuf::from("/opt/homebrew/bin/prx")));

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].severity, Severity::Error);
        assert_eq!(items[0].message, "deployed prx binary not found on PATH");
        assert!(items.iter().all(|item| !item.message.contains("/home/ck")));
    }

    struct EnvGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvGuard {
        #[allow(unsafe_code)]
        fn set(key: &'static str, value: Option<&str>) -> Self {
            let original = std::env::var(key).ok();
            if key == "HOME" {
                crate::config::schema::follow_test_home_env(value.map(std::ffi::OsStr::new));
            }
            // SAFETY: test-only env manipulation; tests that use this must be serialized.
            unsafe {
                match value {
                    Some(next) => std::env::set_var(key, next),
                    None => std::env::remove_var(key),
                }
            }
            Self { key, original }
        }
    }

    impl Drop for EnvGuard {
        #[allow(unsafe_code)]
        fn drop(&mut self) {
            if self.key == "HOME" {
                crate::config::schema::follow_test_home_env(self.original.as_deref().map(std::ffi::OsStr::new));
            }
            // SAFETY: test-only env manipulation; restoring previous value.
            unsafe {
                if let Some(original) = self.original.as_deref() {
                    std::env::set_var(self.key, original);
                } else {
                    std::env::remove_var(self.key);
                }
            }
        }
    }

    fn doctor_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .expect("doctor env lock poisoned")
    }

    #[test]
    fn provider_validation_checks_custom_url_shape() {
        let config = Config::default();
        let validate = |provider: &str| {
            provider_validation_error(
                &config,
                provider,
                crate::providers::selection::default_model(&config),
                crate::providers::selection::credential_for(&config, provider),
            )
        };
        assert!(validate("openrouter").is_none());
        assert!(validate("custom:https://example.com").is_none());
        assert!(validate("anthropic-custom:https://example.com").is_none());

        let invalid_custom = validate("custom:").unwrap_or_default();
        assert!(invalid_custom.contains("requires a URL"));

        let invalid_unknown = validate("totally-fake").unwrap_or_default();
        assert!(invalid_unknown.contains("Unknown provider"));
    }

    #[test]
    fn diag_item_icons() {
        assert_eq!(DiagItem::ok("t", "m").icon(), "✅");
        assert_eq!(DiagItem::warn("t", "m").icon(), "⚠️ ");
        assert_eq!(DiagItem::error("t", "m").icon(), "❌");
    }

    #[tokio::test]
    async fn diagnostic_states_are_emitted_by_real_checks() {
        let temp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.config_path = temp.path().join("config.toml");
        config.workspace_dir = temp.path().to_path_buf();
        std::fs::write(&config.config_path, "configured").unwrap();

        let mut items = Vec::new();
        check_memory_diagnostics(&config, &mut items);
        check_runtime_readiness(&config, &mut items);
        check_environment(&mut items);
        check_daemon_state(&config, &mut items);
        check_runtime_memory_health(&config, &mut items);

        let states: std::collections::HashSet<_> = items.iter().map(|item| item.state).collect();
        for expected in [
            DiagnosticState::Declared,
            DiagnosticState::Configured,
            DiagnosticState::Ready,
            DiagnosticState::Healthy,
            DiagnosticState::Unknown,
        ] {
            assert!(states.contains(&expected), "missing diagnostic state {expected:?}");
        }
    }

    #[test]
    fn doctor_reports_legacy_fitness_counts_and_repair_command() {
        let temp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.workspace_dir = temp.path().join("workspace");
        std::fs::create_dir_all(config.workspace_dir.join("memory")).unwrap();
        let connection = rusqlite::Connection::open(config.workspace_dir.join("memory/brain.db")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE memories (key TEXT NOT NULL, content TEXT NOT NULL);
                 INSERT INTO memories VALUES ('self/fitness/daily/2026-09-10', 'one');
                 INSERT INTO memories VALUES ('self/fitness/daily/2026-09-11', 'two');
                 INSERT INTO memories VALUES ('keep/key', 'keep');",
            )
            .unwrap();
        std::fs::write(
            config.workspace_dir.join("MEMORY.md"),
            "- [t] **self/fitness/daily/2026-09-10**: legacy\n- [t] **keep/key**: keep\n",
        )
        .unwrap();
        std::fs::write(
            config.workspace_dir.join("MEMORY_SNAPSHOT.md"),
            "### 🔑 `self/fitness/daily/2026-09-10`\n\nlegacy\n\n---\n",
        )
        .unwrap();
        let mut items = Vec::new();

        check_legacy_fitness_telemetry(&config, &mut items);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].severity, Severity::Warn);
        assert!(items[0].message.contains("database=2"));
        assert!(items[0].message.contains("MEMORY.md=1"));
        assert!(items[0].message.contains("MEMORY_SNAPSHOT.md=1"));
        assert!(items[0].message.contains("prx fitness migrate-legacy --apply"));
    }

    #[test]
    fn doctor_clean_fitness_probe_is_read_only_on_missing_database() {
        let temp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.workspace_dir = temp.path().join("workspace");
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        let db_path = config.workspace_dir.join("memory/brain.db");
        let mut items = Vec::new();

        check_legacy_fitness_telemetry(&config, &mut items);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].severity, Severity::Ok);
        assert!(items[0].message.contains("absent"));
        assert!(!db_path.exists());
    }

    #[test]
    fn classify_model_probe_error_marks_unsupported_as_skipped() {
        let outcome = classify_model_probe_error("Provider 'copilot' does not support live model discovery yet");
        assert_eq!(outcome, ModelProbeOutcome::Skipped);
    }

    #[test]
    fn classify_model_probe_error_marks_auth_and_plan_issues() {
        let auth_outcome = classify_model_probe_error("OpenAI API error (401): unauthorized");
        assert_eq!(auth_outcome, ModelProbeOutcome::AuthOrAccess);

        let plan_outcome = classify_model_probe_error("Z.AI API error (429): plan does not include requested model");
        assert_eq!(plan_outcome, ModelProbeOutcome::AuthOrAccess);
    }

    #[test]
    fn config_validation_catches_bad_temperature() {
        let mut config = Config::default();
        config.default_temperature = 5.0;
        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);
        let temp_item = items.iter().find(|i| i.message.contains("temperature"));
        assert!(temp_item.is_some());
        assert_eq!(temp_item.unwrap().severity, Severity::Error);
    }

    #[test]
    fn config_validation_accepts_valid_temperature() {
        let mut config = Config::default();
        config.default_temperature = 0.7;
        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);
        let temp_item = items.iter().find(|i| i.message.contains("temperature"));
        assert!(temp_item.is_some());
        assert_eq!(temp_item.unwrap().severity, Severity::Ok);
    }

    #[test]
    fn config_validation_warns_for_explicit_unrestricted_autonomy() {
        let mut config = Config::default();
        config.autonomy.workspace_only = false;
        config.autonomy.forbidden_paths.clear();
        config.autonomy.max_cost_per_day_cents = u32::MAX;
        let mut items = Vec::new();

        check_config_semantics(&config, &mut items);

        let item = items
            .iter()
            .find(|item| item.message.contains("explicit unrestricted autonomy profile"))
            .expect("unrestricted posture warning");
        assert_eq!(item.severity, Severity::Warn);
    }

    #[test]
    fn config_validation_acknowledged_unrestricted_autonomy_is_quiet() {
        let mut config = Config::default();
        config.autonomy.workspace_only = false;
        config.autonomy.forbidden_paths.clear();
        config.autonomy.max_cost_per_day_cents = u32::MAX;
        config.autonomy.acknowledge_unrestricted_profile = true;
        let mut items = Vec::new();

        check_config_semantics(&config, &mut items);

        assert!(
            !items
                .iter()
                .any(|item| item.message.contains("explicit unrestricted autonomy profile"))
        );
    }

    #[test]
    fn config_validation_warns_for_explicit_delegate_wildcard() {
        let mut config = Config::default();
        config.agents.insert(
            "worker".into(),
            crate::config::DelegateAgentConfig {
                provider: "openrouter".into(),
                model: "model-test".into(),
                system_prompt: None,
                api_key: None,
                credential_ref: None,
                temperature: None,
                max_depth: 3,
                agentic: true,
                allowed_tools: vec!["*".into()],
                identity_dir: None,
                memory_scope: None,
                spawn_enabled: None,
            },
        );
        let mut items = Vec::new();

        check_config_semantics(&config, &mut items);

        let item = items
            .iter()
            .find(|item| item.message.contains("explicitly inherits all eligible parent tools"))
            .expect("delegate wildcard warning");
        assert_eq!(item.severity, Severity::Warn);
    }

    #[test]
    fn config_validation_warns_no_channels() {
        let config = Config::default();
        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);
        let ch_item = items.iter().find(|i| i.message.contains("channel"));
        assert!(ch_item.is_some());
        assert_eq!(ch_item.unwrap().severity, Severity::Warn);
    }

    #[test]
    fn wacli_is_reported_as_a_configured_channel() {
        let mut config = Config::default();
        config.channels_config.cli = false;
        config.channels_config.wacli = Some(crate::config::WacliConfig::default());

        assert_eq!(configured_channel_names(&config), vec!["wacli"]);
        let mut items = Vec::new();
        check_channel_runtime(&config, &mut items);
        assert!(
            items
                .iter()
                .any(|item| item.message.contains("configured channels: wacli"))
        );
    }

    #[cfg(not(feature = "channel-matrix"))]
    #[test]
    fn configured_matrix_is_reported_as_unavailable_without_the_feature() {
        let mut config = Config::default();
        config.channels_config.matrix = Some(crate::config::MatrixConfig {
            homeserver: "https://matrix.example.com".into(),
            access_token: "fixture-token".into(),
            user_id: Some("@prx_bot:matrix.example.com".into()),
            device_id: Some("PRXDEVICE".into()),
            room_id: "!prx_channel:matrix.example.com".into(),
            allowed_users: vec!["@prx_user:matrix.example.com".into()],
            mention_only: false,
        });

        let mut items = Vec::new();
        check_channel_runtime(&config, &mut items);

        let unavailable = items
            .iter()
            .find(|item| item.message.contains("Matrix is configured but unavailable"))
            .expect("default build must report configured Matrix as unavailable");
        assert_eq!(unavailable.severity, Severity::Warn);
        assert!(unavailable.message.contains("--features channel-matrix"));
        assert!(!configured_channel_names(&config).contains(&"matrix"));
    }

    #[test]
    fn daemon_state_reports_stale_scheduler_as_unhealthy() {
        let temp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.config_path = temp.path().join("config.toml");

        let stale = (Utc::now() - chrono::Duration::seconds(SCHEDULER_STALE_SECONDS + 60)).to_rfc3339();
        let state = serde_json::json!({
            "pid": std::process::id(),
            "updated_at": Utc::now().to_rfc3339(),
            "components": {
                "scheduler": {
                    "status": "ok",
                    "updated_at": stale,
                    "last_ok": stale,
                    "last_error": null,
                    "restart_count": 0
                }
            }
        });
        std::fs::write(
            crate::daemon::state_file_path(&config),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();

        let mut items = Vec::new();
        check_daemon_state(&config, &mut items);

        let scheduler_item = items
            .iter()
            .find(|item| item.message.contains("scheduler unhealthy"))
            .expect("stale always-on scheduler should be unhealthy");
        assert_eq!(scheduler_item.severity, Severity::Error);
    }

    #[test]
    fn config_validation_catches_unknown_provider() {
        let mut config = Config::default();
        config.default_provider = Some("totally-fake".into());
        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);
        let prov_item = items.iter().find(|i| i.message.contains("default provider"));
        assert!(prov_item.is_some());
        assert_eq!(prov_item.unwrap().severity, Severity::Error);
    }

    #[test]
    fn config_validation_catches_malformed_custom_provider() {
        let mut config = Config::default();
        config.default_provider = Some("custom:".into());
        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);

        let prov_item = items
            .iter()
            .find(|item| item.message.contains("default provider \"custom:\" is invalid"));
        assert!(prov_item.is_some());
        assert_eq!(prov_item.unwrap().severity, Severity::Error);
    }

    #[test]
    fn config_validation_accepts_custom_provider() {
        let mut config = Config::default();
        config.default_provider = Some("custom:https://my-api.com".into());
        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);
        let prov_item = items.iter().find(|i| i.message.contains("is valid"));
        assert!(prov_item.is_some());
        assert_eq!(prov_item.unwrap().severity, Severity::Ok);
    }

    #[test]
    fn config_validation_accepts_active_auth_profile_credential() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("config.toml"), "").unwrap();
        let store = crate::auth::profiles::AuthProfilesStore::new(tmp.path(), false);
        store
            .upsert_profile(
                crate::auth::profiles::AuthProfile::new_token("moonshot", "default", "moonshot-test-key".to_string()),
                true,
            )
            .unwrap();

        let mut config = Config::default();
        config.config_path = tmp.path().join("config.toml");
        config.default_provider = Some("moonshot".into());
        config.api_key = None;
        config.secrets.encrypt = false;

        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);

        let auth_item = items
            .iter()
            .find(|item| item.message.contains("auth profile credential configured"));
        assert!(
            auth_item.is_some(),
            "items: {:?}",
            items.iter().map(|i| &i.message).collect::<Vec<_>>()
        );
        assert_eq!(auth_item.unwrap().severity, Severity::Ok);
        assert!(
            !items.iter().any(|item| item.message.contains("no api_key")),
            "active auth profile should suppress missing api_key warning"
        );
    }

    #[test]
    fn config_validation_maps_kimi_alias_to_moonshot_auth_profile() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("config.toml"), "").unwrap();
        let store = crate::auth::profiles::AuthProfilesStore::new(tmp.path(), false);
        store
            .upsert_profile(
                crate::auth::profiles::AuthProfile::new_token("moonshot", "default", "moonshot-test-key".to_string()),
                true,
            )
            .unwrap();

        let mut config = Config::default();
        config.config_path = tmp.path().join("config.toml");
        config.default_provider = Some("kimi".into());
        config.api_key = None;
        config.secrets.encrypt = false;

        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);

        let auth_item = items
            .iter()
            .find(|item| item.message.contains("auth profile credential configured"));
        assert!(
            auth_item.is_some(),
            "items: {:?}",
            items.iter().map(|i| &i.message).collect::<Vec<_>>()
        );
        assert_eq!(auth_item.unwrap().severity, Severity::Ok);
    }

    #[test]
    fn config_validation_warns_bad_fallback() {
        let mut config = Config::default();
        config.reliability.fallback_providers = vec!["fake-provider".into()];
        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);
        let fb_item = items.iter().find(|i| i.message.contains("fallback provider"));
        assert!(fb_item.is_some());
        assert_eq!(fb_item.unwrap().severity, Severity::Warn);
    }

    #[test]
    fn config_validation_warns_bad_custom_fallback() {
        let mut config = Config::default();
        config.reliability.fallback_providers = vec!["custom:".into()];
        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);

        let fb_item = items
            .iter()
            .find(|item| item.message.contains("fallback provider \"custom:\" is invalid"));
        assert!(fb_item.is_some());
        assert_eq!(fb_item.unwrap().severity, Severity::Warn);
    }

    #[test]
    fn config_validation_warns_provider_resilience_degraded_with_single_available_provider() {
        let _guard = doctor_env_lock();

        // Isolate from host credentials: override HOME to an empty temp dir so
        // resolve_claude_code_context cannot find ~/.claude/.credentials.json,
        // and clear any ambient Anthropic env vars.
        let iso_home = std::env::temp_dir().join(format!("openprx-doctor-degrade-test-{}", std::process::id()));
        std::fs::create_dir_all(&iso_home).unwrap();
        let _home_guard = EnvGuard::set("HOME", Some(iso_home.to_str().unwrap()));
        let _anthropic_key_guard = EnvGuard::set("ANTHROPIC_API_KEY", None);
        let _anthropic_oauth_guard = EnvGuard::set("ANTHROPIC_OAUTH_TOKEN", None);

        let mut config = Config::default();
        config.default_provider = Some("openai".into());
        config.api_key = Some("sk-test".into());
        config.reliability.fallback_providers = vec!["anthropic".into()];

        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);

        let _ = std::fs::remove_dir_all(&iso_home);
        let degraded = items.iter().find(|item| {
            item.message
                .contains("provider resilience degraded: only 1 configured+available")
        });
        assert!(degraded.is_some());
        assert_eq!(degraded.unwrap().severity, Severity::Warn);
    }

    #[test]
    fn config_validation_acknowledged_single_provider_risk_is_quiet() {
        let _guard = doctor_env_lock();
        let iso_home = std::env::temp_dir().join(format!("openprx-doctor-ack-test-{}", std::process::id()));
        std::fs::create_dir_all(&iso_home).unwrap();
        let _home_guard = EnvGuard::set("HOME", Some(iso_home.to_str().unwrap()));
        let _anthropic_key_guard = EnvGuard::set("ANTHROPIC_API_KEY", None);
        let _anthropic_oauth_guard = EnvGuard::set("ANTHROPIC_OAUTH_TOKEN", None);

        let mut config = Config::default();
        config.default_provider = Some("openai".into());
        config.api_key = Some("sk-test".into());
        config.reliability.fallback_providers = vec!["anthropic".into()];
        config.reliability.acknowledge_single_provider_risk = true;

        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);

        assert!(
            !items
                .iter()
                .any(|item| item.message.contains("provider resilience degraded"))
        );
        let _ = std::fs::remove_dir_all(&iso_home);
    }

    #[test]
    fn config_validation_warns_model_fallback_route_mismatch() {
        let mut config = Config::default();
        config.default_provider = Some("openai".into());
        config.reliability.model_fallbacks.insert(
            "openai/gpt-4o".into(),
            vec!["anthropic/claude-sonnet-4-20250514".into()],
        );

        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);

        let mismatch = items
            .iter()
            .find(|item| item.message.contains("model_fallbacks route mismatch"));
        assert!(mismatch.is_some());
        assert_eq!(mismatch.unwrap().severity, Severity::Warn);
    }

    /// D-072 L-9 / D-075: doctor mirrors the failover chain. A llama.cpp
    /// fallback never answers another provider's route, so a fallback model
    /// only it could serve is a route mismatch; on its own route it serves
    /// any repository, so no warning there.
    #[test]
    fn config_validation_follows_llamacpp_route_identity() {
        let temp = tempfile::tempdir().expect("test: temp dir");
        let base = Config {
            config_path: temp.path().join("config.toml"),
            workspace_dir: temp.path().join("workspace"),
            ..Config::default()
        };
        let warning = |config: &Config, needle: &str| {
            let mut items = Vec::new();
            check_config_semantics(config, &mut items);
            items.into_iter().find(|item| item.message.contains(needle))
        };

        // D-076 N-1: a llama.cpp fallback with nothing it may serve is never used.
        let mut unused = base.clone();
        unused.default_provider = Some("anthropic".into());
        unused.reliability.fallback_providers = vec!["llamacpp".into()];
        assert!(
            warning(&unused, "\"llamacpp\" is never used").is_some_and(|item| item.severity == Severity::Warn),
            "a llama.cpp fallback without a repository entry is dead config"
        );
        unused
            .reliability
            .model_fallbacks
            .insert("anthropic/claude-sonnet-5".into(), vec!["claude-haiku-5".into()]);
        assert!(
            warning(&unused, "\"llamacpp\" is never used").is_some(),
            "a bare id is routed to another provider"
        );

        // Named in model_fallbacks: the fallback serves it; no warning.
        let mut named = base.clone();
        named.default_provider = Some("anthropic".into());
        named.reliability.fallback_providers = vec!["llamacpp".into()];
        named.reliability.model_fallbacks.insert(
            "anthropic/claude-sonnet-5".into(),
            vec!["ggml-org/gpt-oss-20b-GGUF".into()],
        );
        assert!(warning(&named, "is never used").is_none());
        assert!(warning(&named, "model_fallbacks route mismatch").is_none());

        // A `<provider>/…` fallback model only the llama.cpp fallback could
        // take is routed to another provider: a mismatch.
        let mut elsewhere = named.clone();
        elsewhere.reliability.model_fallbacks.insert(
            "anthropic/claude-sonnet-5".into(),
            vec!["openai/gpt-5".into(), "ggml-org/gpt-oss-20b-GGUF".into()],
        );
        assert!(
            warning(&elsewhere, "model_fallbacks route mismatch").is_some_and(|item| item.severity == Severity::Warn)
        );

        // On its own route llama.cpp serves any repository.
        let mut own_route = base;
        own_route.default_provider = Some("llamacpp".into());
        own_route.default_model = Some("ggml-org/gpt-oss-20b-GGUF".into());
        own_route.reliability.model_fallbacks.insert(
            "ggml-org/gpt-oss-20b-GGUF".into(),
            vec!["google/gemma-3-27b-it-GGUF".into()],
        );
        assert!(warning(&own_route, "model_fallbacks route mismatch").is_none());
    }

    /// PMM-C: doctor resolves the configured default: refused is an error,
    /// id-only is a warning (D-018 Q1: it keeps running, unverified),
    /// declared is fine.
    #[test]
    fn doctor_reports_how_the_default_model_resolves() {
        let temp = tempfile::tempdir().expect("test: temp dir");
        let mut config = Config::default();
        config.workspace_dir = temp.path().to_path_buf();
        config.config_path = temp.path().join("config.toml");
        config.default_provider = Some("openrouter".to_string());
        config.api_key = Some("PMMC-DOCTOR-ROOT".to_string());
        let models_item = |config: &Config| {
            diagnose(config)
                .items
                .into_iter()
                .find(|item| item.category == "models" && item.message.contains("default model"))
        };

        config.default_model = Some("jev-decision-test".to_string());
        config.providers.models.push(
            toml::from_str(
                "provider = \"openrouter\"\nmodel = \"jev-decision-test\"\noperations = [\"decision_endpoint\"]\n",
            )
            .expect("test: declaration"),
        );
        let item = models_item(&config).expect("a refused default is reported");
        assert_eq!(item.severity, Severity::Error, "{}", item.message);
        assert!(item.message.contains("cannot be used for chat"), "{}", item.message);
        assert!(!item.message.contains("PMMC-DOCTOR-ROOT"));

        config.default_model = Some("vendor/id-only".to_string());
        let item = models_item(&config).expect("an unverified default is reported");
        assert_eq!(item.severity, Severity::Warn, "{}", item.message);
        assert!(item.message.contains("unverified"), "{}", item.message);

        config.providers.models.push(
            toml::from_str("provider = \"openrouter\"\nmodel = \"vendor/id-only\"\noperations = [\"chat\"]\n")
                .expect("test: declaration"),
        );
        let item = models_item(&config).expect("a verified default is reported");
        assert_eq!(item.severity, Severity::Ok, "{}", item.message);
    }

    #[test]
    fn config_validation_warns_empty_model_route() {
        let mut config = Config::default();
        config.model_routes = vec![crate::config::ModelRouteConfig {
            hint: "fast".into(),
            provider: "groq".into(),
            model: String::new(),
            api_key: None,
            credential_ref: None,
        }];
        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);
        let route_item = items.iter().find(|i| i.message.contains("empty model"));
        assert!(route_item.is_some());
        assert_eq!(route_item.unwrap().severity, Severity::Warn);
    }

    #[test]
    fn config_validation_warns_empty_embedding_route_model() {
        let mut config = Config::default();
        config.embedding_routes = vec![crate::config::EmbeddingRouteConfig {
            hint: "semantic".into(),
            provider: "openai".into(),
            model: String::new(),
            dimensions: Some(1536),
            api_key: None,
            credential_ref: None,
        }];

        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);
        let route_item = items
            .iter()
            .find(|item| item.message.contains("embedding route \"semantic\" has empty model"));
        assert!(route_item.is_some());
        assert_eq!(route_item.unwrap().severity, Severity::Warn);
    }

    #[test]
    fn config_validation_warns_invalid_embedding_route_provider() {
        let mut config = Config::default();
        config.embedding_routes = vec![crate::config::EmbeddingRouteConfig {
            hint: "semantic".into(),
            provider: "groq".into(),
            model: "text-embedding-3-small".into(),
            dimensions: None,
            api_key: None,
            credential_ref: None,
        }];

        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);
        let route_item = items
            .iter()
            .find(|item| item.message.contains("uses invalid provider \"groq\""));
        assert!(route_item.is_some());
        assert_eq!(route_item.unwrap().severity, Severity::Warn);
    }

    #[test]
    fn config_validation_warns_missing_embedding_hint_target() {
        let mut config = Config::default();
        config.memory.embedding_model = "hint:semantic".into();

        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);
        let route_item = items
            .iter()
            .find(|item| item.message.contains("no matching [[embedding_routes]] entry exists"));
        assert!(route_item.is_some());
        assert_eq!(route_item.unwrap().severity, Severity::Warn);
    }

    #[test]
    fn environment_check_finds_git() {
        let mut items = Vec::new();
        check_environment(&mut items);
        let git_item = items.iter().find(|i| i.message.starts_with("git:"));
        // git should be available in any CI/dev environment
        assert!(git_item.is_some());
        assert_eq!(git_item.unwrap().severity, Severity::Ok);
    }

    #[test]
    fn parse_df_available_mb_uses_last_data_line() {
        let stdout = "Filesystem 1M-blocks Used Available Use% Mounted on\n/dev/sda1 1000 500 500 50% /\n";
        assert_eq!(parse_df_available_mb(stdout), Some(500));
    }

    #[test]
    fn truncate_for_display_preserves_utf8_boundaries() {
        let preview = truncate_for_display("🙂example-alpha-build", 3);
        assert_eq!(preview, "🙂ex…");
    }

    #[test]
    fn workspace_check_does_not_create_probe_files() {
        let tmp = TempDir::new().unwrap();
        let marker = tmp.path().join("marker.txt");
        std::fs::write(&marker, "existing").unwrap();
        let before: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        let mut config = Config::default();
        config.workspace_dir = tmp.path().to_path_buf();
        let mut items = Vec::new();

        check_workspace(&config, &mut items);

        let after: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(before, after);
        assert!(
            items
                .iter()
                .any(|item| item.message.contains("no write probe performed"))
        );
    }

    #[test]
    fn doctor_run_returns_error_when_report_contains_errors() {
        let temp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.config_path = temp.path().join("missing-config.toml");
        config.workspace_dir = temp.path().join("missing-workspace");

        assert!(
            run(&config).is_err(),
            "doctor must return nonzero semantics for ERROR findings"
        );
    }

    #[tokio::test]
    async fn runtime_memory_probe_does_not_create_missing_sqlite_database() {
        let temp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.workspace_dir = temp.path().join("workspace");
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        let db_path = config.workspace_dir.join("memory").join("brain.db");

        let mut items = Vec::new();
        check_runtime_memory_health(&config, &mut items);

        assert!(!db_path.exists(), "doctor memory probe must not create brain.db");
    }

    #[test]
    fn config_validation_reports_delegate_agents_in_sorted_order() {
        let mut config = Config::default();
        config.agents.insert(
            "zeta".into(),
            crate::config::DelegateAgentConfig {
                provider: "totally-fake".into(),
                model: "model-z".into(),
                system_prompt: None,
                api_key: None,
                credential_ref: None,
                temperature: None,
                max_depth: 3,
                agentic: false,
                allowed_tools: Vec::new(),
                identity_dir: None,
                memory_scope: None,
                spawn_enabled: None,
            },
        );
        config.agents.insert(
            "alpha".into(),
            crate::config::DelegateAgentConfig {
                provider: "totally-fake".into(),
                model: "model-a".into(),
                system_prompt: None,
                api_key: None,
                credential_ref: None,
                temperature: None,
                max_depth: 3,
                agentic: false,
                allowed_tools: Vec::new(),
                identity_dir: None,
                memory_scope: None,
                spawn_enabled: None,
            },
        );

        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);

        let agent_messages: Vec<_> = items
            .iter()
            .filter(|item| item.message.starts_with("agent \""))
            .map(|item| item.message.as_str())
            .collect();

        assert_eq!(agent_messages.len(), 2);
        assert!(agent_messages[0].contains("agent \"alpha\""));
        assert!(agent_messages[1].contains("agent \"zeta\""));
    }

    // ---- FIX-P2-06: runtime readiness sub-checks ------------------------

    #[test]
    fn runtime_readiness_emits_six_subchecks() {
        let config = Config::default();
        let mut items = Vec::new();
        check_runtime_readiness(&config, &mut items);
        let readiness: Vec<&str> = items
            .iter()
            .filter(|i| i.category == "readiness")
            .map(|i| i.message.as_str())
            .collect();
        assert_eq!(readiness.len(), 6, "expected 6 readiness sub-checks");
        for prefix in ["owner:", "topic:", "task:", "document:", "vector:", "runtime-control:"] {
            assert!(
                readiness.iter().any(|m| m.starts_with(prefix)),
                "missing readiness sub-check with prefix {prefix}"
            );
        }
        assert!(items.iter().any(|item| {
            item.state == DiagnosticState::Ready && item.message.starts_with("topic: local similarity")
        }));
        assert!(
            items.iter().any(|item| {
                item.state == DiagnosticState::Ready && item.message.starts_with("vector: index ready")
            })
        );
    }

    #[test]
    fn runtime_readiness_flags_zero_dimension_with_embeddings() {
        let mut config = Config::default();
        config.memory.embedding_provider = "openai".into();
        config.memory.embedding_dimensions = 0;
        let mut items = Vec::new();
        check_runtime_readiness(&config, &mut items);
        assert!(
            items.iter().any(|i| i.category == "readiness"
                && i.severity == Severity::Error
                && i.message.starts_with("vector:")),
            "expected vector readiness error for zero dimension"
        );
    }

    // ---- FIX-P2-07: postgres health -------------------------------------

    #[tokio::test]
    async fn postgres_health_skips_when_not_selected() {
        // Default backend is sqlite -> no postgres items emitted at all.
        let config = Config::default();
        let mut items = Vec::new();
        check_postgres_health(&config, &mut items).await;
        assert!(items.iter().all(|i| i.category != "postgres"));
    }

    #[tokio::test]
    async fn postgres_health_errors_on_missing_db_url() {
        let mut config = Config::default();
        config.memory.backend = "postgres".into();
        let mut items = Vec::new();
        check_postgres_health(&config, &mut items).await;
        assert!(
            items
                .iter()
                .any(|i| i.category == "postgres" && i.severity == Severity::Error),
            "expected error when postgres selected without db_url"
        );
    }

    #[tokio::test]
    async fn postgres_health_errors_on_bad_scheme() {
        let mut config = Config::default();
        config.memory.backend = "postgres".into();
        config.storage.provider.config.db_url = Some("mysql://host/db".into());
        let mut items = Vec::new();
        check_postgres_health(&config, &mut items).await;
        assert!(
            items.iter().any(|i| i.category == "postgres"
                && i.severity == Severity::Error
                && i.message.contains("postgres://")),
            "expected scheme validation error"
        );
    }

    // ---- FIX-P2-08: embedding endpoint probe ----------------------------

    #[tokio::test]
    async fn embedding_probe_skips_without_custom_provider() {
        // Default provider is "none" -> no embedding probe items.
        let config = Config::default();
        let mut items = Vec::new();
        check_embedding_endpoint(&config, &mut items).await;
        assert!(items.iter().all(|i| i.category != "embedding"));
    }

    #[tokio::test]
    async fn embedding_probe_warns_on_unreachable_custom_endpoint() {
        let mut config = Config::default();
        // Port 1 on localhost is reserved and refuses connections quickly.
        config.memory.embedding_provider = "custom:http://127.0.0.1:1/embed".into();
        let mut items = Vec::new();
        check_embedding_endpoint(&config, &mut items).await;
        assert!(
            items
                .iter()
                .any(|i| i.category == "embedding" && i.severity == Severity::Warn),
            "expected warn for unreachable embedding endpoint"
        );
    }

    // ---- daemon process PATH (v0.8.139) ---------------------------------

    fn environ_blob(entries: &[&str]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for entry in entries {
            bytes.extend_from_slice(entry.as_bytes());
            bytes.push(0);
        }
        bytes
    }

    fn path_value(dirs: &[&Path]) -> String {
        std::env::join_paths(dirs).unwrap().to_string_lossy().into_owned()
    }

    #[test]
    fn daemon_path_from_environ_picks_the_path_entry() {
        let blob = environ_blob(&["HOME=/home/ck", "PATH=/usr/bin:/bin", "LANG=C"]);
        assert_eq!(daemon_path_from_environ(&blob).as_deref(), Some("/usr/bin:/bin"));
    }

    #[test]
    fn daemon_path_from_environ_ignores_path_lookalike_keys() {
        let blob = environ_blob(&["LD_LIBRARY_PATH=/opt/lib", "MANPATH=/usr/share/man"]);
        assert!(daemon_path_from_environ(&blob).is_none());
    }

    #[test]
    fn daemon_path_ok_when_required_directory_is_present() {
        let temp = TempDir::new().unwrap();
        let bin = temp.path().join("cargo-bin");
        std::fs::create_dir_all(&bin).unwrap();
        let other = temp.path().join("usr-bin");
        std::fs::create_dir_all(&other).unwrap();

        let path = path_value(&[other.as_path(), bin.as_path()]);
        let blob = environ_blob(&["HOME=/home/ck", &format!("PATH={path}")]);
        let item = evaluate_daemon_environ(&blob, std::slice::from_ref(&bin));

        assert_eq!(item.severity, Severity::Ok, "message: {}", item.message);
        assert_eq!(item.category, "daemon");
        assert!(
            item.message.contains(&path),
            "ok message must print the daemon PATH, got: {}",
            item.message
        );
    }

    #[test]
    fn daemon_path_warns_when_required_directory_is_missing() {
        let temp = TempDir::new().unwrap();
        let bin = temp.path().join("cargo-bin");
        std::fs::create_dir_all(&bin).unwrap();
        let other = temp.path().join("usr-bin");
        std::fs::create_dir_all(&other).unwrap();

        let path = path_value(&[other.as_path()]);
        let blob = environ_blob(&[&format!("PATH={path}")]);
        let item = evaluate_daemon_environ(&blob, std::slice::from_ref(&bin));

        assert_eq!(item.severity, Severity::Warn, "message: {}", item.message);
        assert!(
            item.message.contains(&bin.display().to_string()),
            "warn message must name the missing directory, got: {}",
            item.message
        );
        assert!(
            item.message.contains("Environment=\"PATH="),
            "warn message must carry the systemd fix, got: {}",
            item.message
        );
        assert!(
            item.message.contains("prx service install"),
            "warn message must carry the re-install fix, got: {}",
            item.message
        );
    }

    #[test]
    fn daemon_path_warns_when_environ_has_no_path_key() {
        let blob = environ_blob(&["HOME=/home/ck", "LANG=C"]);
        let item = evaluate_daemon_environ(&blob, &[PathBuf::from("/home/ck/.cargo/bin")]);

        assert_eq!(item.severity, Severity::Warn, "message: {}", item.message);
        assert!(
            item.message.contains("no PATH entry"),
            "warn message must explain the missing PATH key, got: {}",
            item.message
        );
        assert!(
            item.message.contains("Environment="),
            "warn message must carry the systemd fix, got: {}",
            item.message
        );
    }

    /// `prx doctor` is very often run from a build tree (`cargo run -- doctor`,
    /// `./target/release/prx doctor`, a one-shot CI copy). Requiring the
    /// doctor's own directory there produced a warning about a healthy daemon
    /// *and* told the operator to put a build directory on the unit's PATH.
    ///
    /// MUTATION GUARD: drop the `is_cargo_build_dir` filter and this fails,
    /// because the test binary itself lives in `target/debug/deps`.
    #[test]
    fn a_doctor_running_from_a_build_directory_never_demands_it_on_the_daemon_path() {
        let required = required_daemon_path_dirs(None);
        assert!(
            required.iter().all(|dir| !is_cargo_build_dir(dir)),
            "a build output directory must never be proposed for a systemd unit: {required:?}"
        );
        let item = evaluate_daemon_path("/usr/local/bin:/usr/bin:/bin", &required);
        assert!(
            !item.message.contains("target/debug"),
            "the repair text must not name a build directory: {}",
            item.message
        );
    }

    /// The directory that actually matters is the daemon's, and the daemon
    /// records it. When it does, the doctor's own location is irrelevant.
    #[test]
    fn the_required_directory_comes_from_the_daemon_binary_the_state_file_records() {
        let required = required_daemon_path_dirs(Some(Path::new("/usr/local/bin/prx")));
        assert!(
            required.contains(&canonical_or_owned(Path::new("/usr/local/bin"))),
            "the daemon's own directory must be required: {required:?}"
        );
        assert!(
            required.iter().all(|dir| !is_cargo_build_dir(dir)),
            "a recorded daemon binary replaces the doctor's own directory: {required:?}"
        );

        let complete = required
            .iter()
            .map(|dir| dir.display().to_string())
            .collect::<Vec<_>>()
            .join(":");
        let item = evaluate_daemon_path(&complete, &required);
        assert_eq!(
            item.severity,
            Severity::Ok,
            "a PATH that holds every required directory is healthy: {}",
            item.message
        );

        let item = evaluate_daemon_path("/usr/bin:/bin", &required);
        assert_eq!(item.severity, Severity::Warn);
        assert!(
            item.message.contains("/usr/local/bin"),
            "a PATH without the daemon's own directory must name it: {}",
            item.message
        );
    }

    #[test]
    fn the_daemon_binary_is_read_back_from_the_state_file_when_it_is_recorded() {
        let temp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.config_path = temp.path().join("config.toml");
        assert_eq!(daemon_binary_from_state(&config), None, "no state file, no answer");

        std::fs::write(
            temp.path().join("daemon_state.json"),
            br#"{"pid":1,"binary_path":"/usr/local/bin/prx"}"#,
        )
        .unwrap();
        assert_eq!(
            daemon_binary_from_state(&config),
            Some(PathBuf::from("/usr/local/bin/prx"))
        );

        std::fs::write(
            temp.path().join("daemon_state.json"),
            br#"{"pid":1,"binary_path":"  "}"#,
        )
        .unwrap();
        assert_eq!(daemon_binary_from_state(&config), None, "a blank field is not a path");
    }

    #[test]
    fn cargo_build_directories_are_recognized_and_ordinary_bin_directories_are_not() {
        for build in [
            "/opt/worker/code/prx/target/debug",
            "/opt/worker/code/prx/target/debug/deps",
            "/opt/worker/code/prx/target/release",
            "/opt/worker/code/prx/target/x86_64-unknown-linux-gnu/release",
        ] {
            assert!(
                is_cargo_build_dir(Path::new(build)),
                "must be a build directory: {build}"
            );
        }
        for deployed in ["/usr/local/bin", "/home/ck/.cargo/bin", "/opt/prx/target", "/srv/debug"] {
            assert!(
                !is_cargo_build_dir(Path::new(deployed)),
                "must not be mistaken for a build directory: {deployed}"
            );
        }
    }

    /// A pid outlives the daemon that wrote it. Reading the environment of
    /// whatever process inherited the number judges the daemon PATH from a
    /// stranger's environment.
    #[test]
    fn only_a_process_that_is_prx_is_treated_as_the_daemon() {
        assert!(process_identity_looks_like_prx(
            Some(Path::new("/usr/local/bin/prx")),
            b""
        ));
        assert!(process_identity_looks_like_prx(
            Some(Path::new("/usr/local/bin/prx-node")),
            b""
        ));
        assert!(process_identity_looks_like_prx(
            None,
            b"/usr/local/bin/prx\x00daemon\x00"
        ));
        assert!(!process_identity_looks_like_prx(Some(Path::new("/usr/bin/sleep")), b""));
        assert!(!process_identity_looks_like_prx(None, b"/usr/bin/sleep\x00600\x00"));
        assert!(
            !process_identity_looks_like_prx(Some(Path::new("/opt/worker/code/prx/target/debug/other")), b""),
            "a checkout directory called prx is not a prx process"
        );
        assert!(!process_identity_looks_like_prx(None, b""), "no evidence is not a yes");
    }

    #[test]
    fn daemon_path_check_skips_when_state_has_no_pid() {
        let temp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.config_path = temp.path().join("config.toml");
        std::fs::write(
            temp.path().join("daemon_state.json"),
            br#"{"updated_at":"2026-09-17T00:00:00Z","components":{}}"#,
        )
        .unwrap();

        let mut items = Vec::new();
        check_daemon_process_path(&config, &mut items);

        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.severity, Severity::Ok, "skip must not be an error or warning");
        assert_eq!(item.state, DiagnosticState::Unknown);
        assert!(
            item.message.contains("no pid"),
            "skip message must explain why, got: {}",
            item.message
        );
    }

    #[test]
    fn daemon_path_check_skips_when_state_file_is_absent() {
        let temp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.config_path = temp.path().join("config.toml");

        let mut items = Vec::new();
        check_daemon_process_path(&config, &mut items);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].severity, Severity::Ok);
        assert_eq!(items[0].state, DiagnosticState::Unknown);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn daemon_path_check_skips_for_a_pid_that_does_not_exist() {
        let temp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.config_path = temp.path().join("config.toml");
        // PID above the kernel maximum can never be live.
        std::fs::write(
            temp.path().join("daemon_state.json"),
            br#"{"pid":4294967000,"updated_at":"2026-09-17T00:00:00Z"}"#,
        )
        .unwrap();

        let mut items = Vec::new();
        check_daemon_process_path(&config, &mut items);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].severity, Severity::Ok, "message: {}", items[0].message);
        assert!(
            items[0].message.contains("environ"),
            "skip message must name the unreadable source, got: {}",
            items[0].message
        );
    }

    /// USERINFO-2: `prx doctor` prints every configured provider label. Each
    /// may be a `custom:` URL carrying `user:password@`.
    #[test]
    fn doctor_names_every_provider_without_url_userinfo() {
        let tmp = tempfile::TempDir::new().unwrap();
        let label = |port: u16| format!("custom:http://u:SECRET-fixture-pw@127.0.0.1:{port}/v1");
        let mut config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            default_provider: Some(label(1)),
            ..Config::default()
        };
        // An unparseable fallback and route make doctor print "invalid" lines too.
        config.reliability.fallback_providers =
            vec![label(2), "custom:http://u:SECRET-fixture-pw@127.0.0.1:99999/v1".into()];
        config.model_routes = vec![crate::config::ModelRouteConfig {
            hint: "fast".into(),
            provider: "custom:http://u:SECRET-fixture-pw@127.0.0.1:99998/v1".into(),
            model: "m".into(),
            api_key: None,
            credential_ref: None,
        }];
        let report = diagnose(&config);
        let rendered = report
            .items
            .iter()
            .map(|item| item.message.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains("[redacted]@127.0.0.1"),
            "doctor no longer names the endpoints: {rendered}"
        );
        assert!(
            !rendered.contains("SECRET-fixture-pw"),
            "doctor printed URL userinfo: {rendered}"
        );
    }

    /// R-C2BUg G-1: doctor messages that start with a URL keep none of their
    /// credentials.
    #[test]
    fn doctor_messages_redact_every_url_in_url_first_text() {
        crate::providers::label_redaction_tests::assert_every_url_redacted("doctor", &|text| {
            DiagItem::warn("fixture", text).message
        });
    }

    /// D-063, RED before: the gateway probe resolved `(host, port)`, and a
    /// bracketed IPv6 `gateway.host = "[::1]"` (the spelling a URL uses) did
    /// not resolve, so doctor reported "cannot resolve gateway bind address".
    /// After: the brackets are removed before resolution.
    #[test]
    fn the_gateway_probe_resolves_a_bracketed_ipv6_host() {
        for host in ["[::1]", "::1", "127.0.0.1"] {
            let mut config = Config::default();
            config.gateway.host = host.to_string();
            config.gateway.port = 9;
            let mut items = Vec::new();
            check_gateway_runtime(&config, &mut items);
            let messages: Vec<&str> = items.iter().map(|item| item.message.as_str()).collect();
            assert!(
                !messages.iter().any(|message| message.contains("cannot resolve")),
                "{host}: {messages:?}"
            );
        }
    }

    /// RED before PMMDtail-j (D-084 E-3): `prx doctor`'s auth-profile check
    /// loaded through the migrating load, rewriting auth-profiles.json.
    #[test]
    fn doctor_never_rewrites_the_profiles_file() {
        let tmp = TempDir::new().expect("test: temp dir");
        let path = crate::auth::profiles::write_legacy_token_profile(tmp.path(), "moonshot", "sentinel-moonshot");
        let before = std::fs::read_to_string(&path).expect("test: read");
        let mut config = Config::default();
        config.config_path = tmp.path().join("config.toml");
        config.secrets.encrypt = true;

        let status = active_auth_profile_credential_status(&config, "moonshot");

        assert!(
            matches!(status, AuthProfileCredentialStatus::Present { .. }),
            "{status:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("test: read"),
            before,
            "the file was rewritten"
        );
    }
}
