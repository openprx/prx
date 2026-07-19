#![allow(clippy::print_stdout, clippy::print_stderr)]

use crate::config::Config;
use anyhow::Result;
use chrono::{DateTime, Utc};
use std::collections::HashSet;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
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
    fn with_state(severity: Severity, state: DiagnosticState, category: &'static str, msg: impl Into<String>) -> Self {
        Self {
            severity,
            state,
            category,
            message: msg.into(),
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
    check_workspace(config, &mut items);
    check_daemon_state(config, &mut items);
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
    DoctorReport::new("OpenPRX Doctor - Memory", items)
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
    let cat = "deployed";
    let deployed = Path::new("/home/ck/.cargo/bin/prx");
    if deployed.exists() {
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

    match std::env::current_exe() {
        Ok(current) if current == deployed => items.push(DiagItem::ok(cat, "doctor is running from deployed binary")),
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
    match address.to_socket_addrs().ok().and_then(|mut addrs| addrs.next()) {
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

    // 2. topic readiness: topic/semantic scoping needs embeddings enabled.
    if embeddings_enabled && mem.embedding_dimensions > 0 {
        items.push(DiagItem::ready(
            cat,
            format!("topic: semantic topic scoping ready (dim {})", mem.embedding_dimensions),
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
    if !embeddings_enabled {
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
    let probe = tokio::task::spawn_blocking(move || postgres_probe(&probe_url));
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

    let embedder = crate::memory::create_embedder_from_config(config, config.api_key.as_deref());
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
        println!("  [{}]", provider_name);

        match crate::onboard::run_models_probe_read_only(config, Some(provider_name), use_cache) {
            Ok(()) => {
                ok_count += 1;
                println!("    ✅ model catalog check passed");
            }
            Err(error) => {
                let error_text = format_error_chain(&error);
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

fn check_config_semantics(config: &Config, items: &mut Vec<DiagItem>) {
    let cat = "config";

    if !config.autonomy.workspace_only
        && config.autonomy.forbidden_paths.is_empty()
        && config.autonomy.max_actions_per_hour == u32::MAX
        && config.autonomy.max_cost_per_day_cents == u32::MAX
    {
        items.push(DiagItem::warn(
            cat,
            "explicit unrestricted autonomy profile: host-wide paths and unbounded action/cost ceilings",
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
        if let Some(reason) = provider_validation_error(provider) {
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
        if let Some(reason) = provider_validation_error(fb) {
            items.push(DiagItem::warn(
                cat,
                format!("fallback provider \"{fb}\" is invalid: {reason}"),
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
                config.default_provider.as_deref().unwrap_or("openrouter"),
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
        config.default_provider.as_deref().unwrap_or("openrouter"),
        config.api_key.as_deref(),
        &config.reliability,
        &crate::providers::provider_runtime_options_from_config(config),
    );
    if availability.degraded {
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
        if let Some(reason) = provider_validation_error(&route.provider) {
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
        if let Some(reason) = provider_validation_error(&agent.provider) {
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
    let store = crate::auth::profiles::AuthProfilesStore::new(openprx_dir, config.secrets.encrypt);
    let data = match store.load() {
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

fn provider_validation_error(name: &str) -> Option<String> {
    match crate::providers::create_provider(name, None) {
        Ok(_) => None,
        Err(err) => Some(err.to_string().lines().next().unwrap_or("invalid provider").into()),
    }
}

fn model_has_reachable_provider(
    default_provider: &str,
    fallback_providers: &[String],
    route_providers: &HashSet<String>,
    model: &str,
) -> bool {
    if crate::providers::provider_matches_model_prefix(default_provider, model) {
        return true;
    }
    if fallback_providers
        .iter()
        .any(|provider| crate::providers::provider_matches_model_prefix(provider, model))
    {
        return true;
    }
    route_providers
        .iter()
        .any(|provider| crate::providers::provider_matches_model_prefix(provider, model))
}

fn embedding_provider_validation_error(name: &str) -> Option<String> {
    let normalized = name.trim();
    if normalized.eq_ignore_ascii_case("none") || normalized.eq_ignore_ascii_case("openai") {
        return None;
    }

    let Some(url) = normalized.strip_prefix("custom:") else {
        return Some("supported values: none, openai, custom:<url>".into());
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

            if status_ok && age <= CHANNEL_STALE_SECONDS {
                items.push(DiagItem::ok(cat, format!("{name} fresh ({age}s ago)")));
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

    struct EnvGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvGuard {
        #[allow(unsafe_code)]
        fn set(key: &'static str, value: Option<&str>) -> Self {
            let original = std::env::var(key).ok();
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
        assert!(provider_validation_error("openrouter").is_none());
        assert!(provider_validation_error("custom:https://example.com").is_none());
        assert!(provider_validation_error("anthropic-custom:https://example.com").is_none());

        let invalid_custom = provider_validation_error("custom:").unwrap_or_default();
        assert!(invalid_custom.contains("requires a URL"));

        let invalid_unknown = provider_validation_error("totally-fake").unwrap_or_default();
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
        config.autonomy.max_actions_per_hour = u32::MAX;
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
    fn config_validation_warns_for_explicit_delegate_wildcard() {
        let mut config = Config::default();
        config.agents.insert(
            "worker".into(),
            crate::config::DelegateAgentConfig {
                provider: "openrouter".into(),
                model: "model-test".into(),
                system_prompt: None,
                api_key: None,
                temperature: None,
                max_depth: 3,
                agentic: true,
                allowed_tools: vec!["*".into()],
                max_iterations: 10,
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

    #[test]
    fn config_validation_warns_empty_model_route() {
        let mut config = Config::default();
        config.model_routes = vec![crate::config::ModelRouteConfig {
            hint: "fast".into(),
            provider: "groq".into(),
            model: String::new(),
            api_key: None,
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
                temperature: None,
                max_depth: 3,
                agentic: false,
                allowed_tools: Vec::new(),
                max_iterations: 10,
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
                temperature: None,
                max_depth: 3,
                agentic: false,
                allowed_tools: Vec::new(),
                max_iterations: 10,
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
}
