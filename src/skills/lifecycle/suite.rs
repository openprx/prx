//! Frozen evaluation suites (SL06).
//!
//! A suite is the exact, closed list of cases a candidate revision is judged
//! on: prompts, environment fixtures, deterministic validators, the
//! trigger expectation, budgets and the attempt limits of the reserved splits.
//! Freezing validates it and computes a content digest; the suite is stored
//! under `suites/<digest>.json` and every later receipt binds to that digest,
//! so editing, filtering or renaming a case yields a different suite.
//!
//! The validators (the oracle) live only in the suite record. They are never
//! part of what an executor is handed, so a task cannot read its own answers.

use super::evaluate::EvaluationError;
use super::generation::ensure_dir_synced;
use super::store::{SkillLifecycleStore, check_private, read_record, record_bytes};
use super::types::{OwnerScope, SkillId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

/// Schema version of suite records.
pub const SUITE_SCHEMA_VERSION: u32 = 1;
/// Hard limits that keep one suite inside the store's record size.
pub const MAX_SUITE_CASES: usize = 128;
pub const MAX_CASE_PROMPT_BYTES: usize = 4096;
pub const MAX_CASE_VALIDATORS: usize = 12;
pub const MAX_CASE_FIXTURES: usize = 8;
pub const MAX_FIXTURE_BYTES: usize = 4096;
pub const MAX_VALIDATOR_TEXT_BYTES: usize = 1024;
pub const MAX_TRIALS: u32 = 5;
pub const MAX_SUITE_RUNS: u32 = 1024;
/// Smallest sample a split must declare (spec 11.3: small noisy samples are
/// inconclusive, so a suite may not declare a smaller minimum).
pub const MIN_SAMPLE_CASES: u32 = 2;
pub const MAX_ALLOWED_REGRESSIONS: u32 = 3;
pub const MAX_VALIDATION_ATTEMPTS: u32 = 10;
pub const MAX_HOLDOUT_ATTEMPTS: u32 = 3;
pub const MAX_REQUIRED_TOOLS: usize = 16;
/// The only tools a suite may declare besides `skill_read`. Everything that
/// can run commands, install or execute skills, spawn work, schedule, reach
/// MCP or plugins, or post arbitrary requests is never available to an
/// evaluation run. The network reads are named explicitly so a suite that
/// needs them says so. Both of them send data off the machine: `web_search_tool`
/// sends the model's queries to the configured search provider, and `web_fetch`
/// requests any host the template's `browser.allowed_domains` permits. A prompt
/// the model writes into such a request can carry task content, so neither is
/// allowed in a suite that has a holdout split, and `web_fetch` additionally
/// needs a non-empty `browser.allowed_domains` in the template.
pub const READ_ONLY_REQUIRED_TOOLS: [&str; 9] = [
    "file_read",
    "skills_list",
    "memory_search",
    "memory_get",
    "memory_recall",
    "document_search",
    "document_get_chunk",
    "web_search_tool",
    "web_fetch",
];
/// Tools that reach the network.
pub(super) const NETWORK_TOOLS: [&str; 2] = ["web_search_tool", "web_fetch"];
/// Additionally allowed when the suite declares workspace writes.
pub const WRITE_REQUIRED_TOOLS: [&str; 2] = ["file_write", "file_edit"];
/// Workspace paths the evaluator owns; a fixture may not occupy them.
const RESERVED_FIXTURE_ROOTS: [&str; 10] = [
    "skills",
    "self",
    "state",
    "memory",
    "plugins",
    ".openprx",
    "config.toml",
    "hooks.json",
    "no-open-skills",
    "no-openclaw-skills",
];
const MAX_IDENT_BYTES: usize = 64;
const MAX_PATH_BYTES: usize = 128;
const MAX_SUITE_RECORD_BYTES: usize = 200 * 1024;
const MIN_CASE_ELAPSED_MS: u64 = 100;
const MAX_CASE_ELAPSED_MS: u64 = 600_000;
const SUITE_DOMAIN: &[u8] = b"prx-skill-suite\0";
pub(super) const SUITES_DIR: &str = "suites";

/// Which part of the task population a case belongs to. The proposer sees
/// development evidence only; validation attempts are limited and the final
/// holdout is reserved for a single confirming run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalSplit {
    Development,
    Validation,
    Holdout,
}

