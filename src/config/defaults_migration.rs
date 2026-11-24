//! One-time removal of old default values that earlier `prx init` builds wrote
//! into the configuration tree.
//!
//! The built-in defaults moved to "no restriction". An installation whose files
//! still hold the former defaults written out explicitly would keep them forever,
//! so the first start of this build resets every value that is still exactly
//! its old default.
//!
//! Rules:
//!
//! - Only a value equal to the old default is reset. A value the operator changed
//!   is left alone. The tree does not record which values `prx init` wrote (the
//!   `.init-defaults.toml` sidecar lists template values only), so equality with
//!   the old default is the criterion.
//! - `workspace_only` and `forbidden_paths` are reset only when
//!   `autonomy.level = "full"`; a lowered install keeps them.
//! - The change goes through the configuration transaction (history record,
//!   revision check), so `prx config rollback` restores the files.
//! - The migration runs once: the marker file records its version, and it is
//!   written whether or not anything was reset. A rolled-back install is not
//!   migrated again; `prx doctor` reports the old values it still holds.

use super::Config;
use super::manage::{ConfigActor, ManagementService};
use crate::security::AutonomyLevel;
use std::path::{Path, PathBuf};

/// Version of this migration. Raising it runs a later migration once more.
const MIGRATION_VERSION: u32 = 1;

/// Marker file beside `config.toml`.
const MARKER_FILE: &str = ".defaults-migration";

/// The `[autonomy] forbidden_paths` list old builds wrote.
const OLD_FORBIDDEN_PATHS: &[&str] = &[
    "/etc",
    "/root",
    "/home",
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/opt",
    "/boot",
    "/dev",
    "/proc",
    "/sys",
    "/var",
    "/tmp",
    "~/.ssh",
    "~/.gnupg",
    "~/.aws",
    "~/.config",
];

/// The `[tool_tiering] channel_exclude` list old builds wrote.
const OLD_CHANNEL_EXCLUDE: &[&str] = &[
    "wasm_plugins_manage",
    "wasm_plugin_reload",
    "wasm_plugins_status",
    "hooks_manage",
    "hooks_status",
    "proxy_config",
    "config_reload",
    "gateway",
    "mcp_status",
    "nodes",
    "skills_manage",
    "memory_reindex",
    "document_sync",
    "document_ingest",
];

/// One former default: its key, the old value (inside `is_old`), and how to
/// reset it to the new built-in default.
struct LegacyDefault {
    key: &'static str,
    /// Reset only while `autonomy.level = "full"`.
    full_only: bool,
    is_old: fn(&Config) -> bool,
    reset: fn(&mut Config),
}

fn equals(list: &[String], old: &[&str]) -> bool {
    list.len() == old.len() && list.iter().zip(old).all(|(a, b)| a == b)
}

/// Every default changed by the "unrestricted by default" work, with its old value.
const LEGACY_DEFAULTS: &[LegacyDefault] = &[
    LegacyDefault {
        key: "autonomy.max_cost_per_day_cents",
        full_only: false,
        is_old: |c| c.autonomy.max_cost_per_day_cents == Some(500),
        reset: |c| c.autonomy.max_cost_per_day_cents = None,
    },
    LegacyDefault {
        key: "cost.daily_limit_usd",
        full_only: false,
        is_old: |c| c.cost.daily_limit_usd == Some(10.0),
        reset: |c| c.cost.daily_limit_usd = None,
    },
    LegacyDefault {
        key: "cost.monthly_limit_usd",
        full_only: false,
        is_old: |c| c.cost.monthly_limit_usd == Some(100.0),
        reset: |c| c.cost.monthly_limit_usd = None,
    },
    LegacyDefault {
        key: "autonomy.workspace_only",
        full_only: true,
        is_old: |c| c.autonomy.workspace_only,
        reset: |c| c.autonomy.workspace_only = false,
    },
    LegacyDefault {
        key: "autonomy.forbidden_paths",
        full_only: true,
        is_old: |c| equals(&c.autonomy.forbidden_paths, OLD_FORBIDDEN_PATHS),
        reset: |c| c.autonomy.forbidden_paths.clear(),
    },
    LegacyDefault {
        key: "multimodal.allow_remote_fetch",
        full_only: false,
        is_old: |c| !c.multimodal.allow_remote_fetch,
        reset: |c| c.multimodal.allow_remote_fetch = true,
    },
    LegacyDefault {
        key: "tool_tiering.channel_exclude",
        full_only: false,
        is_old: |c| equals(&c.tool_tiering.channel_exclude, OLD_CHANNEL_EXCLUDE),
        reset: |c| c.tool_tiering.channel_exclude.clear(),
    },
    LegacyDefault {
        key: "gateway.targeted_agents",
        full_only: false,
        is_old: |c| !c.gateway.targeted_agents,
        reset: |c| c.gateway.targeted_agents = true,
    },
    LegacyDefault {
        key: "nodes.server.allowed_commands",
        full_only: false,
        is_old: |c| c.nodes.server.allowed_commands == ["echo"],
        reset: |c| c.nodes.server.allowed_commands.clear(),
    },
];

