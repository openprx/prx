use crate::chat::session::ChatTurn;
use crate::security::op_id;
use crate::security::policy::{ApprovalGrant, ResourceRiskLevel};
use crate::security::{SecurityPolicy, SideEffectGate};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

pub(crate) const DIFF_APPLY_MAX_BYTES: usize = 256 * 1024;
pub(crate) const DIFF_APPLY_MAX_LINES: usize = 2_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiffApplyPlan {
    files: Vec<FilePatch>,
}

impl DiffApplyPlan {
    #[must_use]
    pub(crate) const fn file_count(&self) -> usize {
        self.files.len()
    }

    #[must_use]
    pub(crate) fn summary(&self) -> String {
        let mut out = format!(
            "Apply fenced diff: {} file{}, {} additions, {} deletions",
            self.files.len(),
            if self.files.len() == 1 { "" } else { "s" },
            self.additions(),
            self.deletions()
        );
        for file in self.files.iter().take(8) {
            let op = if file.is_add { "add" } else { "modify" };
            out.push_str(&format!(
                "\n- {op} {} (+{} -{})",
                file.path, file.additions, file.deletions
            ));
        }
        if self.files.len() > 8 {
            out.push_str("\n- [targets truncated]");
        }
        out
    }

    #[must_use]
    pub(crate) fn approval_args_json(&self) -> String {
        serde_json::json!({
            "operation": "apply_fenced_diff",
            "file_count": self.files.len(),
            "additions": self.additions(),
            "deletions": self.deletions(),
            "targets": self.files.iter().take(12).map(|file| {
                serde_json::json!({
                    "path": file.path,
                    "operation": if file.is_add { "add" } else { "modify" },
                    "additions": file.additions,
                    "deletions": file.deletions,
                })
            }).collect::<Vec<_>>(),
            "targets_truncated": self.files.len() > 12,
        })
        .to_string()
    }

    fn additions(&self) -> usize {
        self.files.iter().map(|file| file.additions).sum()
    }