/// A deterministic check of a finished run. Expectations are literal data
/// held in the suite, never given to the executor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Validator {
    ResponseContains {
        text: String,
    },
    ResponseExcludes {
        text: String,
    },
    /// Hex SHA-256 of the exact response; leaves no expected text in the suite.
    ResponseSha256 {
        sha256: String,
    },
    ArtifactContains {
        path: String,
        text: String,
    },
    ArtifactSha256 {
        path: String,
        sha256: String,
    },
    ArtifactAbsent {
        path: String,
    },
}

impl Validator {
    /// Short stable name used in receipts and failure categories.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::ResponseContains { .. } => "response_contains",
            Self::ResponseExcludes { .. } => "response_excludes",
            Self::ResponseSha256 { .. } => "response_sha256",
            Self::ArtifactContains { .. } => "artifact_contains",
            Self::ArtifactSha256 { .. } => "artifact_sha256",
            Self::ArtifactAbsent { .. } => "artifact_absent",
        }
    }

    /// The artifact path this validator inspects, when it inspects one.
    #[must_use]
    pub const fn artifact_path(&self) -> Option<&str> {
        match self {
            Self::ArtifactContains { path, .. } | Self::ArtifactSha256 { path, .. } | Self::ArtifactAbsent { path } => {
                Some(path.as_str())
            }
            _ => None,
        }
    }
}

/// A text file placed in the fresh task workspace before the run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureFile {
    pub path: String,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalCase {
    pub case_id: String,
    pub split: EvalSplit,
    pub task_family: String,
    pub prompt: String,
    pub fixtures: Vec<FixtureFile>,
    pub validators: Vec<Validator>,
    /// Whether the skill must be loaded for this task. `false` is a negative
    /// trigger case: loading the skill there is a failure.
    pub expect_skill_read: bool,
    /// A critical case that fails or regresses blocks any improvement.
    pub critical: bool,
    pub max_elapsed_ms: u64,
}

impl SuiteBudget {
    /// What an unmeasured run is charged: its even share of the token ceiling.
    #[must_use]
    pub fn unmeasured_tokens(&self) -> u64 {
        (self.max_total_tokens / u64::from(self.max_runs.max(1))).max(1)
    }

    /// What an unmeasured run is charged: its even share of the cost ceiling.
    #[must_use]
    pub fn unmeasured_cost_micros(&self) -> u64 {
        (self.max_total_cost_micros / u64::from(self.max_runs.max(1))).max(1)
    }
}

/// What a task may change in its fresh workspace. Nothing outside the task
/// workspace is ever writable, whatever is declared.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideEffects {
    /// Read-only tools; the workspace cannot be modified.
    None,
    /// Files below the task workspace may be written.
    WorkspaceWrite,
}

/// Ceilings on one evaluation. Every ceiling is mandatory (spec 14): a suite
/// without a budget is a configuration error. Tokens and money are measured
/// from the run's own cost ledger; a run whose usage cannot be measured is
/// invalid, never free.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuiteBudget {
    pub max_runs: u32,
    pub max_total_elapsed_ms: u64,
    pub max_total_tokens: u64,
    /// Millionths of a US dollar, as the cost ledger prices it.
    pub max_total_cost_micros: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalSuite {
    pub schema_version: u32,
    pub suite_id: String,
    pub owner_scope: OwnerScope,
    pub skill_id: SkillId,
    /// Repetitions of every case per arm, chosen in advance. A case passes in
    /// an arm only when every trial passes; the best trial is never selected.
    pub trials: u32,
    /// Smallest number of cases a split must have to be evaluated.
    pub min_cases: u32,
    /// Non-critical regressions tolerated next to an improvement (default 0).
    pub allowed_regressions: u32,
    /// Comparison rule: exact paired case outcomes, which is only sound for a
    /// deterministic suite. A stochastic suite needs a documented interval
    /// rule first, so `false` is refused.
    pub deterministic: bool,
    /// Prespecified minimum practical gain: the candidate must pass at least
    /// this many more cases than the baseline. A tie is never a gain.
    pub min_gain_cases: u32,
    /// Evaluations allowed on the validation and holdout splits.
    pub validation_attempts: u32,
    pub holdout_attempts: u32,
    /// Tools offered to the task besides `skill_read`. Nothing else is.
    pub required_tools: Vec<String>,
    pub side_effects: SideEffects,
    pub budget: SuiteBudget,
    pub cases: Vec<EvalCase>,
}

