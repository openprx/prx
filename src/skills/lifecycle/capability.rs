//! Capability re-check at publication (spec 8.3 step 3).
//!
//! A learned revision may use what its baseline used and nothing more:
//! - no new declared capability requirement;
//! - no new executable file, and no executable whose bytes changed;
//! - no new `[[tools]]` entry in `SKILL.toml`, and no file a tool entry
//!   references (a bundle path named in its command) whose bytes changed.
//!
//! The check reads both revisions from the store again at publication, so a
//! candidate that changed after the source review (or was written by another
//! path) cannot widen what the skill may do. Files are compared by SHA-256.

use super::bundle::{Bundle, BundleFile};
use super::manifest::validate_manifest;
use super::proposal::{SkillProposal, bounded};
use super::store::SkillLifecycleStore;
use super::types::LifecycleError;
use sha2::{Digest, Sha256};
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

fn digest_of(bundle: &Bundle, path: &str) -> Option<String> {
    bundle.file(path).map(|f| hex::encode(Sha256::digest(f.bytes())))
}

/// Bundle paths named by the `command` of a `[[tools]]` entry of `bundle`.
fn tool_files(bundle: &Bundle) -> BTreeSet<String> {
    let Some(file) = bundle.file(SKILL_TOML) else {
        return BTreeSet::new();
    };
    let Ok(text) = std::str::from_utf8(file.bytes()) else {
        return BTreeSet::new();
    };
    let Ok(value) = toml::from_str::<toml::Value>(text) else {
        return BTreeSet::new();
    };
    let commands: Vec<&str> = value
        .get("tools")
        .and_then(toml::Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool.get("command").and_then(toml::Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    bundle
        .files()
        .iter()
        .map(BundleFile::path)
        .filter(|path| commands.iter().any(|command| command.contains(path)))
        .map(str::to_string)
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
    /// The capability check of a NEW skill, which has no baseline to compare
    /// with: it is instruction-only (`SKILL.md` alone, not executable, no
    /// `SKILL.toml` and so no declared tool), its recorded requirements equal
    /// its manifest's, and they stay within the capabilities the patterns it
    /// rests on are scoped to. Executable bundles are refused until SL11.
    fn check_new_skill_capabilities(
        &self,
        proposal: &SkillProposal,
        candidate: &super::store::VerifiedRevision,
    ) -> Result<(), LifecycleError> {
        // The prepare step checked the name; another skill may have taken it
        // since, so it is checked again before the first publication. At
        // publication this runs under the proposals lock, which serialises
        // every proposal-driven publication in the store (across processes),
        // so two new skills cannot both pass it. Writers outside proposals
        // (migration imports, operator publishes) are not covered by it.
        if let Some((holder, _)) = self.find_binding_by_name(&candidate.revision.display_name)?
            && holder.skill_id != proposal.skill_id
        {
            return Err(LifecycleError::SkillNameTaken {
                name: candidate.revision.display_name.clone(),
                holder: holder.skill_id.to_string(),
            });
        }
        let mut problems = Vec::new();
        for file in candidate.bundle.files() {
            if file.path() != "SKILL.md" {
                problems.push(format!(
                    "{} is not allowed in a new instruction skill",
                    bounded(file.path())
                ));
            } else if file.is_executable() {
                problems.push("SKILL.md is marked executable".to_string());
            }
        }
        match validate_manifest(&candidate.bundle) {
            Ok(info) => {
                if candidate.revision.capability_requirements != info.required_tools {
                    problems.push("its recorded requirements differ from its manifest".to_string());
                }
                let mut scoped: BTreeSet<String> = BTreeSet::new();
                for id in &proposal.pattern_ids {
                    match self.read_pattern(id)? {
                        Some(pattern) => scoped.extend(pattern.scope.capabilities),
                        None => problems.push(format!("pattern {id} it rests on is missing")),
                    }
                }
                for required in info.required_tools.iter().filter(|tool| !scoped.contains(*tool)) {
                    problems.push(format!(
                        "it requires {:?}, outside the capabilities its patterns are scoped to",
                        bounded(required)
                    ));
                }
            }
            Err(e) => problems.push(format!("its manifest is invalid: {e}")),
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
        let Some(baseline_revision) = &proposal.baseline_revision else {
            return self.check_new_skill_capabilities(proposal, &candidate);
        };
        let baseline = self.read_revision(&proposal.skill_id, baseline_revision, None)?;
        let mut problems = Vec::new();
        let allowed: BTreeSet<&String> = baseline.revision.capability_requirements.iter().collect();
        for required in &candidate.revision.capability_requirements {
            if !allowed.contains(required) {
                problems.push(format!("it newly requires the capability {:?}", bounded(required)));
            }
        }
        // The required tools are the adherence oracle: a learned revision
        // keeps exactly its baseline's, in the record and in its manifests.
        if candidate.revision.capability_requirements != baseline.revision.capability_requirements {
            problems.push("it changes the required tools its baseline recorded".to_string());
        }
        match (
            validate_manifest(&baseline.bundle),
            validate_manifest(&candidate.bundle),
        ) {
            (Ok(before), Ok(after)) => {
                if before.required_tools != after.required_tools {
                    problems.push("its manifests change the required tools".to_string());
                }
            }
            (Err(e), _) | (_, Err(e)) => problems.push(format!("a manifest is invalid: {e}")),
        }
        let before = executables(&baseline.bundle);
        for path in executables(&candidate.bundle) {
            if !before.contains(path) {
                problems.push(format!("{} is a new executable file", bounded(path)));
            } else if digest_of(&baseline.bundle, path) != digest_of(&candidate.bundle, path) {
                problems.push(format!("the executable {} changed", bounded(path)));
            }
        }
        for path in tool_files(&candidate.bundle) {
            if digest_of(&baseline.bundle, &path) != digest_of(&candidate.bundle, &path) {
                problems.push(format!("{} is run by a declared tool and changed", bounded(&path)));
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
