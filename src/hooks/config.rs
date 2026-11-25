use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

use super::event::{HookEvent, normalize_event_name};
use super::{DEFAULT_TIMEOUT_MS, HOOKS_JSON_FILE, MAX_ACTIONS, MAX_ARGS_PER_ACTION, MAX_ENV_PER_ACTION, MAX_HOOKS_FILE_BYTES, MAX_TIMEOUT_MS};

#[derive(Debug, Clone, Deserialize)]
pub(super) struct HookAction {
    pub(super) command: String,
    #[serde(default)]
    pub(super) args: Vec<String>,
    #[serde(default)]
    pub(super) env: HashMap<String, String>,
    #[serde(default)]
    pub(super) cwd: Option<String>,
    #[serde(default)]
    pub(super) timeout_ms: Option<u64>,
    #[serde(default = "default_stdin_json")]
    pub(super) stdin_json: bool,
}

const fn default_stdin_json() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Default)]
pub(super) struct HooksFile {
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    hooks: HashMap<String, Vec<HookAction>>,
}

#[derive(Debug, Clone)]
pub(super) struct HookConfig {
    pub(super) timeout_ms: u64,
    pub(super) hooks: HashMap<String, Vec<HookAction>>,
}

impl Default for HookConfig {
    fn default() -> Self {
        Self {
            timeout_ms: DEFAULT_TIMEOUT_MS,
            hooks: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct RuntimeState {
    pub(super) config: HookConfig,
    pub(super) hooks_json_digest: Option<[u8; 32]>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HookDiagnostics {
    pub configured: bool,
    pub valid: bool,
    pub config_path: String,
    pub timeout_ms: u64,
    pub action_count: usize,
    pub events: BTreeMap<String, usize>,
    /// Private catalog identity for the effective hooks file. Not returned
    /// through doctor/API diagnostics.
    #[serde(skip)]
    pub(crate) catalog_digest: Option<[u8; 32]>,
    pub error: Option<String>,
    /// Whether the command hooks of this `hooks.json` run: only with
    /// `security.workspace_hooks = "on"` in config.toml (HOOKS-ENV).
    pub runs: bool,
    /// When configured hooks do not run: why, with the one-line fix.
    pub blocked: Option<String>,
}

/// The one line that allows workspace command hooks.
pub const WORKSPACE_HOOKS_OPT_IN: &str = "[security] workspace_hooks = \"on\"";

/// How many command hooks the workspace `hooks.json` under
/// `workspace_dir` defines; `None` without one. Reads and validates the
/// file only (for `prx doctor`).
///
/// # Errors
///
/// An unreadable, oversized or invalid `hooks.json`.
pub fn workspace_hook_actions(workspace_dir: &Path) -> anyhow::Result<Option<usize>> {
    let Some(raw) = read_bounded_file(&workspace_dir.join(HOOKS_JSON_FILE), MAX_HOOKS_FILE_BYTES)? else {
        return Ok(None);
    };
    Ok(Some(parse_hook_config(&raw)?.hooks.values().map(Vec::len).sum()))
}

/// Why the command hooks of a `hooks.json` holding `actions` actions do not
/// run, with the fix; `None` when there are none.
#[must_use]
pub fn blocked_hooks_note(actions: usize) -> Option<String> {
    (actions > 0).then(|| {
        format!(
            "{actions} workspace command hook(s) in hooks.json are not run: `security.workspace_hooks` is off. To \
             run them, add `{WORKSPACE_HOOKS_OPT_IN}` to config.toml (a high-risk change: the workspace can then \
             start programs that run as PRX's user)"
        )
    })
}

/// What a change to the workspace hooks can do: the one risk
/// classification of `hooks.json` changes on every surface (`hooks_manage`,
/// the Gateway hooks API; D-096). The file tools classify any write of the
/// file as a launch-definition write, which is High as well.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookChange {
    /// Installs, changes or runs a hook command: its program, arguments or
    /// the event it runs on.
    Command,
    /// Only removes hooks or changes their limits: nothing new runs.
    Restriction,
}

impl HookChange {
    /// The side-effect risk of this change.
    #[must_use]
    pub const fn risk(self) -> crate::security::policy::ResourceRiskLevel {
        match self {
            Self::Command => crate::security::policy::ResourceRiskLevel::High,
            Self::Restriction => crate::security::policy::ResourceRiskLevel::Medium,
        }
    }

    /// The change a `hooks_manage` action makes; `None` for the actions
    /// that only read.
    #[must_use]
    pub fn of_manage_action(action: &str) -> Option<Self> {
        match action {
            "test" | "replace" => Some(Self::Command),
            "remove" => Some(Self::Restriction),
            _ => None,
        }
    }
}

pub(super) fn bounded_timeout(timeout_ms: u64) -> anyhow::Result<u64> {
    if timeout_ms == 0 || timeout_ms > MAX_TIMEOUT_MS {
        anyhow::bail!("hook timeout_ms must be between 1 and {MAX_TIMEOUT_MS}");
    }
    Ok(timeout_ms)
}

fn validate_hooks_file(file: &HooksFile) -> anyhow::Result<()> {
    if let Some(timeout_ms) = file.timeout_ms {
        bounded_timeout(timeout_ms)?;
    }
    let action_count = file.hooks.values().map(Vec::len).sum::<usize>();
    if action_count > MAX_ACTIONS {
        anyhow::bail!("hooks.json has {action_count} actions; limit is {MAX_ACTIONS}");
    }
    let mut normalized_events = std::collections::HashSet::new();
    for event in file.hooks.keys() {
        let normalized = normalize_event_name(event);
        if HookEvent::from_name(&normalized).is_none() {
            anyhow::bail!("unknown hook event '{event}'");
        }
        if !normalized_events.insert(normalized) {
            anyhow::bail!("duplicate hook event after normalization: '{event}'");
        }
    }
    for actions in file.hooks.values() {
        for action in actions {
            if action.args.len() > MAX_ARGS_PER_ACTION {
                anyhow::bail!("hook action has too many arguments");
            }
            if action.env.len() > MAX_ENV_PER_ACTION {
                anyhow::bail!("hook action has too many environment entries");
            }
            if let Some(timeout_ms) = action.timeout_ms {
                bounded_timeout(timeout_ms)?;
            }
            // A hook starts in its dedicated directory beside config.toml.
            // `cwd` is workspace-controlled, so it is refused here rather
            // than accepted and silently ignored.
            if action.cwd.is_some() {
                anyhow::bail!(
                    "hooks.json `cwd` is not supported: hooks always start in their dedicated `hooks/` directory \
                     beside config.toml; remove `cwd` and use an absolute path or a workspace-relative `command`"
                );
            }
        }
    }
    Ok(())
}

pub(super) fn parse_hook_config(raw: &[u8]) -> anyhow::Result<HookConfig> {
    let parsed: HooksFile = serde_json::from_slice(raw)?;
    validate_hooks_file(&parsed)?;
    Ok(HookConfig {
        timeout_ms: bounded_timeout(parsed.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS))?,
        hooks: parsed
            .hooks
            .into_iter()
            .map(|(name, actions)| (normalize_event_name(&name), actions))
            .collect(),
    })
}

pub(super) fn read_bounded_file(path: &Path, max_bytes: u64) -> anyhow::Result<Option<Vec<u8>>> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if metadata.len() > max_bytes {
        anyhow::bail!("{} exceeds {max_bytes} byte limit", path.display());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    std::io::Read::by_ref(&mut file)
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        anyhow::bail!("{} exceeds {max_bytes} byte limit", path.display());
    }
    Ok(Some(bytes))
}