/// A validated suite together with its content digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrozenSuite {
    suite: EvalSuite,
    digest: String,
}

impl FrozenSuite {
    #[must_use]
    pub const fn suite(&self) -> &EvalSuite {
        &self.suite
    }

    /// Lowercase hex SHA-256 over the canonical suite encoding.
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Cases of one split in canonical (case id) order.
    pub fn cases_of(&self, split: EvalSplit) -> impl Iterator<Item = &EvalCase> {
        self.suite.cases.iter().filter(move |case| case.split == split)
    }

    #[must_use]
    pub fn case(&self, case_id: &str) -> Option<&EvalCase> {
        self.suite.cases.iter().find(|case| case.case_id == case_id)
    }

    /// Digest of the held-out tasks of one split: the skill and the sorted,
    /// normalized prompts. Attempt limits key on it, so renaming the suite or a
    /// case, editing other splits, or changing validators does not reopen a
    /// reserved split; only different tasks do.
    #[must_use]
    pub fn split_digest(&self, split: EvalSplit) -> String {
        let mut prompts: Vec<String> = self
            .cases_of(split)
            .map(|case| normalized_prompt(&case.prompt))
            .collect();
        prompts.sort();
        prompts.dedup();
        let mut hasher = Sha256::new();
        hasher.update(b"prx-skill-split-v2\0");
        hasher.update(self.suite.skill_id.as_str().as_bytes());
        hasher.update([0]);
        hasher.update(format!("{split:?}").as_bytes());
        for prompt in prompts {
            hasher.update(u64::try_from(prompt.len()).unwrap_or(u64::MAX).to_be_bytes());
            hasher.update(prompt.as_bytes());
        }
        hex::encode(hasher.finalize())
    }

    /// Case-ID/split-independent task and fixture-source identities for
    /// adopted-guidance exclusion. No validator/oracle/body is retained in the
    /// returned hashes, and fixture renaming cannot hide identical source.
    /// This exact normalization is not a semantic-equivalence detector.
    #[must_use]
    pub(crate) fn lesson_root_digests(&self) -> Vec<String> {
        let mut roots = BTreeSet::new();
        for case in &self.suite.cases {
            let prompt = normalized_prompt(&case.prompt);
            let mut task = Sha256::new();
            task.update(b"prx-lesson-task-root-v1\0");
            task.update(prompt.as_bytes());
            roots.insert(hex::encode(task.finalize()));
            for fixture in &case.fixtures {
                let mut source = Sha256::new();
                source.update(b"prx-lesson-task-fixture-source-root-v1\0");
                // A common/empty fixture is not an identity shared by every
                // real task. Bind its source bytes to the actual task content.
                source.update(u64::try_from(prompt.len()).unwrap_or(u64::MAX).to_be_bytes());
                source.update(prompt.as_bytes());
                source.update(u64::try_from(fixture.content.len()).unwrap_or(u64::MAX).to_be_bytes());
                source.update(fixture.content.as_bytes());
                roots.insert(hex::encode(source.finalize()));
            }
        }
        roots.into_iter().collect()
    }

    #[must_use]
    pub const fn attempt_limit(&self, split: EvalSplit) -> Option<u32> {
        match split {
            EvalSplit::Development => None,
            EvalSplit::Validation => Some(self.suite.validation_attempts),
            EvalSplit::Holdout => Some(self.suite.holdout_attempts),
        }
    }
}

fn invalid(reason: impl Into<String>) -> EvaluationError {
    EvaluationError::InvalidSuite(reason.into())
}

fn suite_digest(suite: &EvalSuite) -> Result<String, EvaluationError> {
    let bytes = serde_json::to_vec(suite).map_err(|e| invalid(e.to_string()))?;
    let mut hasher = Sha256::new();
    hasher.update(SUITE_DOMAIN);
    hasher.update(SUITE_SCHEMA_VERSION.to_be_bytes());
    hasher.update(bytes);
    Ok(hex::encode(hasher.finalize()))
}