    fn deletions(&self) -> usize {
        self.files.iter().map(|file| file.deletions).sum()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FilePatch {
    path: String,
    is_add: bool,
    hunks: Vec<Hunk>,
    additions: usize,
    deletions: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Hunk {
    old_start: usize,
    lines: Vec<HunkLine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HunkLineKind {
    Context,
    Add,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HunkLine {
    kind: HunkLineKind,
    text: String,
    no_newline: bool,
}

impl HunkLine {
    fn old_segment(&self) -> Option<String> {
        match self.kind {
            HunkLineKind::Context | HunkLineKind::Delete => Some(self.segment()),
            HunkLineKind::Add => None,
        }
    }

    fn new_segment(&self) -> Option<String> {
        match self.kind {
            HunkLineKind::Context | HunkLineKind::Add => Some(self.segment()),
            HunkLineKind::Delete => None,
        }
    }

    fn segment(&self) -> String {
        if self.no_newline {
            self.text.clone()
        } else {
            format!("{}\n", self.text)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DiffApplyError {
    MissingFencedDiff,
    Oversized,
    Malformed(String),
    Unsupported(String),
    RejectedPath(String),
    Stale(String),
    Io(String),
}

impl std::fmt::Display for DiffApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingFencedDiff => write!(f, "no applicable fenced diff block found"),
            Self::Oversized => write!(f, "diff is too large to apply safely"),
            Self::Malformed(msg) => write!(f, "malformed diff: {msg}"),
            Self::Unsupported(msg) => write!(f, "unsupported diff: {msg}"),
            Self::RejectedPath(msg) => write!(f, "path rejected: {msg}"),
            Self::Stale(msg) => write!(f, "stale patch: {msg}"),
            Self::Io(msg) => write!(f, "diff apply I/O error: {msg}"),
        }
    }
}

impl std::error::Error for DiffApplyError {}

#[must_use]
pub(crate) fn latest_fenced_diff(turns: &[ChatTurn], latest_index: usize) -> Option<String> {
    let mut remaining = latest_index.max(1);
    for turn in turns
        .iter()
        .rev()
        .filter(|turn| matches!(turn.role.as_str(), "assistant" | "user"))
    {
        let blocks = fenced_diff_blocks(&turn.content);
        for block in blocks.into_iter().rev() {
            if remaining == 1 {
                return Some(block);
            }
            remaining = remaining.saturating_sub(1);
        }
    }
    None
}

#[must_use]
pub(crate) fn fenced_diff_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut in_diff = false;
    let mut current = String::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if !in_diff {
            if let Some(lang) = trimmed.strip_prefix("```") {
                let lang = lang.trim().to_ascii_lowercase();
                in_diff = matches!(lang.as_str(), "diff" | "patch" | "unified-diff" | "udiff");
                if in_diff {
                    current.clear();
                }
            }
            continue;
        }

        if trimmed.starts_with("```") {
            blocks.push(current.clone());
            current.clear();
            in_diff = false;
            continue;
        }
        current.push_str(line);
        current.push('\n');
    }
    blocks
}

pub(crate) fn parse_unified_diff(diff: &str) -> Result<DiffApplyPlan, DiffApplyError> {
    if diff.len() > DIFF_APPLY_MAX_BYTES || diff.lines().count() > DIFF_APPLY_MAX_LINES {
        return Err(DiffApplyError::Oversized);
    }
    if diff.contains('\0') {
        return Err(DiffApplyError::Unsupported(
            "binary diff contains NUL bytes".to_string(),
        ));
    }

    let lines = diff.lines().map(|line| line.trim_end_matches('\r')).collect::<Vec<_>>();
    let mut idx = 0usize;
    let mut files = Vec::new();
    while idx < lines.len() {
        let line = lines.get(idx).copied().unwrap_or_default();
        if line.trim().is_empty() {
            idx = idx.saturating_add(1);
            continue;
        }
        if line.starts_with("Binary files ") || line == "GIT binary patch" {
            return Err(DiffApplyError::Unsupported(
                "binary patches are not supported".to_string(),
            ));
        }
        if is_unsupported_metadata(line) {
            return Err(DiffApplyError::Unsupported(line.to_string()));
        }
        if line.starts_with("diff --git ") {
            idx = idx.saturating_add(1);
            while idx < lines.len() {
                let meta = lines.get(idx).copied().unwrap_or_default();
                if meta.starts_with("--- ") {
                    break;
                }
                if meta.starts_with("Binary files ") || meta == "GIT binary patch" || is_unsupported_metadata(meta) {
                    return Err(DiffApplyError::Unsupported(meta.to_string()));
                }
                idx = idx.saturating_add(1);
            }
        }
        if !lines.get(idx).copied().unwrap_or_default().starts_with("--- ") {
            return Err(DiffApplyError::Malformed(format!(
                "expected file header at line {}",
                idx + 1
            )));
        }
        let old_path = parse_header_path(lines.get(idx).copied().unwrap_or_default(), "--- ")?;
        idx = idx.saturating_add(1);
        let new_line = lines
            .get(idx)
            .copied()
            .ok_or_else(|| DiffApplyError::Malformed("missing +++ file header".to_string()))?;
        let new_path = parse_header_path(new_line, "+++ ")?;
        idx = idx.saturating_add(1);

        if new_path == "/dev/null" {
            return Err(DiffApplyError::Unsupported(
                "delete patches are not supported".to_string(),
            ));
        }
        let is_add = old_path == "/dev/null";
        let path = normalize_diff_path(&new_path)?;
        if !is_add {
            let old_normalized = normalize_diff_path(&old_path)?;
            if old_normalized != path {
                return Err(DiffApplyError::Unsupported(
                    "rename patches are not supported".to_string(),
                ));
            }
        }
        let mut hunks = Vec::new();
        let mut additions = 0usize;
        let mut deletions = 0usize;
        while idx < lines.len() {
            let line = lines.get(idx).copied().unwrap_or_default();
            if line.starts_with("diff --git ") || line.starts_with("--- ") {
                break;
            }
            if line.trim().is_empty() {
                idx = idx.saturating_add(1);
                continue;
            }
            if !line.starts_with("@@ ") {
                return Err(DiffApplyError::Malformed(format!(
                    "expected hunk header at line {}",
                    idx + 1
                )));
            }
            let old_start = parse_hunk_old_start(line)?;
            idx = idx.saturating_add(1);
            let mut hunk_lines: Vec<HunkLine> = Vec::new();
            while idx < lines.len() {
                let hline = lines.get(idx).copied().unwrap_or_default();
                if hline.starts_with("@@ ") || hline.starts_with("diff --git ") || hline.starts_with("--- ") {
                    break;
                }
                if hline.starts_with("\\ No newline at end of file") {
                    if let Some(last) = hunk_lines.last_mut() {
                        last.no_newline = true;
                    }
                    idx = idx.saturating_add(1);
                    continue;
                }
                let Some((kind, text)) = parse_hunk_line(hline) else {
                    return Err(DiffApplyError::Malformed(format!("invalid hunk line at {}", idx + 1)));
                };
                match kind {
                    HunkLineKind::Add => additions = additions.saturating_add(1),
                    HunkLineKind::Delete => deletions = deletions.saturating_add(1),
                    HunkLineKind::Context => {}
                }
                hunk_lines.push(HunkLine {
                    kind,
                    text: text.to_string(),
                    no_newline: false,
                });
                idx = idx.saturating_add(1);
            }
            if hunk_lines.is_empty() {
                return Err(DiffApplyError::Malformed("empty hunk".to_string()));
            }
            hunks.push(Hunk {
                old_start,
                lines: hunk_lines,
            });
        }
        if hunks.is_empty() {
            return Err(DiffApplyError::Malformed(format!("{} has no hunks", path)));
        }
        files.push(FilePatch {
            path,
            is_add,
            hunks,
            additions,
            deletions,
        });
    }

    if files.is_empty() {
        return Err(DiffApplyError::Malformed("no file patches found".to_string()));
    }
    Ok(DiffApplyPlan { files })
}

fn parse_header_path(line: &str, prefix: &str) -> Result<String, DiffApplyError> {
    let raw = line
        .strip_prefix(prefix)
        .ok_or_else(|| DiffApplyError::Malformed(format!("missing {prefix} header")))?;
    let path = raw.split('\t').next().map(str::trim).unwrap_or_default();
    if path.is_empty() {
        return Err(DiffApplyError::Malformed("empty file path".to_string()));
    }
    Ok(path.to_string())
}

fn normalize_diff_path(path: &str) -> Result<String, DiffApplyError> {
    if path.starts_with('"') || path == "/dev/null" {
        return Err(DiffApplyError::RejectedPath(path.to_string()));
    }
    let stripped = path
        .strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path);
    let path = stripped.trim();
    if path.is_empty() || Path::new(path).is_absolute() || path.contains("..") || path.contains('\0') {
        return Err(DiffApplyError::RejectedPath(path.to_string()));
    }
    Ok(path.to_string())
}

fn parse_hunk_old_start(line: &str) -> Result<usize, DiffApplyError> {
    let mut parts = line.split_whitespace();
    let _marker = parts.next();
    let old = parts
        .next()
        .ok_or_else(|| DiffApplyError::Malformed("missing old hunk range".to_string()))?;
    let old = old
        .strip_prefix('-')
        .ok_or_else(|| DiffApplyError::Malformed("invalid old hunk range".to_string()))?;
    let start = old.split(',').next().unwrap_or_default();
    start
        .parse::<usize>()
        .map_err(|_| DiffApplyError::Malformed(format!("invalid old hunk start: {start}")))
}

fn parse_hunk_line(line: &str) -> Option<(HunkLineKind, &str)> {
    let mut chars = line.char_indices();
    let (_, first) = chars.next()?;
    let rest_start = chars.next().map_or(line.len(), |(idx, _)| idx);
    let rest = line.get(rest_start..)?;
    match first {
        ' ' => Some((HunkLineKind::Context, rest)),
        '+' => Some((HunkLineKind::Add, rest)),
        '-' => Some((HunkLineKind::Delete, rest)),
        _ => None,
    }
}

fn is_unsupported_metadata(line: &str) -> bool {
    line.starts_with("rename from ")
        || line.starts_with("rename to ")
        || line.starts_with("deleted file mode ")
        || line.starts_with("old mode ")
        || line.starts_with("new mode ")
        || line.starts_with("similarity index ")
        || line.starts_with("dissimilarity index ")
        || line.starts_with("Subproject commit ")
}

pub(crate) async fn execute_plan(plan: &DiffApplyPlan, security: &SecurityPolicy) -> Result<String, DiffApplyError> {
    if !security.can_act() {
        return Err(DiffApplyError::RejectedPath("autonomy is read-only".to_string()));
    }
    if security.is_rate_limited() {
        return Err(DiffApplyError::RejectedPath("action rate limit exceeded".to_string()));
    }

    let mut prepared = Vec::with_capacity(plan.files.len());
    for file in &plan.files {
        let target = validate_target(file, security).await?;
        let current = read_current(file, &target).await?;
        let next = apply_file_patch(file, &current)?;
        prepared.push(PreparedWrite {
            target,
            content: next,
            create_new: file.is_add,
        });
    }

    for item in &prepared {
        authorize_write(&item.target, security)?;
    }
    for item in prepared {
        write_target(&item.target, item.content, item.create_new).await?;
    }
    if !security.record_action() {
        return Err(DiffApplyError::RejectedPath("action budget exhausted".to_string()));
    }

    Ok(format!(
        "Applied fenced diff to {} file{}.",
        plan.file_count(),
        if plan.file_count() == 1 { "" } else { "s" }
    ))
}

struct PreparedWrite {
    target: PathBuf,
    content: String,
    create_new: bool,
}

async fn validate_target(file: &FilePatch, security: &SecurityPolicy) -> Result<PathBuf, DiffApplyError> {
    if !security.is_path_allowed(&file.path) {
        return Err(DiffApplyError::RejectedPath(file.path.clone()));
    }
    let full_path = security.workspace_dir.join(&file.path);
    let Some(parent) = full_path.parent() else {
        return Err(DiffApplyError::RejectedPath("missing parent directory".to_string()));
    };
    let resolved_parent = tokio::fs::canonicalize(parent)
        .await
        .map_err(|err| DiffApplyError::RejectedPath(format!("failed to resolve parent: {err}")))?;
    if !security.is_resolved_path_allowed(&resolved_parent) {
        return Err(DiffApplyError::RejectedPath(format!(
            "resolved parent escapes workspace: {}",
            resolved_parent.display()
        )));
    }
    let Some(file_name) = full_path.file_name() else {
        return Err(DiffApplyError::RejectedPath("missing file name".to_string()));
    };
    let target = resolved_parent.join(file_name);
    reject_protected_memory_path(&target, security)?;
    match tokio::fs::symlink_metadata(&target).await {
        Ok(meta) => {
            let file_type = meta.file_type();
            if file_type.is_symlink() {
                return Err(DiffApplyError::RejectedPath(format!(
                    "symlink target rejected: {}",
                    file.path
                )));
            }
            if file_type.is_dir() {
                return Err(DiffApplyError::RejectedPath(format!(
                    "directory target rejected: {}",
                    file.path
                )));
            }
            if file.is_add {
                return Err(DiffApplyError::Stale(format!("{} already exists", file.path)));
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if !file.is_add {
                return Err(DiffApplyError::Stale(format!("{} does not exist", file.path)));
            }
        }
        Err(err) => return Err(DiffApplyError::Io(format!("failed to stat {}: {err}", file.path))),
    }
    Ok(target)
}

fn reject_protected_memory_path(target: &Path, security: &SecurityPolicy) -> Result<(), DiffApplyError> {
    let workspace = security
        .workspace_dir
        .canonicalize()
        .unwrap_or_else(|_| security.workspace_dir.clone());
    let Ok(rel) = target.strip_prefix(&workspace) else {
        return Err(DiffApplyError::RejectedPath("target escapes workspace".to_string()));
    };
    if rel
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("MEMORY.md") || name.eq_ignore_ascii_case("MEMORY_SNAPSHOT.md"))
    {
        return Err(DiffApplyError::RejectedPath(
            "memory files are protected by ACL policy".to_string(),
        ));
    }
    if rel.components().count() == 2 {
        let mut components = rel.components();
        let first = components.next().and_then(|c| c.as_os_str().to_str());
        let second = components.next().and_then(|c| c.as_os_str().to_str());
        if first.is_some_and(|part| part.eq_ignore_ascii_case("memory"))
            && second.is_some_and(|part| {
                part.eq_ignore_ascii_case("brain.db")
                    || part.eq_ignore_ascii_case("brain.db-wal")
                    || part.eq_ignore_ascii_case("brain.db-shm")
                    || part.eq_ignore_ascii_case("brain.db-journal")
            })
        {
            return Err(DiffApplyError::RejectedPath(
                "memory files are protected by ACL policy".to_string(),
            ));
        }
    }
    if rel
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
        && rel.components().any(|component| {
            component
                .as_os_str()
                .to_str()
                .is_some_and(|part| part.eq_ignore_ascii_case("memory"))
        })
    {
        return Err(DiffApplyError::RejectedPath(
            "memory files are protected by ACL policy".to_string(),
        ));
    }
    Ok(())
}

async fn read_current(file: &FilePatch, target: &Path) -> Result<String, DiffApplyError> {
    if file.is_add {
        return Ok(String::new());
    }
    let target = target.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<String, DiffApplyError> {
        let mut opts = std::fs::OpenOptions::new();
        opts.read(true);
        #[cfg(unix)]
        opts.custom_flags(libc::O_NOFOLLOW);
        let mut opened = opts
            .open(&target)
            .map_err(|err| DiffApplyError::Io(format!("failed to read {}: {err}", target.display())))?;
        let mut contents = String::new();
        std::io::Read::read_to_string(&mut opened, &mut contents)
            .map_err(|err| DiffApplyError::Unsupported(format!("target is not valid UTF-8 text: {err}")))?;
        Ok(contents)
    })
    .await
    .map_err(|err| DiffApplyError::Io(format!("read task failed: {err}")))?
}

fn authorize_write(target: &Path, security: &SecurityPolicy) -> Result<(), DiffApplyError> {
    let operation = op_id::op_id("file_write", "write", &[&op_id::ref_for_file(target)]);
    let grant = ApprovalGrant::for_resource_operation("file_write", &operation, "chat-operator", None);
    SideEffectGate::new(security)
        .authorize_resource_operation("file_write", &operation, ResourceRiskLevel::Medium, Some(&grant))
        .map(|_| ())
        .map_err(DiffApplyError::RejectedPath)
}

async fn write_target(target: &Path, content: String, create_new: bool) -> Result<(), DiffApplyError> {
    let target = target.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<(), DiffApplyError> {
        use std::io::Write as _;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true);
        if create_new {
            opts.create_new(true);
        } else {
            opts.create(false).truncate(true);
        }
        #[cfg(unix)]
        opts.custom_flags(libc::O_NOFOLLOW);
        let mut opened = opts
            .open(&target)
            .map_err(|err| DiffApplyError::Io(format!("failed to write {}: {err}", target.display())))?;
        opened
            .write_all(content.as_bytes())
            .map_err(|err| DiffApplyError::Io(format!("failed to write {}: {err}", target.display())))
    })
    .await
    .map_err(|err| DiffApplyError::Io(format!("write task failed: {err}")))?
}

