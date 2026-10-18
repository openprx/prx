//! Legacy import (SL02): turn owner-owned `skills/<directory>` entries into
//! immutable revisions, each activated through `publish` (first publish is
//! generation 1; the disabled marker becomes `enabled = false`).
//!
//! [`plan_migration`] is the dry run: it classifies every entry and changes
//! nothing. [`apply_migration`] imports only the entries the plan marked
//! [`MigrationAction::Import`], and only if the directory still hashes to the
//! planned revision. Neither function writes into, renames or deletes anything
//! below `skills/`: the legacy directory remains the compatibility projection,
//! and applying a migration is separate from any later cleanup.

use super::bundle::Bundle;
use super::journal::{TransactionId, TransactionState};
use super::manifest::validate_manifest;
use super::publish::PublishRequest;
use super::store::SkillLifecycleStore;
use super::types::{LifecycleError, NewRevision, RevisionId, SkillId, SourceKind};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

const UNTRUSTED_ORIGIN_MARKER: &str = ".openprx-untrusted-origin.json";
const MAX_MIGRATION_ENTRIES: usize = 1024;
const MAX_IMPORT_ATTEMPTS: u32 = 8;

/// What a migration would do (or did not do) for one legacy directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MigrationAction {
    /// Import this exact bundle and create generation 1 of the binding.
    Import { revision_id: RevisionId, enabled: bool },
    /// A binding already points at a revision identical to the directory.
    AlreadyImported { revision_id: RevisionId },
    /// A binding exists but the directory no longer matches its revision. The
    /// directory is left alone and the revision stays authoritative.
    Drifted { bound: RevisionId, directory: RevisionId },
    /// Not importable. The skill keeps its legacy behaviour; nothing is
    /// rewritten. `reason` says why and what the operator can do.
    Refused { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationEntry {
    pub directory: String,
    pub skill_id: Option<SkillId>,
    pub display_name: Option<String>,
    pub action: MigrationAction,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MigrationPlan {
    pub entries: Vec<MigrationEntry>,
}

/// Result of applying a plan.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MigrationReport {
    pub imported: Vec<(SkillId, RevisionId)>,
    pub failed: Vec<(String, String)>,
}

/// Dry run. `store` is the existing lifecycle store of the owner, when there is
/// one; without it every importable entry is planned as an import.
pub fn plan_migration(
    workspace_dir: &Path,
    store: Option<&SkillLifecycleStore>,
) -> Result<MigrationPlan, LifecycleError> {
    let root = crate::skills::skills_dir(workspace_dir);
    let mut directories = Vec::new();
    match fs::read_dir(&root) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.map_err(|e| LifecycleError::io(&root, e))?;
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                if name.starts_with('.') {
                    continue;
                }
                directories.push((
                    name,
                    entry.file_type().map_err(|e| LifecycleError::io(entry.path(), e))?,
                ));
                if directories.len() > MAX_MIGRATION_ENTRIES {
                    return Err(LifecycleError::InvalidRecord {
                        path: root,
                        reason: format!("more than {MAX_MIGRATION_ENTRIES} skill directories"),
                    });
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(LifecycleError::io(&root, e)),
    }
    directories.sort_by(|a, b| a.0.cmp(&b.0));

    let mut seen_names = BTreeSet::new();
    let mut plan = MigrationPlan::default();
    for (directory, file_type) in directories {
        if !file_type.is_symlink() && !file_type.is_dir() {
            // Plain files such as README.md are not skills.
            continue;
        }
        let entry = classify(
            workspace_dir,
            &root,
            &directory,
            file_type.is_symlink(),
            store,
            &mut seen_names,
        )?;
        plan.entries.push(entry);
    }
    Ok(plan)
}

fn refused(directory: &str, skill_id: Option<SkillId>, display_name: Option<String>, reason: String) -> MigrationEntry {
    MigrationEntry {
        directory: directory.to_string(),
        skill_id,
        display_name,
        action: MigrationAction::Refused { reason },
    }
}

fn classify(
    workspace_dir: &Path,
    root: &Path,
    directory: &str,
    is_symlink: bool,
    store: Option<&SkillLifecycleStore>,
    seen_names: &mut BTreeSet<String>,
) -> Result<MigrationEntry, LifecycleError> {
    let skill_id = match SkillId::from_legacy_dir(directory) {
        Ok(id) => id,
        Err(e) => return Ok(refused(directory, None, None, format!("no stable id: {e}"))),
    };
    if is_symlink {
        return Ok(refused(
            directory,
            Some(skill_id),
            None,
            "symlinked skill: shared or external content is never imported in place; it keeps its legacy behaviour, \
             or an owner fork can be created from a real directory"
                .to_string(),
        ));
    }
    let bundle = match Bundle::from_dir(&root.join(directory)) {
        Ok(bundle) => bundle,
        Err(e) => {
            return Ok(refused(
                directory,
                Some(skill_id),
                None,
                format!("bundle rejected: {e}"),
            ));
        }
    };
    let info = match validate_manifest(&bundle) {
        Ok(info) => info,
        Err(e) => {
            return Ok(refused(
                directory,
                Some(skill_id),
                None,
                format!("manifest rejected: {e}"),
            ));
        }
    };
    if !seen_names.insert(info.name.to_lowercase()) {
        return Ok(refused(
            directory,
            Some(skill_id),
            Some(info.name),
            "another directory already uses this display name".to_string(),
        ));
    }
    let revision_id = bundle.revision_id().clone();
    let action = match store.map(|s| s.active_binding(&skill_id)).transpose()?.flatten() {
        Some(binding) if binding.revision_id == revision_id => MigrationAction::AlreadyImported { revision_id },
        Some(binding) => MigrationAction::Drifted {
            bound: binding.revision_id,
            directory: revision_id,
        },
        None => MigrationAction::Import {
            revision_id,
            enabled: !crate::skills::disabled_skill_marker(workspace_dir, directory).is_file(),
        },
    };
    Ok(MigrationEntry {
        directory: directory.to_string(),
        skill_id: Some(skill_id),
        display_name: Some(info.name),
        action,
    })
}

/// Imports the `Import` entries of `plan`. Each import re-reads the directory
/// and proceeds only if it still hashes to the planned revision; a failure is
/// reported per entry and does not stop the others. The caller is responsible
/// for the migration authorization (SL09 gates this with the shared policy).
/// Each activation goes through `publish`, so it takes the store and skill
/// locks and writes a journal transaction; the call blocks while waiting for
/// them (async callers use `spawn_blocking`).
pub fn apply_migration(store: &SkillLifecycleStore, workspace_dir: &Path, plan: &MigrationPlan) -> MigrationReport {
    let root = crate::skills::skills_dir(workspace_dir);
    let mut report = MigrationReport::default();
    for entry in &plan.entries {
        let (MigrationAction::Import { revision_id, enabled }, Some(skill_id)) = (&entry.action, &entry.skill_id)
        else {
            continue;
        };
        match import_one(store, &root, &entry.directory, skill_id, revision_id, *enabled) {
            Ok(()) => report.imported.push((skill_id.clone(), revision_id.clone())),
            Err(e) => report.failed.push((entry.directory.clone(), e.to_string())),
        }
    }
    report
}

fn import_one(
    store: &SkillLifecycleStore,
    root: &Path,
    directory: &str,
    skill_id: &SkillId,
    planned: &RevisionId,
    enabled: bool,
) -> Result<(), LifecycleError> {
    let bundle = Bundle::from_dir(&root.join(directory))?;
    if bundle.revision_id() != planned {
        return Err(LifecycleError::RevisionConflict {
            revision: format!("{skill_id}/{planned}"),
            field: "directory content changed since the plan",
        });
    }
    let source_kind = if bundle.file(UNTRUSTED_ORIGIN_MARKER).is_some() {
        SourceKind::Upstream
    } else {
        SourceKind::Operator
    };
    let written = store.write_revision(&NewRevision::new(skill_id.clone(), source_kind), &bundle)?;
    // An import is always the skill's first publication: generation 0 is
    // expected, so a retry after commit replays instead of conflicting, and a
    // plan that went stale because someone else published meanwhile fails with
    // `StaleGeneration` instead of overwriting their revision.
    //
    // The transaction id is deterministic (skill, revision, enabled, attempt),
    // so re-applying a plan replays a committed import. An attempt that ended
    // ABORTED (a crash before the binding swap) can never be re-run, so the
    // next attempt number derives the next id.
    for attempt in 0..MAX_IMPORT_ATTEMPTS {
        let transaction_id = import_transaction_id(skill_id, planned, enabled, attempt)?;
        if store
            .journal_entry(&transaction_id)?
            .is_some_and(|entry| entry.state == TransactionState::Aborted)
        {
            continue;
        }
        let request = PublishRequest {
            transaction_id,
            skill_id: skill_id.clone(),
            candidate: planned.clone(),
            candidate_record_digest: written.record_digest.clone(),
            expected_generation: 0,
            enabled,
        };
        match store.publish_migration(&request) {
            // A PREPARED entry of a dead writer was just reconciled to ABORTED.
            Err(LifecycleError::TransactionNotCommitted { .. }) => {}
            other => return other.map(|_| ()),
        }
    }
    Err(LifecycleError::ImportAttemptsExhausted {
        skill_id: skill_id.to_string(),
        attempts: MAX_IMPORT_ATTEMPTS,
    })
}

fn import_transaction_id(
    skill_id: &SkillId,
    planned: &RevisionId,
    enabled: bool,
    attempt: u32,
) -> Result<TransactionId, LifecycleError> {
    let seed = format!("{skill_id}\0{planned}\0{enabled}");
    let digest = if attempt == 0 {
        hex::encode(Sha256::digest(seed.as_bytes()))
    } else {
        hex::encode(Sha256::digest(format!("{seed}\0{attempt}").as_bytes()))
    };
    TransactionId::parse(&format!("mig-{}", digest.get(..32).unwrap_or(&digest)))
}