fn is_hex_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn check_ident(what: &str, value: &str) -> Result<(), EvaluationError> {
    let ok = !value.is_empty()
        && value.len() <= MAX_IDENT_BYTES
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        && !value.starts_with('.');
    if ok {
        Ok(())
    } else {
        Err(invalid(format!(
            "{what} must be 1..={MAX_IDENT_BYTES} ASCII letters, digits, '-', '_' or '.', not starting with '.'"
        )))
    }
}

fn check_text(what: &str, value: &str, max: usize) -> Result<(), EvaluationError> {
    if value.trim().is_empty() || value.len() > max || value.chars().any(|c| c.is_control() && c != '\n' && c != '\t') {
        return Err(invalid(format!(
            "{what} must be 1..={max} bytes without control characters"
        )));
    }
    Ok(())
}

/// A workspace-relative path: no traversal, no absolute or drive forms, no
/// empty or dot components, no backslashes.
pub(super) fn check_relative_path(what: &str, path: &str) -> Result<(), EvaluationError> {
    let bad = path.is_empty()
        || path.len() > MAX_PATH_BYTES
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains(':')
        || path.chars().any(char::is_control)
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..");
    if bad {
        return Err(invalid(format!("{what} {path:?} must be a safe relative path")));
    }
    Ok(())
}

