//! Capability re-check at publication (spec 8.3 step 3).
//!
//! A learned revision may use what its baseline used and nothing more: no new
//! declared capability requirement, no new executable file and no new
//! `[[tools]]` entry in `SKILL.toml`. The check reads both revisions from the
//! store again at publication, so a candidate that changed after the source
//! review (or was written by another path) cannot widen what the skill may do.

use super::bundle::Bundle;
use super::proposal::{SkillProposal, bounded};
use super::store::SkillLifecycleStore;
use super::types::LifecycleError;
use std::collections::BTreeSet;

const SKILL_TOML: &str = "SKILL.toml";

fn executables(bundle: &Bundle) -> BTreeSet<&str> {
    bundle
        .files()
        .iter()
        .filter(|f| f.is_executable())
        .map(|f| f.path())
        .collect()
}

/// Tables of `[[tools]]` as canonical JSON strings; an unreadable or absent
/// `SKILL.toml` declares none, and an unparseable one is a problem of its own.
fn declared_tools(bundle: &Bundle) -> Result<BTreeSet<String>, String> {
    let Some(file) = bundle.file(SKILL_TOML) else {
        return Ok(BTreeSet::new());
    };
    let text = std::str::from_utf8(file.bytes()).map_err(|_| "SKILL.toml is not UTF-8".to_string())?;
    let value: toml::Value = toml::from_str(text).map_err(|e| format!("SKILL.toml is malformed: {e}"))?;
    let mut tools = BTreeSet::new();
    if let Some(list) = value.get("tools").and_then(toml::Value::as_array) {
        for tool in list {
            tools.insert(serde_json::to_string(tool).map_err(|e| e.to_string())?);
        }
    }
    Ok(tools)
}

impl SkillLifecycleStore {
    /// Refuses a candidate that asks for more than its baseline had, with
    /// [`LifecycleError::CapabilityExpansion`]. The candidate record is read
    /// pinned to `candidate_record_digest` when given.
    pub(super) fn check_capabilities(
        &self,
        proposal: &SkillProposal,
        candidate_record_digest: Option<&str>,
    ) -> Result<(), LifecycleError> {
        let candidate_id = proposal
            .candidate_revision
            .as_ref()
            .ok_or_else(|| LifecycleError::ProposalRefused("the proposal has no staged candidate".to_string()))?;
        let candidate = self.read_revision(&proposal.skill_id, candidate_id, candidate_record_digest)?;
        let baseline = self.read_revision(&proposal.skill_id, &proposal.baseline_revision, None)?;
        let mut problems = Vec::new();
        let allowed: BTreeSet<&String> = baseline.revision.capability_requirements.iter().collect();
        for required in &candidate.revision.capability_requirements {
            if !allowed.contains(required) {
                problems.push(format!("it newly requires the capability {:?}", bounded(required)));
            }
        }
        let before = executables(&baseline.bundle);
        for path in executables(&candidate.bundle) {
            if !before.contains(path) {
                problems.push(format!("{} is a new executable file", bounded(path)));
            }
        }
        match (declared_tools(&baseline.bundle), declared_tools(&candidate.bundle)) {
            (Ok(before), Ok(after)) => {
                if !after.is_subset(&before) {
                    problems.push("it declares a tool its baseline did not".to_string());
                }
            }
            (_, Err(e)) | (Err(e), _) => problems.push(e),
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(LifecycleError::CapabilityExpansion {
                proposal_id: proposal.proposal_id.clone(),
                reason: bounded(&problems.join("; ")),
            })
        }
    }
}
