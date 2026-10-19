//! `[skills_lifecycle]`: operator configuration of the skill lifecycle (SL09).
//!
//! The section is operator-owned. Nothing a model or a tool produces can edit
//! it, and it is validated when the configuration loads: a policy weaker than
//! the lifecycle's floor is refused with a typed error rather than weakened
//! silently. [`SkillsLifecycleConfig::to_settings`] maps the validated section
//! to the [`SkillsLifecycleSettings`] the lifecycle service runs under.

use crate::skills::lifecycle::{
    LifecycleLimits, LifecycleMode, MIN_PUBLICATION_CASES_FLOOR, OwnerScope, ServiceError, SkillId,
    SkillsLifecycleSettings, SuiteRegistry, default_gate_policy,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use thiserror::Error;

/// The only store location the lifecycle store supports: under the owner
/// workspace, where `skill_read` and recovery look for it.
pub const SUPPORTED_STORAGE_DIR: &str = "self/skill-lifecycle";

/// Largest `min_cases_floor` accepted (a suite holds at most 128 cases).
const MAX_MIN_CASES_FLOOR: u32 = 128;
const MAX_CONCURRENT_EVALUATIONS: u32 = 8;
const MAX_PROPOSALS_PER_CYCLE: u32 = 32;
const MAX_EDIT_TOKENS: u64 = 100_000;

/// What the lifecycle may do. Mirrors `LifecycleMode`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillsLifecycleModeConfig {
    /// Drafts and reviews only.
    #[default]
    DraftOnly,
    /// Drafts plus evaluation and gating; never publishes.
    Shadow,
    /// Publishes an eligible proposal after every gate.
    Auto,
}

impl From<SkillsLifecycleModeConfig> for LifecycleMode {
    fn from(mode: SkillsLifecycleModeConfig) -> Self {
        match mode {
            SkillsLifecycleModeConfig::DraftOnly => Self::DraftOnly,
            SkillsLifecycleModeConfig::Shadow => Self::Shadow,
            SkillsLifecycleModeConfig::Auto => Self::Auto,
        }
    }
}

/// The frozen suite that governs one skill's proposals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SkillSuiteRegistration {
    /// The skill the suite governs.
    pub skill_id: String,
    /// SHA-256 digest (64 lowercase hex digits) of the frozen suite.
    pub suite_digest: String,
}

/// Why `[skills_lifecycle]` is refused.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SkillsLifecycleConfigError {
    #[error("skills_lifecycle.owner_scope is required when the lifecycle is enabled")]
    OwnerScopeMissing,
    #[error("skills_lifecycle.owner_scope is invalid: {0}")]
    OwnerScopeInvalid(String),
    #[error("skills_lifecycle.storage_dir must be \"self/skill-lifecycle\" (the store location is fixed), got \"{0}\"")]
    UnsupportedStorageDir(String),
    #[error("skills_lifecycle.require_holdout = false is refused: publication needs a holdout split")]
    HoldoutRequired,
    #[error("skills_lifecycle.min_cases_floor = {0} is refused: it must be 3..=128")]
    CasesFloorOutOfRange(u32),
    #[error("skills_lifecycle.allow_executable_proposals = true is refused: executable bundles are not supported yet")]
    ExecutableProposalsUnsupported,
    #[error("skills_lifecycle.{field} is out of range: {detail}")]
    LimitOutOfRange { field: &'static str, detail: String },
    #[error("skills_lifecycle.suites names skill \"{0}\" twice")]
    DuplicateSuite(String),
    #[error("skills_lifecycle.suites entry \"{skill}\" is invalid: {reason}")]
    InvalidSuite { skill: String, reason: String },
    #[error("skills_lifecycle settings are invalid: {0}")]
    Settings(String),
}