fn normalized_prompt(prompt: &str) -> String {
    prompt.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

fn validate_validator(validator: &Validator) -> Result<(), EvaluationError> {
    match validator {
        Validator::ResponseContains { text } | Validator::ResponseExcludes { text } => {
            check_text("validator text", text, MAX_VALIDATOR_TEXT_BYTES)
        }
        Validator::ResponseSha256 { sha256 } => {
            if is_hex_digest(sha256) {
                Ok(())
            } else {
                Err(invalid("validator sha256 must be 64 lowercase hex digits"))
            }
        }
        Validator::ArtifactContains { path, text } => {
            check_relative_path("artifact path", path)?;
            check_text("validator text", text, MAX_VALIDATOR_TEXT_BYTES)
        }
        Validator::ArtifactSha256 { path, sha256 } => {
            check_relative_path("artifact path", path)?;
            if is_hex_digest(sha256) {
                Ok(())
            } else {
                Err(invalid("validator sha256 must be 64 lowercase hex digits"))
            }
        }
        Validator::ArtifactAbsent { path } => check_relative_path("artifact path", path),
    }
}

fn validate_case(case: &EvalCase) -> Result<(), EvaluationError> {
    check_ident("case id", &case.case_id)?;
    check_ident("task family", &case.task_family)?;
    check_text("prompt", &case.prompt, MAX_CASE_PROMPT_BYTES)?;
    if case.validators.is_empty() || case.validators.len() > MAX_CASE_VALIDATORS {
        return Err(invalid(format!(
            "case {} needs 1..={MAX_CASE_VALIDATORS} validators",
            case.case_id
        )));
    }
    case.validators.iter().try_for_each(validate_validator)?;
    if case.fixtures.len() > MAX_CASE_FIXTURES {
        return Err(invalid(format!(
            "case {} has more than {MAX_CASE_FIXTURES} fixtures",
            case.case_id
        )));
    }
    let mut paths = BTreeSet::new();
    for fixture in &case.fixtures {
        check_relative_path("fixture path", &fixture.path)?;
        let root = fixture.path.split('/').next().unwrap_or_default().to_ascii_lowercase();
        if RESERVED_FIXTURE_ROOTS.contains(&root.as_str()) {
            return Err(invalid(format!(
                "fixture path {} is reserved for the evaluator",
                fixture.path
            )));
        }
        if fixture.content.len() > MAX_FIXTURE_BYTES {
            return Err(invalid(format!("fixture {} is too large", fixture.path)));
        }
        if !paths.insert(fixture.path.as_str()) {
            return Err(invalid(format!("duplicate fixture path {}", fixture.path)));
        }
    }
    if !(MIN_CASE_ELAPSED_MS..=MAX_CASE_ELAPSED_MS).contains(&case.max_elapsed_ms) {
        return Err(invalid(format!(
            "case {} elapsed ceiling must be {MIN_CASE_ELAPSED_MS}..={MAX_CASE_ELAPSED_MS} ms",
            case.case_id
        )));
    }
    Ok(())
}

impl EvalSuite {
    /// Validates the suite, puts its cases in canonical order and computes the
    /// digest. Rejected: no cases, duplicate case ids, a prompt shared by two
    /// splits (the splits must stay disjoint), unsafe paths, unbounded sizes
    /// and budgets that cannot run the suite once.
    pub fn freeze(mut self) -> Result<FrozenSuite, EvaluationError> {
        if self.schema_version != SUITE_SCHEMA_VERSION {
            return Err(invalid(format!(
                "unsupported suite schema version {}",
                self.schema_version
            )));
        }
        check_ident("suite id", &self.suite_id)?;
        if self.cases.is_empty() || self.cases.len() > MAX_SUITE_CASES {
            return Err(invalid(format!("a suite has 1..={MAX_SUITE_CASES} cases")));
        }
        if self.trials == 0 || self.trials > MAX_TRIALS {
            return Err(invalid(format!("trials must be 1..={MAX_TRIALS}")));
        }
        if self.min_cases < MIN_SAMPLE_CASES {
            return Err(invalid(format!(
                "min_cases must be at least {MIN_SAMPLE_CASES}: a suite declares its minimum sample"
            )));
        }
        if !self.deterministic {
            return Err(invalid(
                "stochastic suites are not supported: exact paired outcomes are only sound for a deterministic suite",
            ));
        }
        if self.min_gain_cases == 0 {
            return Err(invalid(
                "min_gain_cases must be at least 1: a tie is not an improvement",
            ));
        }
        if self.allowed_regressions > MAX_ALLOWED_REGRESSIONS
            || !(1..=MAX_VALIDATION_ATTEMPTS).contains(&self.validation_attempts)
            || !(1..=MAX_HOLDOUT_ATTEMPTS).contains(&self.holdout_attempts)
        {
            return Err(invalid(format!(
                "allowed_regressions must be at most {MAX_ALLOWED_REGRESSIONS}, validation_attempts 1..={MAX_VALIDATION_ATTEMPTS} and holdout_attempts 1..={MAX_HOLDOUT_ATTEMPTS}"
            )));
        }
        if self.budget.max_runs == 0
            || self.budget.max_runs > MAX_SUITE_RUNS
            || self.budget.max_total_elapsed_ms == 0
            || self.budget.max_total_tokens == 0
            || self.budget.max_total_cost_micros == 0
        {
            return Err(invalid(format!(
                "budget needs max_runs 1..={MAX_SUITE_RUNS} and positive elapsed, token and cost ceilings"
            )));
        }
        if self.required_tools.len() > MAX_REQUIRED_TOOLS {
            return Err(invalid(format!("at most {MAX_REQUIRED_TOOLS} required tools")));
        }
        self.required_tools.sort();
        self.required_tools.dedup();
        for tool in &self.required_tools {
            check_ident("required tool", tool)?;
            if tool == "skill_read" {
                return Err(invalid("skill_read is always offered; do not list it"));
            }
            if NETWORK_TOOLS.contains(&tool.as_str()) && self.cases.iter().any(|c| c.split == EvalSplit::Holdout) {
                return Err(invalid(format!(
                    "tool {tool} sends task content off the machine and cannot be used in a suite with a holdout split"
                )));
            }
            let allowed = READ_ONLY_REQUIRED_TOOLS.contains(&tool.as_str())
                || (self.side_effects == SideEffects::WorkspaceWrite && WRITE_REQUIRED_TOOLS.contains(&tool.as_str()));
            if !allowed {
                return Err(invalid(format!(
                    "tool {tool} cannot be offered to an evaluation run with side effects {:?}",
                    self.side_effects
                )));
            }
        }
        self.cases.sort_by(|a, b| a.case_id.cmp(&b.case_id));
        let mut ids = BTreeSet::new();
        let mut prompts: Vec<(String, EvalSplit)> = Vec::new();
        for case in &self.cases {
            validate_case(case)?;
            if !ids.insert(case.case_id.as_str()) {
                return Err(invalid(format!("duplicate case id {}", case.case_id)));
            }
            let normalized = normalized_prompt(&case.prompt);
            if let Some((_, other)) = prompts
                .iter()
                .find(|(p, split)| *p == normalized && *split != case.split)
            {
                return Err(invalid(format!(
                    "case {} repeats a prompt of the {other:?} split; splits must be disjoint",
                    case.case_id
                )));
            }
            prompts.push((normalized, case.split));
        }
        let smallest_run = u64::from(self.trials) * 2;
        for split in [EvalSplit::Development, EvalSplit::Validation, EvalSplit::Holdout] {
            let count = self.cases.iter().filter(|case| case.split == split).count();
            if count == 0 {
                continue;
            }
            let runs = u64::try_from(count).unwrap_or(u64::MAX).saturating_mul(smallest_run);
            if runs > u64::from(self.budget.max_runs) {
                return Err(invalid(format!(
                    "the {split:?} split needs {runs} runs, over the budget of {}",
                    self.budget.max_runs
                )));
            }
        }
        let digest = suite_digest(&self)?;
        let encoded = serde_json::to_vec(&self).map_err(|e| invalid(e.to_string()))?;
        if encoded.len() > MAX_SUITE_RECORD_BYTES {
            return Err(invalid("the suite is larger than the record size limit"));
        }
        Ok(FrozenSuite { suite: self, digest })
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SuiteRecord {
    schema_version: u32,
    digest: String,
    suite: EvalSuite,
}

impl SkillLifecycleStore {
    fn suites_dir(&self) -> PathBuf {
        self.root().join(SUITES_DIR)
    }

    /// Stores a frozen suite under its digest. Idempotent; a different suite
    /// can never take the place of an existing digest.
    pub(in crate::skills) fn write_suite(&self, suite: &FrozenSuite) -> Result<(), EvaluationError> {
        self.check_owner_scope(&suite.suite.owner_scope)?;
        let dir = self.suites_dir();
        ensure_dir_synced(self.root(), &dir)?;
        let path = dir.join(format!("{}.json", suite.digest));
        if fs::symlink_metadata(&path).is_ok() {
            let stored = self.read_suite(&suite.digest)?;
            return if stored == *suite {
                Ok(())
            } else {
                Err(EvaluationError::Lifecycle(super::types::LifecycleError::Tampered {
                    path,
                    reason: "a different suite is stored under this digest".to_string(),
                }))
            };
        }
        let record = SuiteRecord {
            schema_version: SUITE_SCHEMA_VERSION,
            digest: suite.digest.clone(),
            suite: suite.suite.clone(),
        };
        self.write_atomic(&suite.suite.skill_id, &path, &record_bytes(&record, &path)?)?;
        Ok(())
    }

    /// Reads a suite and re-derives its digest: the file name, the stored
    /// digest and the content must all agree, and the suite must belong to this
    /// owner. Anything else is tampering.
    pub fn read_suite(&self, digest: &str) -> Result<FrozenSuite, EvaluationError> {
        if !is_hex_digest(digest) {
            return Err(invalid("suite digest must be 64 lowercase hex digits"));
        }
        let path = self.suites_dir().join(format!("{digest}.json"));
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(EvaluationError::NotFound(format!("suite {digest}")));
            }
            Err(e) => return Err(super::types::LifecycleError::io(&path, e).into()),
        }
        check_private(&path)?;
        let record: SuiteRecord = read_record(&path)?;
        let tampered = |reason: &str| {
            EvaluationError::Lifecycle(super::types::LifecycleError::Tampered {
                path: path.clone(),
                reason: reason.to_string(),
            })
        };
        if record.schema_version != SUITE_SCHEMA_VERSION {
            return Err(tampered("unsupported suite schema version"));
        }
        self.check_owner_scope(&record.suite.owner_scope)?;
        let frozen = record.suite.clone().freeze().map_err(|e| tampered(&e.to_string()))?;
        if frozen.digest != digest || record.digest != digest || frozen.suite != record.suite {
            return Err(tampered("suite content does not match its digest"));
        }
        Ok(frozen)
    }
}
