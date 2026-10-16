#![cfg(test)]

mod binding;
mod bundle;
mod capture;
mod evaluate;
mod evaluate_agent;
mod knowledge;
mod manifest;
mod migrate;
mod publish;
mod store;

use super::{
    ActiveBinding, BundleEntry, OwnerScope, PublishRequest, RevisionId, SkillId, SkillLifecycleStore, TransactionId,
};
use std::path::PathBuf;

pub(super) fn md(name: &str, description: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n\nInstructions body.\n")
}

pub(super) fn entry(path: &str, bytes: &[u8]) -> BundleEntry {
    BundleEntry {
        path: PathBuf::from(path),
        bytes: bytes.to_vec(),
        executable: false,
    }
}

pub(super) fn exec_entry(path: &str, bytes: &[u8]) -> BundleEntry {
    BundleEntry {
        executable: true,
        ..entry(path, bytes)
    }
}

pub(super) fn basic_entries() -> Vec<BundleEntry> {
    vec![
        entry("SKILL.md", md("Deploy helper", "Helps with deploys").as_bytes()),
        entry("references/guide.md", b"# Guide\r\nkeep CRLF\r\n"),
        exec_entry("scripts/run.sh", b"#!/bin/sh\necho hi\n"),
    ]
}

pub(super) fn owner(id: &str) -> OwnerScope {
    OwnerScope::new(id).expect("test: valid owner scope")
}

/// Publishes `revision` through the SL08 path with a fresh transaction id.
pub(super) fn activate(
    store: &SkillLifecycleStore,
    skill: &SkillId,
    revision: &(RevisionId, String),
    expected_generation: u64,
    enabled: bool,
) -> ActiveBinding {
    store
        .publish(&PublishRequest {
            transaction_id: TransactionId::generate(),
            skill_id: skill.clone(),
            candidate: revision.0.clone(),
            candidate_record_digest: revision.1.clone(),
            expected_generation,
            enabled,
        })
        .expect("test: publish");
    store
        .active_binding(skill)
        .expect("test: binding readable")
        .expect("test: binding present")
}
