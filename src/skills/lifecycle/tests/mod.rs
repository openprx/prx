mod bundle;
mod manifest;
mod publish;
mod store;

use super::{BundleEntry, OwnerScope};
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
