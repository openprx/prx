use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use std::path::Path;

use super::schema::ModulesConfig;

// ── Spec preset enum ────────────────────────────────────────────

/// Configuration preset for `prx init`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Spec {
    /// Bare-minimum: memory + agent only
    Minimal,
    /// Production server: memory + agent + network + security + tools + integrations
    Server,
    /// Everything enabled
    Full,
}

impl Spec {
    /// Human-readable name for display.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Server => "server",
            Self::Full => "full",
        }
    }

    /// Return the `ModulesConfig` for this preset.
    pub const fn modules(self) -> ModulesConfig {
        match self {
            Self::Minimal => ModulesConfig {
                memory: true,
                channels: false,
                network: false,
                security: false,
                scheduler: false,
                agent: true,
                identity: false,
                routing: false,
                tools: false,
                integrations: false,
                nodes: false,
                cost: false,
                observability: false,
            },
            Self::Server => ModulesConfig {
                memory: true,
                channels: false,
                network: true,
                security: true,
                scheduler: false,
                agent: true,
                identity: false,
                routing: false,
                tools: true,
                integrations: true,
                nodes: false,
                cost: false,
                observability: false,
            },
            Self::Full => ModulesConfig::all_enabled(),
        }
    }

    /// Count of enabled modules for this preset.
    const fn enabled_count(self) -> usize {
        let m = self.modules();
        m.memory as usize
            + m.channels as usize
            + m.network as usize
            + m.security as usize
            + m.scheduler as usize
            + m.agent as usize
            + m.identity as usize
            + m.routing as usize
            + m.tools as usize
            + m.integrations as usize
            + m.nodes as usize
            + m.cost as usize
            + m.observability as usize
    }

    /// Generate the full configuration tree into `target_dir`.
    pub fn generate(self, target_dir: &Path, force: bool) -> Result<()> {
        // 1. Check for existing configuration
        if target_dir.join("config.toml").exists() && !force {
            bail!(
                "Configuration already exists at {}. Use --force to overwrite.",
                target_dir.display()
            );
        }

        // 2. Create directory structure
        std::fs::create_dir_all(target_dir.join("config.d"))
            .with_context(|| format!("Failed to create config.d in {}", target_dir.display()))?;
        std::fs::create_dir_all(target_dir.join("workspace"))
            .with_context(|| format!("Failed to create workspace in {}", target_dir.display()))?;

        for subdir in &["sessions", "memory", "state", "cron", "skills"] {
            std::fs::create_dir_all(target_dir.join("workspace").join(subdir))
                .with_context(|| format!("Failed to create workspace/{subdir} in {}", target_dir.display()))?;
        }

        // 3. Write config.toml
        let config_content = main_config_template(self);
        write_config_file(&target_dir.join("config.toml"), &config_content)?;

        // 4. Write config.d/*.toml (only for enabled modules)
        let modules = self.modules();

        if modules.memory {
            write_config_file(&target_dir.join("config.d/memory.toml"), &memory_template(self))?;
        }
        if modules.channels {
            write_config_file(&target_dir.join("config.d/channels.toml"), &channels_template(self))?;
        }
        if modules.network {
            write_config_file(&target_dir.join("config.d/network.toml"), &network_template(self))?;
        }
        if modules.security {
            write_config_file(&target_dir.join("config.d/security.toml"), &security_template(self))?;
        }
        if modules.scheduler {
            write_config_file(&target_dir.join("config.d/scheduler.toml"), &scheduler_template(self))?;
        }
        if modules.agent {
            write_config_file(&target_dir.join("config.d/agent.toml"), &agent_template(self))?;
        }
        if modules.identity {
            write_config_file(&target_dir.join("config.d/identity.toml"), &identity_template(self))?;
        }
        if modules.routing {
            write_config_file(&target_dir.join("config.d/routing.toml"), &routing_template(self))?;
        }
        if modules.tools {
            write_config_file(&target_dir.join("config.d/tools.toml"), &tools_template(self))?;
        }
        if modules.integrations {
            write_config_file(
                &target_dir.join("config.d/integrations.toml"),
                &integrations_template(self),
            )?;
        }
        if modules.nodes {
            write_config_file(&target_dir.join("config.d/nodes.toml"), &nodes_template(self))?;
        }
        if modules.cost {
            write_config_file(&target_dir.join("config.d/cost.toml"), &cost_template(self))?;
        }
        if modules.observability {
            write_config_file(
                &target_dir.join("config.d/observability.toml"),
                &observability_template(self),
            )?;
        }

        // 5. Set directory permissions (Unix only)
        #[cfg(unix)]
        set_directory_permissions(target_dir)?;

        // 6. Log summary
        tracing::info!("PRX configuration initialized ({spec})", spec = self.name());
        tracing::info!("  Config dir: {}", target_dir.display());
        tracing::info!("  Modules enabled: {}/13", self.enabled_count());
        tracing::info!("  Config files: config.toml + {} module files", self.enabled_count());

        Ok(())
    }
}