fn apply_file_patch(file: &FilePatch, current: &str) -> Result<String, DiffApplyError> {
    let original = split_preserve_newlines(current);
    let mut output = Vec::new();
    let mut cursor = 0usize;
    for hunk in &file.hunks {
        let start = hunk.old_start.saturating_sub(1);
        if start < cursor || start > original.len() {
            return Err(DiffApplyError::Stale(format!(
                "{} hunk offset no longer matches",
                file.path
            )));
        }
        output.extend(original.iter().skip(cursor).take(start.saturating_sub(cursor)).cloned());
        cursor = start;
        for line in &hunk.lines {
            if let Some(expected) = line.old_segment() {
                let Some(actual) = original.get(cursor) else {
                    return Err(DiffApplyError::Stale(format!("{} hunk exceeds file", file.path)));
                };
                if actual != &expected {
                    return Err(DiffApplyError::Stale(format!(
                        "{} content no longer matches",
                        file.path
                    )));
                }
                if matches!(line.kind, HunkLineKind::Context) {
                    output.push(actual.clone());
                }
                cursor = cursor.saturating_add(1);
            }
            if let Some(new_segment) = line.new_segment()
                && matches!(line.kind, HunkLineKind::Add)
            {
                output.push(new_segment);
            }
        }
    }
    output.extend(original.into_iter().skip(cursor));
    Ok(output.concat())
}