/// `[skills_lifecycle]`. The defaults leave the lifecycle off, in `draft_only`
/// mode, with the strongest publication policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SkillsLifecycleConfig {
    /// Turns the lifecycle on. Off by default: nothing is offered and no store
    /// is opened.
    #[serde(default)]
    pub enabled: bool,
    /// `draft_only`, `shadow` or `auto`.
    #[serde(default)]
    pub mode: SkillsLifecycleModeConfig,
    /// The owner scope whose lifecycle store this installation manages.
    /// Required when `enabled`.
    #[serde(default)]
    pub owner_scope: Option<String>,
    /// Store location under the workspace. Only the default is supported.
    #[serde(default = "default_storage_dir")]
    pub storage_dir: String,
    /// A governing suite must have a holdout split. `false` is refused.
    #[serde(default = "default_true")]
    pub require_holdout: bool,
    /// Smallest `min_cases` a governing suite may declare. Below 3 is refused.
    #[serde(default = "default_min_cases_floor")]
    pub min_cases_floor: u32,
    /// Most evaluations running at once.
    #[serde(default = "default_one")]
    pub max_concurrent_evaluations: u32,
    /// Most proposals one cycle may start.
    #[serde(default = "default_one")]
    pub max_proposals_per_cycle: u32,
    /// Edit budget of a proposal, in tokens.
    #[serde(default = "default_max_edit_tokens")]
    pub max_edit_tokens: u64,
    /// Edit budget of a proposal as a fraction of the skill body, in (0, 1].
    #[serde(default = "default_max_edit_fraction")]
    pub max_edit_fraction: f64,
    /// Allow proposals that touch executable files. `true` is refused until
    /// reviewed executable bundles exist.
    #[serde(default)]
    pub allow_executable_proposals: bool,
    /// The frozen suite per skill. Operator-owned: it is the only source of
    /// the suite that governs a proposal.
    #[serde(default)]
    pub suites: Vec<SkillSuiteRegistration>,
}

fn default_storage_dir() -> String {
    SUPPORTED_STORAGE_DIR.to_string()
}

const fn default_true() -> bool {
    true
}

const fn default_min_cases_floor() -> u32 {
    MIN_PUBLICATION_CASES_FLOOR
}

const fn default_one() -> u32 {
    1
}

const fn default_max_edit_tokens() -> u64 {
    300
}

const fn default_max_edit_fraction() -> f64 {
    0.20
}

impl Default for SkillsLifecycleConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: SkillsLifecycleModeConfig::DraftOnly,
            owner_scope: None,
            storage_dir: default_storage_dir(),
            require_holdout: true,
            min_cases_floor: default_min_cases_floor(),
            max_concurrent_evaluations: 1,
            max_proposals_per_cycle: 1,
            max_edit_tokens: default_max_edit_tokens(),
            max_edit_fraction: default_max_edit_fraction(),
            allow_executable_proposals: false,
            suites: Vec::new(),
        }
    }
}

impl SkillsLifecycleConfig {
    /// Checks the section; the first problem found is returned.
    pub fn validate(&self) -> Result<(), SkillsLifecycleConfigError> {
        self.to_settings().map(|_| ())
    }

    /// The owner scope the store belongs to, `None` while disabled and unset.
    pub fn owner(&self) -> Result<Option<OwnerScope>, SkillsLifecycleConfigError> {
        match self.owner_scope.as_deref() {
            Some(owner) => OwnerScope::new(owner)
                .map(Some)
                .map_err(|e| SkillsLifecycleConfigError::OwnerScopeInvalid(e.to_string())),
            None if self.enabled => Err(SkillsLifecycleConfigError::OwnerScopeMissing),
            None => Ok(None),
        }
    }

