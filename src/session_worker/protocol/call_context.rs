//! The parent-decided call context a process worker receives in its manifest.
//!
//! The parent computes who the worker acts for, at which level, where, on
//! which model and with which published surface, and seals the result into the
//! manifest. The manifest HMAC covers it like every other field. The worker
//! builds its call context from this value and never derives any of it from
//! its own configuration; the sealed child scope and the signed generation
//! binding are still compared against it, so a mismatch fails the start.

use super::WorkerManifest;
use crate::config::Config;
use crate::context::call::LevelReason;
use crate::security::AutonomyLevel;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Version of the call context a worker manifest carries. A parent and a
/// worker that disagree on it refuse to start with a version error.
pub const WORKER_CALL_CONTEXT_VERSION: u32 = 1;

/// The tool surface the parent published for the worker. The worker publishes
/// exactly this; it does not read the tool tiering from its configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerPublishedSurface {
    /// `None` publishes every member the sealed scope allows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<BTreeSet<String>>,
    pub excluded_names: BTreeSet<String>,
    pub direct_names: BTreeSet<String>,
}

/// Identity, authority, place, model and published surface of a worker's calls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerCallContext {
    pub version: u32,
    pub owner_id: String,
    pub session_key: String,
    /// The named agent running the worker; `None` for Main.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    pub level: AutonomyLevel,
    pub level_reason: LevelReason,
    /// The authenticated parent workspace; tools and policy are rooted here.
    pub workspace_root: PathBuf,
    /// The directory tools run in: the parent's working root, else the workspace.
    pub working_dir: PathBuf,
    pub provider: String,
    pub model: String,
    pub surface: WorkerPublishedSurface,
}

/// What the parent knows when it seals a worker.
pub struct WorkerCallInputs<'a> {
    pub owner_id: &'a str,
    pub session_key: &'a str,
    pub agent_id: Option<&'a str>,
    pub level: AutonomyLevel,
    pub workspace_root: &'a str,
    pub working_root: Option<&'a Path>,
    pub provider: &'a str,
    pub model: &'a str,
    /// The parent's effective configuration generation.
    pub config: &'a Config,
    pub allowed_tools: &'a [String],
    pub channel_excluded_tools: &'a [String],
    pub direct_tools: Option<&'a [String]>,
}

impl WorkerCallContext {
    /// Build the call context a worker is sealed with. The surface keeps the
    /// rules the worker used to apply itself: a wildcard or empty allowance
    /// publishes every sealed member; below Full the configured and inherited
    /// exclusions hide names, at Full none; the direct names are the sealed
    /// chat-lineage set, else `always_include` plus the publish-all member.
    pub fn for_worker(inputs: &WorkerCallInputs<'_>) -> Self {
        let tiering = &inputs.config.tool_tiering;
        let allowed_tools = if inputs.allowed_tools.is_empty() || inputs.allowed_tools.iter().any(|name| name == "*") {
            None
        } else {
            Some(inputs.allowed_tools.iter().cloned().collect())
        };
        let excluded_names = if inputs.level == AutonomyLevel::Full {
            BTreeSet::new()
        } else {
            tiering
                .always_exclude
                .iter()
                .chain(inputs.channel_excluded_tools)
                .cloned()
                .collect()
        };
        let direct_names = inputs.direct_tools.map_or_else(
            || {
                let publish_all = tiering
                    .publishes_all_tools()
                    .then_some(crate::tools::capability::catalog::ALL_TOOLS_DIRECT);
                tiering
                    .always_include
                    .iter()
                    .map(String::as_str)
                    .chain(publish_all)
                    .map(str::to_owned)
                    .collect()
            },
            |names| names.iter().cloned().collect(),
        );
        Self {
            version: WORKER_CALL_CONTEXT_VERSION,
            owner_id: inputs.owner_id.to_owned(),
            session_key: inputs.session_key.to_owned(),
            agent_id: inputs.agent_id.map(str::to_owned),
            level: inputs.level,
            level_reason: LevelReason::ChildScope,
            workspace_root: PathBuf::from(inputs.workspace_root),
            working_dir: inputs
                .working_root
                .map_or_else(|| PathBuf::from(inputs.workspace_root), Path::to_path_buf),
            provider: inputs.provider.to_owned(),
            model: inputs.model.to_owned(),
            surface: WorkerPublishedSurface {
                allowed_tools,
                excluded_names,
                direct_names,
            },
        }
    }

    /// The surface policy the worker publishes with: the parent's decision.
    pub(crate) fn surface_policy(&self) -> crate::tools::capability::catalog::SurfacePolicy {
        crate::tools::capability::catalog::SurfacePolicy {
            allowed_tools: self.surface.allowed_tools.clone(),
            excluded_names: self.surface.excluded_names.clone(),
            direct_names: self.surface.direct_names.clone(),
            channel_allowed: false,
        }
    }
}

/// The call context of a worker manifest, or the protocol-version error of a
/// pair whose parent does not seal one (or seals another version).
pub fn require_call_context(manifest: &WorkerManifest) -> anyhow::Result<&WorkerCallContext> {
    let call = manifest.call_context.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "session-worker protocol version mismatch: the manifest carries no call context (this worker requires \
             call context version {WORKER_CALL_CONTEXT_VERSION}); parent and worker must be the same build"
        )
    })?;
    anyhow::ensure!(
        call.version == WORKER_CALL_CONTEXT_VERSION,
        "session-worker protocol version mismatch: call context version {} (this worker requires {}); parent and \
         worker must be the same build",
        call.version,
        WORKER_CALL_CONTEXT_VERSION
    );
    Ok(call)
}