fn split_preserve_newlines(text: &str) -> Vec<String> {
    text.split_inclusive('\n').map(ToString::to_string).collect()
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::security::{AutonomyLevel, SecurityPolicy};

    fn policy(workspace: &Path) -> SecurityPolicy {
        SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: workspace.to_path_buf(),
            ..SecurityPolicy::default()
        }
    }

    fn diff_for(path: &str, old: &str, new: &str) -> String {
        format!("--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-{old}\n+{new}\n")
    }

    #[test]
    fn latest_fenced_diff_picks_newest_assistant_or_user_block() {
        let turns = vec![
            ChatTurn {
                role: "assistant".to_string(),
                content: "```diff\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-old\n+new\n```".to_string(),
                timestamp: chrono::Utc::now(),
                tool_calls: Vec::new(),
            },
            ChatTurn {
                role: "user".to_string(),
                content: "```diff\n--- a/b\n+++ b/b\n@@ -1 +1 @@\n-left\n+right\n```".to_string(),
                timestamp: chrono::Utc::now(),
                tool_calls: Vec::new(),
            },
        ];
        let latest = latest_fenced_diff(&turns, 1).unwrap_or_default();
        assert!(latest.contains("+++ b/b"));
        let previous = latest_fenced_diff(&turns, 2).unwrap_or_default();
        assert!(previous.contains("+++ b/a"));
    }

    #[test]
    fn parse_rejects_literal_oversized_cap() {
        let oversized = format!("--- a/x\n+++ b/x\n@@ -1 +1 @@\n-{}\n+x\n", "a".repeat(262_145));
        assert_eq!(parse_unified_diff(&oversized), Err(DiffApplyError::Oversized));
    }

    #[test]
    fn parse_rejects_unsupported_delete_rename_mode_and_binary() {
        for diff in [
            "--- a/a\n+++ /dev/null\n@@ -1 +0,0 @@\n-old\n",
            "rename from a\nrename to b\n--- a/a\n+++ b/b\n@@ -1 +1 @@\n-a\n+b\n",
            "old mode 100644\nnew mode 100755\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-a\n+b\n",
            "Binary files a/a and b/a differ\n",
        ] {
            assert!(matches!(
                parse_unified_diff(diff),
                Err(DiffApplyError::Unsupported(_) | DiffApplyError::Malformed(_))
            ));
        }
    }

    #[tokio::test]
    async fn execute_plan_approves_and_writes_modify_and_add() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        tokio::fs::write(temp.path().join("a.txt"), "old\n")
            .await
            .expect("seed");
        let plan = parse_unified_diff(
            "--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-old\n+new\n--- /dev/null\n+++ b/b.txt\n@@ -0,0 +1 @@\n+added\n",
        )
        .expect("plan");
        let result = execute_plan(&plan, &policy(temp.path())).await.expect("apply");
        assert!(result.contains("2 files"));
        assert_eq!(
            tokio::fs::read_to_string(temp.path().join("a.txt")).await.unwrap(),
            "new\n"
        );
        assert_eq!(
            tokio::fs::read_to_string(temp.path().join("b.txt")).await.unwrap(),
            "added\n"
        );
    }

    #[tokio::test]
    async fn stale_patch_aborts_all_writes() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        tokio::fs::write(temp.path().join("a.txt"), "old\n")
            .await
            .expect("seed a");
        tokio::fs::write(temp.path().join("b.txt"), "keep\n")
            .await
            .expect("seed b");
        let plan = parse_unified_diff(
            "--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-old\n+new\n--- a/b.txt\n+++ b/b.txt\n@@ -1 +1 @@\n-stale\n+changed\n",
        )
        .expect("plan");
        let err = execute_plan(&plan, &policy(temp.path())).await.expect_err("stale");
        assert!(matches!(err, DiffApplyError::Stale(_)));
        assert_eq!(
            tokio::fs::read_to_string(temp.path().join("a.txt")).await.unwrap(),
            "old\n"
        );
        assert_eq!(
            tokio::fs::read_to_string(temp.path().join("b.txt")).await.unwrap(),
            "keep\n"
        );
    }

    #[tokio::test]
    async fn path_negatives_are_rejected_unchanged() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        tokio::fs::write(temp.path().join("safe.txt"), "safe\n")
            .await
            .expect("seed");
        let cases = [
            diff_for("/etc/passwd", "x", "y"),
            diff_for("../escape.txt", "x", "y"),
            diff_for("MEMORY.md", "x", "y"),
            diff_for("memory/topic.md", "x", "y"),
        ];
        for diff in cases {
            let parsed = parse_unified_diff(&diff);
            if let Ok(plan) = parsed {
                assert!(execute_plan(&plan, &policy(temp.path())).await.is_err());
            }
            assert_eq!(
                tokio::fs::read_to_string(temp.path().join("safe.txt")).await.unwrap(),
                "safe\n"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_target_is_rejected_unchanged() {
        use std::os::unix::fs as unix_fs;

        let temp = tempfile::TempDir::new().expect("tempdir");
        let outside = tempfile::TempDir::new().expect("outside");
        tokio::fs::write(outside.path().join("target.txt"), "old\n")
            .await
            .expect("outside seed");
        unix_fs::symlink(outside.path().join("target.txt"), temp.path().join("link.txt")).expect("symlink");
        let plan = parse_unified_diff(&diff_for("link.txt", "old", "new")).expect("plan");
        let err = execute_plan(&plan, &policy(temp.path())).await.expect_err("symlink");
        assert!(matches!(err, DiffApplyError::RejectedPath(_)));
        assert_eq!(
            tokio::fs::read_to_string(outside.path().join("target.txt"))
                .await
                .unwrap(),
            "old\n"
        );
    }

    #[tokio::test]
    async fn directory_target_is_rejected() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        tokio::fs::create_dir(temp.path().join("dir.txt")).await.expect("dir");
        let plan = parse_unified_diff(&diff_for("dir.txt", "old", "new")).expect("plan");
        let err = execute_plan(&plan, &policy(temp.path())).await.expect_err("dir");
        assert!(matches!(err, DiffApplyError::RejectedPath(_)));
    }
}
