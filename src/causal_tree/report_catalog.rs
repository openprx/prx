//! Authenticated ownership for persisted, owner-scoped evaluation reports.
//! Historical unbound and local-operator reports remain private to local tools.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::evaluate::{EvaluateError, EvaluationReport};
use crate::decision::{DecisionKey, KeyDomain};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ownership {
    version: u8,
    report_id: String,
    owner_tag: [u8; 32],
    content_mac: [u8; 32],
}

fn owner_tag(key: &DecisionKey, workspace: &Path, owner: &str) -> [u8; 32] {
    key.hmac(
        KeyDomain::ReportOwnership,
        &[b"owner", workspace.as_os_str().as_encoded_bytes(), owner.as_bytes()],
    )
}

/// Preserve the report's existing JSON format. Its separate ownership receipt
/// authenticates the exact written bytes, avoiding floating-point round-trip
/// changes. A failed receipt write leaves an unlisted report, never a public one.
pub(crate) fn write_owned_report(
    workspace: &Path,
    owner: &str,
    key: &DecisionKey,
    report: &EvaluationReport,
) -> Result<PathBuf, EvaluateError> {
    if owner.is_empty() || report.scope.kind != "owner" {
        return Err(std::io::Error::other("owner report scope is missing").into());
    }
    let bytes = serde_json::to_vec_pretty(report)?;
    if bytes.len() > 2 * 1024 * 1024 {
        return Err(std::io::Error::other("owner report exceeds its size bound").into());
    }
    let tag = owner_tag(key, workspace, owner);
    let receipt = Ownership {
        version: 1,
        report_id: report.report_id.clone(),
        owner_tag: tag,
        content_mac: key.hmac(
            KeyDomain::ReportOwnership,
            &[b"content", &tag, report.report_id.as_bytes(), &bytes],
        ),
    };
    let path = super::evaluate::write_report(workspace, report)?;
    let receipt_path = path.with_extension("ownership.json");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(receipt_path)?;
    file.write_all(&serde_json::to_vec(&receipt)?)?;
    file.sync_all()?;
    Ok(path)
}

fn read_bounded(path: &Path, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(std::io::Error::other("report is not a bounded regular file"));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(std::io::Error::other("report exceeds its size bound"));
    }
    Ok(bytes)
}

/// Unknown, foreign, legacy and altered reports have the same absent result.
/// No report content is decoded before its ownership and exact bytes verify.
pub(crate) fn verified_report(
    workspace: &Path,
    owner: &str,
    key: &DecisionKey,
    id: uuid::Uuid,
) -> Option<serde_json::Value> {
    if owner.is_empty() || !crate::decision::key::check_decision_dir(workspace).ok()? {
        return None;
    }
    let dir = workspace.join("self/causal_tree/reports");
    if !std::fs::symlink_metadata(&dir).ok()?.file_type().is_dir() {
        return None;
    }
    let id = id.hyphenated().to_string();
    let path = dir.join(format!("{id}.json"));
    let receipt: Ownership =
        serde_json::from_slice(&read_bounded(&path.with_extension("ownership.json"), 4096).ok()?).ok()?;
    if receipt.version != 1
        || receipt.report_id != id
        || !key.verify_hmac(
            KeyDomain::ReportOwnership,
            &[b"owner", workspace.as_os_str().as_encoded_bytes(), owner.as_bytes()],
            &receipt.owner_tag,
        )
    {
        return None;
    }
    let bytes = read_bounded(&path, 2 * 1024 * 1024).ok()?;
    if !key.verify_hmac(
        KeyDomain::ReportOwnership,
        &[b"content", &receipt.owner_tag, id.as_bytes(), &bytes],
        &receipt.content_mac,
    ) {
        return None;
    }
    let report: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    if report.get("report_id")?.as_str()? != id || report.get("scope")?.get("kind")?.as_str()? != "owner" {
        return None;
    }
    Some(report)
}

/// Only authenticated metadata is exposed, never report payloads or endpoints.
#[derive(Debug, Serialize)]
pub(crate) struct SavedReport {
    report_id: String,
    created_at_ms: i64,
    from_ms: i64,
    to_ms: i64,
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct SavedReports {
    reports: Vec<SavedReport>,
    /// False means the bounded scan cannot describe the complete catalog.
    complete: bool,
}

pub(crate) fn list_owned_reports(workspace: &Path, owner: &str, key: &DecisionKey) -> std::io::Result<SavedReports> {
    if owner.is_empty() || !crate::decision::key::check_decision_dir(workspace).map_err(std::io::Error::other)? {
        return Ok(SavedReports::default());
    }
    let dir = workspace.join("self/causal_tree/reports");
    match std::fs::symlink_metadata(&dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SavedReports {
                reports: Vec::new(),
                complete: true,
            });
        }
        Err(error) => return Err(error),
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return Err(std::io::Error::other("report directory is not a directory"));
        }
        Ok(_) => {}
    }
    let mut result = SavedReports {
        reports: Vec::new(),
        complete: true,
    };
    let mut candidates = Vec::new();
    for (index, entry) in std::fs::read_dir(dir)?.enumerate() {
        if index == 4096 {
            result.complete = false;
            break;
        }
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(stem) = name.strip_suffix(".json") else {
            continue;
        };
        let Ok(id) = uuid::Uuid::parse_str(stem) else { continue };
        if id.hyphenated().to_string() == stem {
            candidates.push(id);
        }
    }
    // Bound authenticated file reads to 128 * 2 MiB, including foreign reports.
    // UUID order is deterministic; it does not claim newest-first selection.
    candidates.sort_unstable();
    if candidates.len() > 128 {
        result.complete = false;
        candidates.truncate(128);
    }
    for id in candidates {
        let Some(report) = verified_report(workspace, owner, key, id) else {
            continue;
        };
        let metadata = (|| {
            Some(SavedReport {
                report_id: id.hyphenated().to_string(),
                created_at_ms: report.get("created_at_ms")?.as_i64()?,
                from_ms: report.get("scope")?.get("from_ms")?.as_i64()?,
                to_ms: report.get("scope")?.get("to_ms")?.as_i64()?,
            })
        })();
        if let Some(metadata) = metadata {
            result.reports.push(metadata);
        }
    }
    result.reports.sort_by(|left, right| {
        right
            .created_at_ms
            .cmp(&left.created_at_ms)
            .then_with(|| left.report_id.cmp(&right.report_id))
    });
    Ok(result)
}