// ── File I/O helpers ────────────────────────────────────────────

fn write_config_file(path: &Path, content: &str) -> Result<()> {
    std::fs::write(path, content).with_context(|| format!("Failed to write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("Failed to set permissions on {}", path.display()))?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_directory_permissions(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("Failed to set permissions on {}", dir.display()))?;
    let config_d = dir.join("config.d");
    if config_d.exists() {
        std::fs::set_permissions(&config_d, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("Failed to set permissions on {}", config_d.display()))?;
    }
    Ok(())
}

// ── Main config.toml template ───────────────────────────────────

fn main_config_template(spec: Spec) -> String {
    let m = spec.modules();
    format!(
        r#"# PRX Configuration
# Generated by: prx init --spec {spec}
# Detailed module configs in config.d/

default_model = "claude-sonnet-4-6"
default_provider = "anthropic"
default_temperature = 0.7

[modules]
memory = {memory}
channels = {channels}
network = {network}
security = {security}
scheduler = {scheduler}
agent = {agent}
identity = {identity}
routing = {routing}
tools = {tools}
integrations = {integrations}
nodes = {nodes}
cost = {cost}
observability = {observability}
"#,
        spec = spec.name(),
        memory = m.memory,
        channels = m.channels,
        network = m.network,
        security = m.security,
        scheduler = m.scheduler,
        agent = m.agent,
        identity = m.identity,
        routing = m.routing,
        tools = m.tools,
        integrations = m.integrations,
        nodes = m.nodes,
        cost = m.cost,
        observability = m.observability,
    )
}

// ── Module templates ────────────────────────────────────────────
//
// Each function returns a static TOML string for the given module.
// The detail level varies by spec:
//   minimal  — essential defaults, sparse comments
//   server   — production-ready defaults, moderate comments
//   full     — all options shown, extensive comments

fn memory_template(spec: Spec) -> String {
    match spec {
        Spec::Minimal => r#"# Memory configuration (minimal)

[memory]
backend = "sqlite"
auto_save = true
"#
        .into(),

        Spec::Server => r#"# Memory configuration (server)
# Backend: sqlite (recommended), markdown, or none

[memory]
backend = "sqlite"
auto_save = true
max_results = 20

[storage]
[storage.provider]
[storage.provider.config]
# Database path is auto-resolved to workspace/memory/
"#
        .into(),

        Spec::Full => r#"# Memory configuration (full)
# Backend options: sqlite (recommended), markdown, none
# Auto-save persists conversation context across sessions

[memory]
backend = "sqlite"
auto_save = true
max_results = 20

# Embedding configuration for semantic search
# [memory.embedding]
# provider = "openai"
# model = "text-embedding-3-small"
# dimension = 1536

[storage]
[storage.provider]
[storage.provider.config]
# Database path is auto-resolved to workspace/memory/
# For external databases, set connection string here
"#
        .into(),
    }
}

fn channels_template(spec: Spec) -> String {
    match spec {
        // channels is only enabled in full spec
        Spec::Minimal | Spec::Server => String::new(),

        Spec::Full => r#"# Channels configuration (full)
# Connect PRX to messaging platforms: Telegram, Discord, Slack, etc.

[channels_config]
# Uncomment and configure the channels you need:

# [channels_config.telegram]
# bot_token = ""
# allowed_users = ["your_username"]
# stream_mode = "edit"
# mention_only = false

# [channels_config.discord]
# bot_token = ""
# guild_id = ""
# allowed_users = []
# listen_to_bots = false
# mention_only = false

# [channels_config.slack]
# bot_token = ""
# app_token = ""
# allowed_users = []

# [channels_config.matrix]
# homeserver_url = "https://matrix.org"
# user_id = "@bot:matrix.org"
# access_token = ""
# allowed_rooms = []

# [channels_config.lark]
# app_id = ""
# app_secret = ""
# receive_mode = "websocket"
# mention_only = false
"#
        .into(),
    }
}

fn network_template(spec: Spec) -> String {
    match spec {
        Spec::Minimal => String::new(),

        Spec::Server => r#"# Network configuration (server)
# Gateway, tunnel, and proxy settings

[gateway]
host = "127.0.0.1"
port = 3120
enable_pairing = false

[tunnel]
enabled = false
# provider = "cloudflared"
# domain = ""

[proxy]
# global_proxy = ""
"#
        .into(),

        Spec::Full => r#"# Network configuration (full)
# Gateway server, tunnel exposure, and proxy settings

[gateway]
host = "127.0.0.1"
port = 3120
enable_pairing = false
# rate_limit_rpm = 60

# Tunnel for exposing gateway to the internet
[tunnel]
enabled = false
# provider = "cloudflared"        # cloudflared | ngrok | localtunnel
# domain = ""                      # custom domain if supported
# auth_token = ""                  # tunnel provider auth token

# Outbound proxy for HTTP/HTTPS/SOCKS5
[proxy]
# global_proxy = ""                # e.g. "socks5://127.0.0.1:1080"
# no_proxy = "localhost,127.0.0.1" # comma-separated bypass list
# Per-service proxy overrides:
# [proxy.service_overrides]
# "provider.openai" = "http://proxy:8080"
"#
        .into(),
    }
}

fn security_template(spec: Spec) -> String {
    match spec {
        Spec::Minimal => String::new(),

        Spec::Server => r#"# Security configuration (server)
# Autonomy limits, sandboxing, and secret management

[autonomy]
level = "supervised"
workspace_only = true
max_actions_per_hour = 100
max_cost_per_day_cents = 500
allowed_commands = ["git", "ls", "cat", "grep", "find", "head", "tail", "wc"]

[secrets]
encrypt = true

[security]
[security.sandbox]
enabled = false
# backend = "native"

[security.resource_limits]
max_memory_mb = 512
max_cpu_seconds = 300
max_file_size_mb = 100
"#
        .into(),

        Spec::Full => r#"# Security configuration (full)
# Autonomy policy, sandboxing, resource limits, audit, and secrets

[autonomy]
level = "supervised"                   # supervised | autonomous | locked
workspace_only = true
max_actions_per_hour = 100
max_cost_per_day_cents = 500
allowed_commands = ["git", "ls", "cat", "grep", "find", "head", "tail", "wc", "curl"]

[secrets]
encrypt = true

[security]
[security.sandbox]
enabled = false
# backend = "native"                  # native | docker | bubblewrap

[security.resource_limits]
max_memory_mb = 512
max_cpu_seconds = 300
max_file_size_mb = 100

[security.audit]
enabled = false
# log_path = "workspace/audit.log"

# Tool-level policy overrides
# [security.tool_policy]
# default_action = "allow"            # allow | deny | ask
# [security.tool_policy.overrides]
# "shell" = "ask"
# "file_write" = "ask"
"#
        .into(),
    }
}

fn scheduler_template(spec: Spec) -> String {
    match spec {
        // scheduler is only enabled in full spec
        Spec::Minimal | Spec::Server => String::new(),

        Spec::Full => r#"# Scheduler configuration (full)
# Periodic tasks, cron jobs, heartbeat, and Xin engine

[scheduler]
enabled = true
max_concurrent = 4
# storage_path = "workspace/cron/scheduler.db"

[cron]
# Pre-defined cron jobs loaded at startup
# [[cron.jobs]]
# expression = "0 9 * * 1-5"
# command = "Good morning briefing"
# timezone = "UTC"

[heartbeat]
enabled = true
interval_minutes = 5

[xin]
enabled = false
# cycle_interval_secs = 3600
# max_concurrent_tasks = 2
"#
        .into(),
    }
}

fn agent_template(spec: Spec) -> String {
    match spec {
        Spec::Minimal => r#"# Agent configuration (minimal)

[agent]
max_tool_calls = 25
max_turns = 50
"#
        .into(),

        Spec::Server => r#"# Agent configuration (server)
# Orchestration, session spawning, and self-system

[agent]
max_tool_calls = 25
max_turns = 50
streaming = true

[agent.compaction]
mode = "sliding_window"
max_context_tokens = 100000

[sessions_spawn]
enabled = false
# max_concurrent = 4

[self_system]
enabled = false
"#
        .into(),

        Spec::Full => r#"# Agent configuration (full)
# Agent orchestration, sessions, self-system, causal tree, and delegates

[agent]
max_tool_calls = 25
max_turns = 50
streaming = true

# Context compaction to manage long conversations
[agent.compaction]
mode = "sliding_window"               # sliding_window | summarize | none
max_context_tokens = 100000

# Session spawning for parallel task execution
[sessions_spawn]
enabled = false
max_concurrent = 4
# timeout_secs = 300

# Self-system for autonomous behavior
[self_system]
enabled = false
# evolution_enabled = false

# Causal tree for structured reasoning
[causal_tree]
enabled = false

# Delegate agents for multi-agent workflows
# [agents.researcher]
# provider = "anthropic"
# model = "claude-sonnet-4-6"
# system_prompt = "You are a research assistant."
# agentic = true
# max_iterations = 20
# allowed_tools = ["web_search", "read_file"]
"#
        .into(),
    }
}

fn identity_template(spec: Spec) -> String {
    match spec {
        // identity is only enabled in full spec
        Spec::Minimal | Spec::Server => String::new(),

        Spec::Full => r#"# Identity configuration (full)
# User identity bindings, policies, and auth profile settings

[identity]
# format = "openprx"                  # openprx | aieos

[auth]
# import_codex_auth = false

# Static identity bindings
# [[identity_bindings]]
# channel = "telegram"
# external_id = "username"
# internal_id = "user-uuid"

# User policy records
# [[user_policies]]
# user_id = "user-uuid"
# max_actions_per_hour = 50
# allowed_tools = ["web_search", "read_file"]
"#
        .into(),
    }
}

fn routing_template(spec: Spec) -> String {
    match spec {
        // routing is only enabled in full spec
        Spec::Minimal | Spec::Server => String::new(),

        Spec::Full => r#"# Routing configuration (full)
# LLM router, model/embedding routes, query classification, and task routing

[router]
enabled = false
# Scoring weights (alpha + beta + gamma + delta + epsilon = 1.0)
# alpha = 0.0    # similarity score weight
# beta = 0.5     # capability score weight
# gamma = 0.3    # Elo score weight
# delta = 0.1    # cost penalty coefficient
# epsilon = 0.1  # latency penalty coefficient

# Model routes: map hint:<name> to provider+model
# [[model_routes]]
# hint = "fast"
# provider = "openrouter"
# model = "meta-llama/llama-3.3-70b-instruct"

# [[model_routes]]
# hint = "smart"
# provider = "anthropic"
# model = "claude-sonnet-4-6"

# Embedding routes
# [[embedding_routes]]
# hint = "default"
# provider = "openai"
# model = "text-embedding-3-small"

# Query classification: auto-route user messages
[query_classification]
enabled = false
# [[query_classification.rules]]
# pattern = "translate|翻译"
# hint = "fast"

# Task routing: classify work by intent
[task_routing]
enabled = false
"#
        .into(),
    }
}

fn tools_template(spec: Spec) -> String {
    match spec {
        Spec::Minimal => String::new(),

        Spec::Server => r#"# Tools configuration (server)
# Browser, HTTP, web search, media, and skills

[browser]
enabled = false
# headless = true

[http_request]
enabled = true
timeout_secs = 30
max_response_bytes = 10485760

[web_search]
enabled = false
# provider = "tavily"
# api_key = ""

[multimodal]
enabled = true

[media]
# audio_stt_enabled = false
# video_frame_extraction = false

[skills]
enabled = true
auto_discover = true
"#
        .into(),

        Spec::Full => r#"# Tools configuration (full)
# Browser automation, HTTP requests, web search, media, skills, and skill RAG

[browser]
enabled = false
# headless = true
# [browser.computer_use]
# enabled = false
# display_width = 1280
# display_height = 720

[http_request]
enabled = true
timeout_secs = 30
max_response_bytes = 10485760
# allowed_domains = []                 # empty = all allowed

[web_search]
enabled = false
# provider = "tavily"                  # tavily | searxng | brave
# api_key = ""

[multimodal]
enabled = true
# max_image_size_bytes = 20971520

[media]
# audio_stt_enabled = false
# video_frame_extraction = false

[skills]
enabled = true
auto_discover = true
# community_repo = ""

[skill_rag]
enabled = false
# max_results = 5
"#
        .into(),
    }
}

fn integrations_template(spec: Spec) -> String {
    match spec {
        Spec::Minimal => String::new(),

        Spec::Server => r#"# Integrations configuration (server)
# MCP servers, Composio, and webhooks

[mcp]
# MCP server connections
# [[mcp.servers]]
# name = "my-server"
# transport = "stdio"
# command = "npx"
# args = ["-y", "my-mcp-server"]

[composio]
enabled = false

[webhook]
enabled = false
"#
        .into(),

        Spec::Full => r#"# Integrations configuration (full)
# MCP tool servers, Composio managed OAuth, and webhook receivers

[mcp]
# MCP (Model Context Protocol) server connections
# [[mcp.servers]]
# name = "filesystem"
# transport = "stdio"
# command = "npx"
# args = ["-y", "@modelcontextprotocol/server-filesystem", "/path"]

# [[mcp.servers]]
# name = "remote-api"
# transport = "sse"
# url = "http://localhost:8090/sse"

[composio]
enabled = false
# api_key = ""
# tools = []

[webhook]
enabled = false
# secret = ""
# topics = []
"#
        .into(),
    }
}

fn nodes_template(spec: Spec) -> String {
    match spec {
        // nodes is only enabled in full spec
        Spec::Minimal | Spec::Server => String::new(),

        Spec::Full => r#"# Nodes configuration (full)
# Remote node proxy for distributed PRX deployments

[nodes]
enabled = false

# Remote node connections
# [[nodes.servers]]
# name = "worker-1"
# url = "https://worker-1.example.com:3120"
# api_key = ""
# weight = 1
"#
        .into(),
    }
}

fn cost_template(spec: Spec) -> String {
    match spec {
        // cost is only enabled in full spec
        Spec::Minimal | Spec::Server => String::new(),

        Spec::Full => r#"# Cost configuration (full)
# Token usage tracking and budget enforcement

[cost]
enabled = false
# daily_budget_usd = 10.0
# monthly_budget_usd = 200.0
# alert_threshold_percent = 80
# storage_path = "workspace/cost/usage.db"
"#
        .into(),
    }
}

fn observability_template(spec: Spec) -> String {
    match spec {
        // observability is only enabled in full spec
        Spec::Minimal | Spec::Server => String::new(),

        Spec::Full => r#"# Observability configuration (full)
# Logging, metrics, runtime adapter, and reliability

[observability]
backend = "tracing"
# level = "info"
# otlp_endpoint = ""

[runtime]
kind = "native"
# [runtime.docker]
# image = "openprx/runtime:latest"
# network = "host"

[reliability]
max_retries = 3
base_backoff_ms = 1000
# fallback_providers = ["openrouter", "openai"]
"#
        .into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn spec_name_matches_variant() {
        assert_eq!(Spec::Minimal.name(), "minimal");
        assert_eq!(Spec::Server.name(), "server");
        assert_eq!(Spec::Full.name(), "full");
    }

    #[test]
    fn minimal_enables_memory_and_agent_only() {
        let m = Spec::Minimal.modules();
        assert!(m.memory);
        assert!(m.agent);
        assert!(!m.channels);
        assert!(!m.network);
        assert!(!m.security);
        assert!(!m.scheduler);
        assert!(!m.identity);
        assert!(!m.routing);
        assert!(!m.tools);
        assert!(!m.integrations);
        assert!(!m.nodes);
        assert!(!m.cost);
        assert!(!m.observability);
    }

    #[test]
    fn server_enables_six_modules() {
        let m = Spec::Server.modules();
        assert!(m.memory);
        assert!(m.agent);
        assert!(m.network);
        assert!(m.security);
        assert!(m.tools);
        assert!(m.integrations);
        // disabled in server
        assert!(!m.channels);
        assert!(!m.scheduler);
        assert!(!m.identity);
        assert!(!m.routing);
        assert!(!m.nodes);
        assert!(!m.cost);
        assert!(!m.observability);
    }

    #[test]
    fn full_enables_all_modules() {
        let m = Spec::Full.modules();
        assert!(m.memory);
        assert!(m.channels);
        assert!(m.network);
        assert!(m.security);
        assert!(m.scheduler);
        assert!(m.agent);
        assert!(m.identity);
        assert!(m.routing);
        assert!(m.tools);
        assert!(m.integrations);
        assert!(m.nodes);
        assert!(m.cost);
        assert!(m.observability);
    }

    #[test]
    fn enabled_count_is_correct() {
        assert_eq!(Spec::Minimal.enabled_count(), 2);
        assert_eq!(Spec::Server.enabled_count(), 6);
        assert_eq!(Spec::Full.enabled_count(), 13);
    }

    #[test]
    fn main_config_template_contains_spec_name() {
        let content = main_config_template(Spec::Server);
        assert!(content.contains("--spec server"));
        assert!(content.contains("[modules]"));
        assert!(content.contains("default_model"));
    }

    #[test]
    fn generate_creates_expected_files() {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();

        Spec::Minimal.generate(dir, false).expect("test: generate minimal");

        assert!(dir.join("config.toml").exists());
        assert!(dir.join("config.d").is_dir());
        assert!(dir.join("workspace/sessions").is_dir());
        assert!(dir.join("workspace/memory").is_dir());
        assert!(dir.join("workspace/state").is_dir());
        assert!(dir.join("workspace/cron").is_dir());
        assert!(dir.join("workspace/skills").is_dir());

        // minimal: memory + agent
        assert!(dir.join("config.d/memory.toml").exists());
        assert!(dir.join("config.d/agent.toml").exists());
        assert!(!dir.join("config.d/channels.toml").exists());
        assert!(!dir.join("config.d/network.toml").exists());
    }

    #[test]
    fn generate_full_creates_all_module_files() {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();

        Spec::Full.generate(dir, false).expect("test: generate full");

        for name in &[
            "memory.toml",
            "channels.toml",
            "network.toml",
            "security.toml",
            "scheduler.toml",
            "agent.toml",
            "identity.toml",
            "routing.toml",
            "tools.toml",
            "integrations.toml",
            "nodes.toml",
            "cost.toml",
            "observability.toml",
        ] {
            assert!(dir.join("config.d").join(name).exists(), "missing config.d/{name}");
        }
    }

    #[test]
    fn generate_refuses_overwrite_without_force() {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();

        Spec::Minimal.generate(dir, false).expect("test: first generate");
        let result = Spec::Minimal.generate(dir, false);
        assert!(result.is_err());
        assert!(
            result
                .as_ref()
                .err()
                .map_or(false, |e| format!("{e}").contains("--force"))
        );
    }

    #[test]
    fn generate_allows_overwrite_with_force() {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();

        Spec::Minimal.generate(dir, false).expect("test: first generate");
        Spec::Server.generate(dir, true).expect("test: force overwrite");

        let content = fs::read_to_string(dir.join("config.toml")).expect("test: read config");
        assert!(content.contains("--spec server"));
    }

    #[cfg(unix)]
    #[test]
    fn generated_files_have_restricted_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();

        Spec::Minimal.generate(dir, false).expect("test: generate minimal");

        let config_perms = fs::metadata(dir.join("config.toml"))
            .expect("test: config metadata")
            .permissions()
            .mode();
        // Check that the file permission bits (lower 9 bits) are 0o600
        assert_eq!(config_perms & 0o777, 0o600);
    }
}