    /// Validates the section and maps it to the settings the service runs
    /// under. A policy weaker than the lifecycle's floor is an error.
    pub fn to_settings(&self) -> Result<SkillsLifecycleSettings, SkillsLifecycleConfigError> {
        use SkillsLifecycleConfigError as E;
        self.owner()?;
        if self.storage_dir.trim().trim_end_matches('/') != SUPPORTED_STORAGE_DIR {
            return Err(E::UnsupportedStorageDir(self.storage_dir.clone()));
        }
        if !self.require_holdout {
            return Err(E::HoldoutRequired);
        }
        if !(MIN_PUBLICATION_CASES_FLOOR..=MAX_MIN_CASES_FLOOR).contains(&self.min_cases_floor) {
            return Err(E::CasesFloorOutOfRange(self.min_cases_floor));
        }
        if self.allow_executable_proposals {
            return Err(E::ExecutableProposalsUnsupported);
        }
        let out_of_range = |field: &'static str, detail: String| E::LimitOutOfRange { field, detail };
        if !(1..=MAX_CONCURRENT_EVALUATIONS).contains(&self.max_concurrent_evaluations) {
            return Err(out_of_range(
                "max_concurrent_evaluations",
                format!("must be 1..={MAX_CONCURRENT_EVALUATIONS}"),
            ));
        }
        if !(1..=MAX_PROPOSALS_PER_CYCLE).contains(&self.max_proposals_per_cycle) {
            return Err(out_of_range(
                "max_proposals_per_cycle",
                format!("must be 1..={MAX_PROPOSALS_PER_CYCLE}"),
            ));
        }
        if !(1..=MAX_EDIT_TOKENS).contains(&self.max_edit_tokens) {
            return Err(out_of_range(
                "max_edit_tokens",
                format!("must be 1..={MAX_EDIT_TOKENS}"),
            ));
        }
        if !(self.max_edit_fraction.is_finite() && self.max_edit_fraction > 0.0 && self.max_edit_fraction <= 1.0) {
            return Err(out_of_range(
                "max_edit_fraction",
                "must be greater than 0 and at most 1".to_string(),
            ));
        }

        let mut seen = BTreeSet::new();
        let mut entries = Vec::with_capacity(self.suites.len());
        for registration in &self.suites {
            let skill = SkillId::parse(&registration.skill_id).map_err(|e| E::InvalidSuite {
                skill: registration.skill_id.clone(),
                reason: e.to_string(),
            })?;
            if !seen.insert(skill.clone()) {
                return Err(E::DuplicateSuite(registration.skill_id.clone()));
            }
            entries.push((skill, registration.suite_digest.clone()));
        }
        let suites = SuiteRegistry::new(entries).map_err(|e| match e {
            ServiceError::Settings(reason) => E::InvalidSuite {
                skill: String::new(),
                reason,
            },
            other => E::Settings(other.to_string()),
        })?;

        let mut gate_policy = default_gate_policy();
        gate_policy.require_holdout = self.require_holdout;
        gate_policy.min_cases_floor = self.min_cases_floor;
        let settings = SkillsLifecycleSettings {
            enabled: self.enabled,
            mode: self.mode.into(),
            gate_policy,
            suites,
            limits: LifecycleLimits {
                max_concurrent_evaluations: self.max_concurrent_evaluations,
                max_proposals_per_cycle: self.max_proposals_per_cycle,
                max_edit_tokens: self.max_edit_tokens,
                max_edit_fraction: self.max_edit_fraction,
            },
        };
        settings.validate().map_err(|e| E::Settings(e.to_string()))?;
        Ok(settings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    const HEADER: &str = "workspace_dir = \"/tmp/ws\"\nconfig_path = \"/tmp/config.toml\"\ndefault_temperature = 0.7\n";

    fn digest() -> String {
        "ab".repeat(32)
    }

    fn enabled() -> SkillsLifecycleConfig {
        SkillsLifecycleConfig {
            enabled: true,
            owner_scope: Some("owner:ws:cli:alice".to_string()),
            ..SkillsLifecycleConfig::default()
        }
    }

    #[test]
    fn defaults_are_disabled_draft_only_with_the_strong_policy() {
        let config = SkillsLifecycleConfig::default();
        let settings = config.to_settings().expect("test: defaults are valid");
        assert!(!settings.enabled);
        assert_eq!(settings.mode, LifecycleMode::DraftOnly);
        assert!(settings.gate_policy.require_holdout);
        assert_eq!(settings.gate_policy.min_cases_floor, MIN_PUBLICATION_CASES_FLOOR);
        assert!(settings.suites.is_empty());
        assert_eq!(settings.limits, LifecycleLimits::default());
        assert_eq!(config.storage_dir, SUPPORTED_STORAGE_DIR);
        assert!(!config.allow_executable_proposals);
        assert!(config.owner().expect("test: no owner needed while disabled").is_none());
    }

    #[test]
    fn a_config_without_the_section_loads_as_the_defaults() {
        let config: Config = toml::from_str(HEADER).expect("test: parses");
        assert_eq!(config.skills_lifecycle, SkillsLifecycleConfig::default());
        config.validate().expect("test: a default config validates");
    }

    #[test]
    fn the_section_parses_and_maps_to_settings() {
        let text = format!(
            "{HEADER}[skills_lifecycle]\nenabled = true\nmode = \"auto\"\nowner_scope = \"owner:ws:cli:alice\"\nmin_cases_floor = 5\nmax_edit_tokens = 200\n[[skills_lifecycle.suites]]\nskill_id = \"release-notes\"\nsuite_digest = \"{}\"\n",
            digest()
        );
        let config: Config = toml::from_str(&text).expect("test: parses");
        config.validate().expect("test: validates");
        let settings = config.skills_lifecycle.to_settings().expect("test: maps");
        assert!(settings.enabled);
        assert_eq!(settings.mode, LifecycleMode::Auto);
        assert_eq!(settings.gate_policy.min_cases_floor, 5);
        assert_eq!(settings.limits.max_edit_tokens, 200);
        let skill = SkillId::parse("release-notes").expect("test: skill id");
        assert_eq!(settings.suites.suite_for(&skill), Some(digest().as_str()));
        assert_eq!(settings.publication_block(), None);
    }

    #[test]
    fn a_weaker_policy_is_refused_at_load() {
        let mut config = enabled();
        config.require_holdout = false;
        assert_eq!(config.validate(), Err(SkillsLifecycleConfigError::HoldoutRequired));
        for floor in [0, 1, 2] {
            let mut config = enabled();
            config.min_cases_floor = floor;
            assert_eq!(
                config.validate(),
                Err(SkillsLifecycleConfigError::CasesFloorOutOfRange(floor))
            );
        }
        let mut config = enabled();
        config.min_cases_floor = 129;
        assert!(config.validate().is_err());

        let text = format!("{HEADER}[skills_lifecycle]\nrequire_holdout = false\n");
        let loaded: Config = toml::from_str(&text).expect("test: parses");
        let error = loaded.validate().expect_err("test: a weak policy must not load");
        assert!(format!("{error:#}").contains("holdout"), "{error:#}");
    }

    #[test]
    fn refuses_what_the_lifecycle_cannot_honour() {
        let mut config = enabled();
        config.allow_executable_proposals = true;
        assert_eq!(
            config.validate(),
            Err(SkillsLifecycleConfigError::ExecutableProposalsUnsupported)
        );
        let mut config = enabled();
        config.storage_dir = "elsewhere/store".to_string();
        assert!(matches!(
            config.validate(),
            Err(SkillsLifecycleConfigError::UnsupportedStorageDir(_))
        ));
        let mut config = enabled();
        config.storage_dir = "../self/skill-lifecycle".to_string();
        assert!(config.validate().is_err());
        assert_eq!(
            SkillsLifecycleConfig {
                enabled: true,
                ..SkillsLifecycleConfig::default()
            }
            .validate(),
            Err(SkillsLifecycleConfigError::OwnerScopeMissing)
        );
        let mut config = enabled();
        config.owner_scope = Some(" padded ".to_string());
        assert!(matches!(
            config.validate(),
            Err(SkillsLifecycleConfigError::OwnerScopeInvalid(_))
        ));
    }

    #[test]
    fn limits_and_suites_are_validated() {
        for mutate in [
            (|c: &mut SkillsLifecycleConfig| c.max_concurrent_evaluations = 0) as fn(&mut SkillsLifecycleConfig),
            |c| c.max_proposals_per_cycle = 0,
            |c| c.max_proposals_per_cycle = 33,
            |c| c.max_edit_tokens = 0,
            |c| c.max_edit_fraction = 0.0,
            |c| c.max_edit_fraction = 1.5,
            |c| c.max_edit_fraction = f64::NAN,
        ] {
            let mut config = enabled();
            mutate(&mut config);
            assert!(
                matches!(
                    config.validate(),
                    Err(SkillsLifecycleConfigError::LimitOutOfRange { .. })
                ),
                "{config:?}"
            );
        }
        let registration = |skill: &str, digest: String| SkillSuiteRegistration {
            skill_id: skill.to_string(),
            suite_digest: digest,
        };
        let mut config = enabled();
        config.suites = vec![registration("demo", digest()), registration("demo", digest())];
        assert!(matches!(
            config.validate(),
            Err(SkillsLifecycleConfigError::DuplicateSuite(_))
        ));
        config.suites = vec![registration("demo", "NOT-A-DIGEST".to_string())];
        assert!(matches!(
            config.validate(),
            Err(SkillsLifecycleConfigError::InvalidSuite { .. })
        ));
        config.suites = vec![registration("Bad Skill", digest())];
        assert!(matches!(
            config.validate(),
            Err(SkillsLifecycleConfigError::InvalidSuite { .. })
        ));
    }

    #[test]
    fn unknown_keys_in_the_section_are_refused() {
        let text = format!("{HEADER}[skills_lifecycle]\nrequire_holduot = false\n");
        assert!(toml::from_str::<Config>(&text).is_err());
    }
}