/// Keys of `config` that still hold their old default (names only).
#[must_use]
pub(crate) fn legacy_default_keys(config: &Config) -> Vec<&'static str> {
    let full = config.autonomy.level == AutonomyLevel::Full;
    LEGACY_DEFAULTS
        .iter()
        .filter(|entry| (full || !entry.full_only) && (entry.is_old)(config))
        .map(|entry| entry.key)
        .collect()
}

fn marker_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(MARKER_FILE)
}

/// Whether the migration has run (its marker is present at this version).
#[must_use]
pub(crate) fn migration_has_run(config_path: &Path) -> bool {
    std::fs::read_to_string(marker_path(config_path))
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .and_then(|table| table.get("version").and_then(toml::Value::as_integer))
        .is_some_and(|version| version >= i64::from(MIGRATION_VERSION))
}

fn write_marker(config_path: &Path, removed: &[&str]) {
    let list = removed
        .iter()
        .map(|key| format!("\"{key}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let text = format!(
        "# Written once by prx after it reset old default values; key names only.\nversion = {MIGRATION_VERSION}\nremoved = [{list}]\n"
    );
    let path = marker_path(config_path);
    if let Err(error) = std::fs::write(&path, text) {
        tracing::warn!(%error, "could not record the defaults migration; it will be retried at the next start");
    }
}

/// Run the migration once for an existing configuration and return the
/// configuration to use (reloaded from disk when values were reset).
pub(crate) async fn run_once(config: Config) -> Config {
    if migration_has_run(&config.config_path) {
        return config;
    }
    let keys = legacy_default_keys(&config);
    if keys.is_empty() {
        write_marker(&config.config_path, &keys);
        return config;
    }
    let mut candidate = config.clone();
    for entry in LEGACY_DEFAULTS.iter().filter(|entry| keys.contains(&entry.key)) {
        (entry.reset)(&mut candidate);
    }
    let service = ManagementService::deferred(super::new_shared(config.clone()));
    let applied = async {
        let revision = service.revision();
        let plan = service
            .plan_candidate(candidate, revision, ConfigActor::Cli)
            .await?;
        let confirmed = plan.revision().map(str::to_string);
        service.apply(plan, ConfigActor::Cli, confirmed.as_deref()).await
    }
    .await;
    match applied {
        Ok(_) => match Config::load_from_path(&config.config_path, config.workspace_dir.clone()) {
            Ok(reloaded) => {
                tracing::info!(
                    removed = %keys.join(", "),
                    "Reset old default values to the new unrestricted defaults; `prx config rollback` restores them"
                );
                write_marker(&config.config_path, &keys);
                reloaded
            }
            Err(error) => {
                tracing::warn!(%error, "defaults migration applied but the configuration could not be reloaded");
                config
            }
        },
        Err(error) => {
            tracing::warn!(%error, "defaults migration was not applied; it will be retried at the next start");
            config
        }
    }
}
