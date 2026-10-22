use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::security::SideEffectGate;
use crate::security::git_launch::{self, GitScope};
use crate::security::policy::{ApprovalGrant, ResourceRiskLevel};
use crate::security::{AutonomyLevel, SecurityPolicy};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

/// Git operations tool for structured repository management.
/// Provides safe, parsed git operations with JSON output.
pub struct GitOperationsTool {
    security: Arc<SecurityPolicy>,
    workspace_dir: std::path::PathBuf,
}

impl GitOperationsTool {
    pub const fn new(security: Arc<SecurityPolicy>, workspace_dir: std::path::PathBuf) -> Self {
        Self {
            security,
            workspace_dir,
        }
    }

    /// Sanitize git arguments to prevent injection attacks
    fn sanitize_git_args(&self, args: &str) -> anyhow::Result<Vec<String>> {
        let mut result = Vec::new();
        for arg in args.split_whitespace() {
            // Block dangerous git options that could lead to command injection
            let arg_lower = arg.to_lowercase();
            if arg_lower.starts_with("--exec=")
                || arg_lower.starts_with("--upload-pack=")
                || arg_lower.starts_with("--receive-pack=")
                || arg_lower.starts_with("--pager=")
                || arg_lower.starts_with("--editor=")
                || arg_lower == "--no-verify"
                || arg_lower.contains("$(")
                || arg_lower.contains('`')
                || arg.contains('|')
                || arg.contains(';')
                || arg.contains('>')
            {
                anyhow::bail!("Blocked potentially dangerous git argument: {arg}");
            }
            // Block `-c` config injection (exact match or `-c=...` prefix).
            // This must not false-positive on `--cached` or `-cached`.
            if arg_lower == "-c" || arg_lower.starts_with("-c=") {
                anyhow::bail!("Blocked potentially dangerous git argument: {arg}");
            }
            result.push(arg.to_string());
        }
        Ok(result)
    }

    /// Check if an operation requires write access
    fn requires_write_access(&self, operation: &str) -> bool {
        matches!(
            operation,
            "commit" | "add" | "checkout" | "stash" | "push" | "reset_hard" | "reset" | "revert"
        )
    }

    fn resource_operation_name(&self, operation: &str, args: &serde_json::Value) -> Option<String> {
        match operation {
            "commit" => Some("git_operations:commit".to_string()),
            "add" => Some("git_operations:add".to_string()),
            "checkout" => {
                let branch = args.get("branch").and_then(|value| value.as_str()).unwrap_or("*");
                Some(format!("git_operations:checkout:{branch}"))
            }
            "stash" => {
                let action = args.get("action").and_then(|value| value.as_str()).unwrap_or("push");
                Some(format!("git_operations:stash:{action}"))
            }
            "push" => {
                let remote = args.get("remote").and_then(|value| value.as_str()).unwrap_or("origin");
                Some(format!("git_operations:push:{remote}"))
            }
            "reset_hard" => Some("git_operations:reset_hard".to_string()),
            "reset" => Some("git_operations:reset".to_string()),
            "revert" => Some("git_operations:revert".to_string()),
            _ => None,
        }
    }

    /// Check if an operation is read-only
    #[cfg(test)]
    fn is_read_only(&self, operation: &str) -> bool {
        matches!(operation, "status" | "diff" | "log" | "show" | "branch" | "rev-parse")
    }

    /// Run `git` in the workspace through the hardened launch (GIT-ENV,
    /// D-100): a cleared environment, and every program the repository's own
    /// configuration names (fsmonitor, hooks, filters, credential helpers,
    /// ...) neutralised, while the operator's global configuration applies.
    async fn run_git_command(&self, args: &[&str]) -> anyhow::Result<String> {
        let output = self.run_git_output(args).await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Git command failed: {stderr}");
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// [`Self::run_git_command`] without judging the exit status.
    async fn run_git_output(&self, args: &[&str]) -> anyhow::Result<std::process::Output> {
        // `git` forks its own helpers (git-remote-https, credential helpers),
        // so cancelling this future must tear down the whole process group.
        let mut command = git_launch::git_command(GitScope::Repository(&self.workspace_dir)).await?;
        command.args(args);
        Ok(crate::runtime::shell_process::run_managed_output(command).await?)
    }

    async fn git_status(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let output = self
            .run_git_command(&["status", "--porcelain=v2", "--branch", "-z"])
            .await?;
        let PorcelainStatus {
            branch,
            staged,
            unstaged,
            untracked,
            conflicted,
        } = parse_porcelain_v2(&output);

        let mut result = serde_json::Map::new();
        result.insert("branch".to_string(), json!(branch));
        result.insert("staged".to_string(), json!(staged));
        result.insert("unstaged".to_string(), json!(unstaged));
        result.insert("untracked".to_string(), json!(untracked));
        result.insert("conflicted".to_string(), json!(conflicted));
        result.insert(
            "clean".to_string(),
            json!(staged.is_empty() && unstaged.is_empty() && untracked.is_empty() && conflicted.is_empty()),
        );

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&result).unwrap_or_default(),
            error: None,
        })
    }

    async fn git_diff(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let files = args.get("files").and_then(|v| v.as_str()).unwrap_or(".");
        let cached = args.get("cached").and_then(|v| v.as_bool()).unwrap_or(false);

        // Validate files argument against injection patterns
        self.sanitize_git_args(files)?;

        let mut git_args = vec!["diff", "--unified=3"];
        git_args.extend(git_launch::DIFF_SAFETY_ARGS);
        if cached {
            git_args.push("--cached");
        }
        git_args.push("--");
        git_args.push(files);

        let output = self.run_git_command(&git_args).await?;

        // Parse diff into structured hunks
        let mut result = serde_json::Map::new();
        let mut hunks = Vec::new();
        let mut current_file = String::new();
        let mut current_hunk = serde_json::Map::new();
        let mut lines = Vec::new();

        for line in output.lines() {
            if line.starts_with("diff --git ") {
                if !lines.is_empty() {
                    current_hunk.insert("lines".to_string(), json!(lines));
                    if !current_hunk.is_empty() {
                        hunks.push(serde_json::Value::Object(current_hunk.clone()));
                    }
                    lines = Vec::new();
                    current_hunk = serde_json::Map::new();
                }
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 4 {
                    // SAFETY: bounds checked above (len >= 4)
                    #[allow(clippy::indexing_slicing)]
                    let p3 = parts[3];
                    current_file = p3.trim_start_matches("b/").to_string();
                    current_hunk.insert("file".to_string(), json!(current_file));
                }
            } else if line.starts_with("@@ ") {
                if !lines.is_empty() {
                    current_hunk.insert("lines".to_string(), json!(lines));
                    if !current_hunk.is_empty() {
                        hunks.push(serde_json::Value::Object(current_hunk.clone()));
                    }
                    lines = Vec::new();
                    current_hunk = serde_json::Map::new();
                    current_hunk.insert("file".to_string(), json!(current_file));
                }
                current_hunk.insert("header".to_string(), json!(line));
            } else if !line.is_empty() {
                lines.push(json!({
                    "text": line,
                    "type": if line.starts_with('+') { "add" }
                           else if line.starts_with('-') { "delete" }
                           else { "context" }
                }));
            }
        }

        if !lines.is_empty() {
            current_hunk.insert("lines".to_string(), json!(lines));
            if !current_hunk.is_empty() {
                hunks.push(serde_json::Value::Object(current_hunk));
            }
        }

        result.insert("hunks".to_string(), json!(hunks));
        result.insert("file_count".to_string(), json!(hunks.len()));

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&result).unwrap_or_default(),
            error: None,
        })
    }

    async fn git_log(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let limit_raw = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10);
        let limit = usize::try_from(limit_raw).unwrap_or(usize::MAX).min(1000);
        let limit_str = limit.to_string();

        let output = self
            .run_git_command(&[
                "log",
                &format!("-{limit_str}"),
                "--pretty=format:%H|%an|%ae|%ad|%s",
                "--date=iso",
            ])
            .await?;

        let mut commits = Vec::new();

        for line in output.lines() {
            let parts: Vec<&str> = line.split('|').collect();
            if parts.len() >= 5 {
                // SAFETY: bounds checked above (len >= 5)
                #[allow(clippy::indexing_slicing)]
                commits.push(json!({
                    "hash": parts[0],
                    "author": parts[1],
                    "email": parts[2],
                    "date": parts[3],
                    "message": parts[4]
                }));
            }
        }

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({ "commits": commits })).unwrap_or_default(),
            error: None,
        })
    }

    async fn git_branch(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let output = self
            .run_git_command(&["branch", "--format=%(refname:short)|%(HEAD)"])
            .await?;

        let mut branches = Vec::new();
        let mut current = String::new();

        for line in output.lines() {
            if let Some((name, head)) = line.split_once('|') {
                let is_current = head == "*";
                if is_current {
                    current = name.to_string();
                }
                branches.push(json!({
                    "name": name,
                    "current": is_current
                }));
            }
        }

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "current": current,
                "branches": branches
            }))
            .unwrap_or_default(),
            error: None,
        })
    }

    fn truncate_commit_message(message: &str) -> String {
        if message.chars().count() > 2000 {
            format!("{}...", message.chars().take(1997).collect::<String>())
        } else {
            message.to_string()
        }
    }

    async fn git_commit(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let message = args
            .get("message")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'message' parameter"))?;

        // Sanitize commit message
        let sanitized = message
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("\n");

        if sanitized.is_empty() {
            anyhow::bail!("Commit message cannot be empty");
        }

        // Limit message length
        let message = Self::truncate_commit_message(&sanitized);

        let output = self.run_git_command(&["commit", "-m", &message]).await;

        match output {
            Ok(_) => Ok(ToolResult {
                success: true,
                output: format!("Committed: {message}"),
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Commit failed: {e}")),
            }),
        }
    }

    async fn git_add(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let paths = args
            .get("paths")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'paths' parameter"))?;

        // Validate paths against injection patterns
        self.sanitize_git_args(paths)?;

        let output = self.run_git_command(&["add", "--", paths]).await;

        match output {
            Ok(_) => Ok(ToolResult {
                success: true,
                output: format!("Staged: {paths}"),
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Add failed: {e}")),
            }),
        }
    }

    /// The validated target of a `checkout`.
    fn checkout_target(&self, args: &serde_json::Value) -> anyhow::Result<String> {
        let branch = args
            .get("branch")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'branch' parameter"))?;

        // Sanitize branch name
        let mut sanitized = self.sanitize_git_args(branch)?;
        if sanitized.len() != 1 {
            anyhow::bail!("Invalid branch specification");
        }
        let branch_name = sanitized
            .pop()
            .ok_or_else(|| anyhow::anyhow!("Invalid branch specification"))?;

        // Block dangerous branch names
        if branch_name.contains('@') || branch_name.contains('^') || branch_name.contains('~') {
            anyhow::bail!("Branch name contains invalid characters");
        }
        Ok(branch_name)
    }

    /// Whether `operation` would change a workspace launch definition
    /// (`hooks.json` or `mcp.json` at the workspace root, in any letter
    /// case) in the working tree: a checkout of a revision whose copy
    /// differs, a stash push that reverts a modified one, or a stash pop
    /// that brings one back (GIT-ENV). Such an operation decides which
    /// programs PRX runs, so it is high risk like the file tools' write of
    /// one. The check reads the repository through the same hardened
    /// launch, and fails closed: a check that cannot run (an unknown
    /// revision, a path given as the branch, no stash entry) counts as a
    /// change.
    async fn changes_launch_definition(&self, operation: &str, args: &serde_json::Value) -> bool {
        let checked = match operation {
            "checkout" => match self.checkout_target(args) {
                // `-` is the previous branch; any other leading dash would be
                // read as an option of the check itself.
                Ok(target) if target == "-" => self.revision_changes_launch_definition("@{-1}").await,
                Ok(target) if !target.starts_with('-') => self.revision_changes_launch_definition(&target).await,
                Ok(_) => Err(anyhow::anyhow!("a checkout target starting with '-'")),
                Err(error) => Err(error),
            },
            "stash" => match args.get("action").and_then(|value| value.as_str()).unwrap_or("push") {
                "push" | "save" => {
                    self.launch_definition_paths(&["status", "--porcelain", "-z", "--untracked-files=no"])
                        .await
                }
                "pop" => self.stash_pop_changes_launch_definition().await,
                _ => Ok(false),
            },
            _ => Ok(false),
        };
        checked.unwrap_or_else(|error| {
            tracing::warn!(
                operation,
                error = %error,
                "could not tell whether a git operation changes hooks.json or mcp.json; treating it as a change"
            );
            true
        })
    }

    /// Whether the read-only git command `args`, limited to the launch
    /// definition paths, prints any path.
    async fn launch_definition_paths(&self, args: &[&str]) -> anyhow::Result<bool> {
        let mut full = args.to_vec();
        full.push("--");
        full.extend(git_launch::LAUNCH_DEFINITION_PATHSPECS);
        let output = self.run_git_command(&full).await?;
        Ok(!output
            .trim_matches(|character: char| character == '\0' || character.is_whitespace())
            .is_empty())
    }

    async fn revision_changes_launch_definition(&self, target: &str) -> anyhow::Result<bool> {
        let mut args = vec!["diff"];
        args.extend(git_launch::DIFF_SAFETY_ARGS);
        args.extend(["--name-only", "-z", "HEAD", target]);
        self.launch_definition_paths(&args).await
    }

    async fn stash_pop_changes_launch_definition(&self) -> anyhow::Result<bool> {
        let mut tracked = vec!["diff"];
        tracked.extend(git_launch::DIFF_SAFETY_ARGS);
        tracked.extend(["--name-only", "-z", "stash@{0}^1", "stash@{0}"]);
        if self.launch_definition_paths(&tracked).await? {
            return Ok(true);
        }
        // The untracked files of `stash push --include-untracked` are the
        // root commit `stash@{0}^3`, when there is one.
        // Exit 1 with no output is "no such revision"; anything else that is
        // not success (a launch refusal, an unreadable configuration) fails
        // the check, which counts as a change.
        let verify = self
            .run_git_output(&["rev-parse", "-q", "--verify", "stash@{0}^3"])
            .await?;
        if !verify.status.success() {
            if verify.status.code() == Some(1) && verify.stdout.is_empty() {
                return Ok(false);
            }
            anyhow::bail!(
                "could not tell whether the stash has untracked files: {}",
                String::from_utf8_lossy(&verify.stderr)
                    .lines()
                    .next()
                    .unwrap_or("unknown error")
            );
        }
        let mut untracked = vec!["show"];
        untracked.extend(git_launch::DIFF_SAFETY_ARGS);
        untracked.extend(["--name-only", "--format=", "-z", "stash@{0}^3"]);
        self.launch_definition_paths(&untracked).await
    }

    async fn git_checkout(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let branch_name = self.checkout_target(&args)?;
        let output = self.run_git_command(&["checkout", branch_name.as_str()]).await;

        match output {
            Ok(_) => Ok(ToolResult {
                success: true,
                output: format!("Switched to branch: {branch_name}"),
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Checkout failed: {e}")),
            }),
        }
    }

    async fn git_stash(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("push");

        let output = match action {
            "push" | "save" => self.run_git_command(&["stash", "push", "-m", "auto-stash"]).await,
            "pop" => self.run_git_command(&["stash", "pop"]).await,
            "list" => self.run_git_command(&["stash", "list"]).await,
            "drop" => {
                let index_raw = args.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                let index =
                    i32::try_from(index_raw).map_err(|_| anyhow::anyhow!("stash index too large: {index_raw}"))?;
                self.run_git_command(&["stash", "drop", &format!("stash@{{{index}}}")])
                    .await
            }
            _ => anyhow::bail!("Unknown stash action: {action}. Use: push, pop, list, drop"),
        };

        match output {
            Ok(out) => Ok(ToolResult {
                success: true,
                output: out,
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Stash {action} failed: {e}")),
            }),
        }
    }

    async fn git_push(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let remote = args.get("remote").and_then(|v| v.as_str()).unwrap_or("origin");
        let remote_args = self.sanitize_git_args(remote)?;
        if remote_args.len() != 1 {
            anyhow::bail!("Invalid remote specification");
        }
        let remote_arg = remote_args
            .first()
            .ok_or_else(|| anyhow::anyhow!("Invalid remote specification"))?;

        let mut git_args = vec!["push"];
        git_args.extend(git_launch::PUSH_SAFETY_ARGS);
        git_args.push(remote_arg.as_str());
        let branch_holder;
        if let Some(branch) = args.get("branch").and_then(|v| v.as_str()) {
            let branch_args = self.sanitize_git_args(branch)?;
            if branch_args.len() != 1 {
                anyhow::bail!("Invalid branch specification");
            }
            branch_holder = branch_args;
            let branch_arg = branch_holder
                .first()
                .ok_or_else(|| anyhow::anyhow!("Invalid branch specification"))?;
            git_args.push(branch_arg.as_str());
        }

        let output = self.run_git_command(&git_args).await;
        match output {
            Ok(out) => Ok(ToolResult {
                success: true,
                output: out,
                error: None,
            }),
            Err(error) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Push failed: {error}")),
            }),
        }
    }

    async fn git_reset_hard(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let target = args.get("target").and_then(|v| v.as_str()).unwrap_or("HEAD");
        let target_args = self.sanitize_git_args(target)?;
        if target_args.len() != 1 {
            anyhow::bail!("Invalid reset target specification");
        }
        let target_arg = target_args
            .first()
            .ok_or_else(|| anyhow::anyhow!("Invalid reset target specification"))?;

        let output = self.run_git_command(&["reset", "--hard", target_arg.as_str()]).await;
        match output {
            Ok(out) => Ok(ToolResult {
                success: true,
                output: out,
                error: None,
            }),
            Err(error) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Reset hard failed: {error}")),
            }),
        }
    }
}

/// `git status --porcelain=v2 --branch -z`, parsed.
#[derive(Debug, Default)]
struct PorcelainStatus {
    branch: String,
    staged: Vec<serde_json::Value>,
    unstaged: Vec<serde_json::Value>,
    untracked: Vec<String>,
    conflicted: Vec<serde_json::Value>,
}

/// Parse `git status --porcelain=v2 --branch -z`. Records are NUL
/// separated, so a path may hold spaces:
///
/// - `1 XY sub mH mI mW hH hI path` (ordinary change);
/// - `2 XY sub mH mI mW hH hI Xscore path` followed by the original path as
///   its own NUL-separated field (rename or copy);
/// - `u XY sub m1 m2 m3 mW h1 h2 h3 path` (unmerged);
/// - `? path` (untracked); `! path` (ignored) is skipped.
fn parse_porcelain_v2(output: &str) -> PorcelainStatus {
    let mut status = PorcelainStatus::default();
    let mut records = output.split('\0');
    while let Some(record) = records.next() {
        if let Some(head) = record.strip_prefix("# branch.head ") {
            status.branch = head.to_string();
        } else if let Some(rest) = record.strip_prefix("1 ") {
            let fields = rest.splitn(8, ' ').collect::<Vec<_>>();
            if let (Some(xy), Some(path)) = (fields.first(), fields.get(7)) {
                status.record_change(xy, path, None);
            }
        } else if let Some(rest) = record.strip_prefix("2 ") {
            let fields = rest.splitn(9, ' ').collect::<Vec<_>>();
            // The original path is the next NUL-separated field.
            let original = records.next();
            if let (Some(xy), Some(path)) = (fields.first(), fields.get(8)) {
                status.record_change(xy, path, original);
            }
        } else if let Some(rest) = record.strip_prefix("u ") {
            let fields = rest.splitn(10, ' ').collect::<Vec<_>>();
            if let (Some(xy), Some(path)) = (fields.first(), fields.get(9)) {
                status.conflicted.push(json!({"path": path, "status": xy}));
            }
        } else if let Some(path) = record.strip_prefix("? ") {
            status.untracked.push(path.to_string());
        }
    }
    status
}

impl PorcelainStatus {
    /// Record a changed path under its index (`X`) and working-tree (`Y`)
    /// codes; `.` means unchanged.
    fn record_change(&mut self, xy: &str, path: &str, original: Option<&str>) {
        let mut codes = xy.chars();
        let entry = |code: char| {
            let mut entry = json!({"path": path, "status": code});
            if let (Some(original), Some(object)) = (original, entry.as_object_mut()) {
                object.insert("original_path".to_string(), json!(original));
            }
            entry
        };
        if let Some(code) = codes.next().filter(|code| *code != '.') {
            self.staged.push(entry(code));
        }
        if let Some(code) = codes.next().filter(|code| *code != '.') {
            self.unstaged.push(entry(code));
        }
    }
}

impl GitOperationsTool {
    fn execution_schema(&self) -> serde_json::Value {
        crate::tools::schema::with_action_requirements(
            json!({
                "type": "object",
                "properties": {
                    "operation": {
                        "type": "string",
                        "enum": ["status", "diff", "log", "branch", "commit", "add", "checkout", "stash", "push", "reset_hard"],
                        "description": "Git operation to perform"
                    },
                    "message": {
                        "type": "string",
                        "description": "Commit message (for 'commit' operation)"
                    },
                    "paths": {
                        "type": "string",
                        "description": "File paths to stage (for 'add' operation)"
                    },
                    "branch": {
                        "type": "string",
                        "description": "Branch name (for 'checkout' or 'push' operation)"
                    },
                    "remote": {
                        "type": "string",
                        "description": "Remote name (for 'push' operation, default: origin)"
                    },
                    "target": {
                        "type": "string",
                        "description": "Revision target (for 'reset_hard' operation, default: HEAD)"
                    },
                    "files": {
                        "type": "string",
                        "description": "File or path to diff (for 'diff' operation, default: '.')"
                    },
                    "cached": {
                        "type": "boolean",
                        "description": "Show staged changes (for 'diff' operation)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Number of log entries (for 'log' operation, default: 10)"
                    },
                    "action": {
                        "type": "string",
                        "enum": ["push", "pop", "list", "drop"],
                        "description": "Sub-action of the 'stash' operation: 'push' saves the working tree, 'pop' restores the latest entry, 'list' shows the entries, 'drop' deletes one by index"
                    },
                    "index": {
                        "type": "integer",
                        "description": "Stash index (for 'stash' with 'drop' action)"
                    }
                },
                "required": ["operation"]
            }),
            "operation",
            &[
                crate::tools::schema::ActionRequirement {
                    action: "commit",
                    required: &["message"],
                },
                crate::tools::schema::ActionRequirement {
                    action: "add",
                    required: &["paths"],
                },
                crate::tools::schema::ActionRequirement {
                    action: "checkout",
                    required: &["branch"],
                },
            ],
        )
    }
}

#[async_trait]
impl Tool for GitOperationsTool {
    fn name(&self) -> &str {
        "git_operations"
    }

    fn description(&self) -> &str {
        "Perform structured Git operations: 'status', 'diff', 'log', 'branch', 'commit', 'add', 'checkout', 'stash', 'push' and 'reset_hard'. The 'stash' operation takes an action of 'push' (save), 'pop' (restore), 'list' (show entries) or 'drop' (delete one). Provides parsed JSON output and integrates with security policy for autonomy controls."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        crate::tools::schema::publish_flat_schema(self.execution_schema())
    }

    async fn execute(&self, mut args: serde_json::Value) -> anyhow::Result<ToolResult> {
        crate::tools::schema::validate_execution_contract(self.name(), &self.execution_schema(), &mut args)?;
        let operation = match args.get("operation").and_then(|v| v.as_str()) {
            Some(op) => op,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'operation' parameter".into()),
                });
            }
        };
        let approval_grant = ApprovalGrant::from_runtime_args(self.name(), &args);

        // Check if we're in a git repository
        if !self.workspace_dir.join(".git").exists() {
            // Try to find .git in parent directories
            let mut current_dir = self.workspace_dir.as_path();
            let mut found_git = false;
            while let Some(parent) = current_dir.parent() {
                if current_dir.join(".git").exists() {
                    found_git = true;
                    break;
                }
                current_dir = parent;
            }

            if !found_git {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Not in a git repository".into()),
                });
            }
        }

        // Check autonomy level for write operations
        if self.requires_write_access(operation) {
            if !self.security.can_act() {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Action blocked: git write operations require higher autonomy level".into()),
                });
            }

            match self.security.autonomy {
                AutonomyLevel::ReadOnly => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("Action blocked: read-only mode".into()),
                    });
                }
                AutonomyLevel::Supervised | AutonomyLevel::Full => {}
            }

            if let Some(operation_name) = self.resource_operation_name(operation, &args) {
                // An operation that rewrites `hooks.json` or `mcp.json` in the
                // working tree is its own, high-risk operation, so an approval
                // of the ordinary one does not cover it (GIT-ENV).
                let launch_definition = self.changes_launch_definition(operation, &args).await;
                let (operation_name, risk) = if launch_definition {
                    (format!("{operation_name}:launch-definition"), ResourceRiskLevel::High)
                } else {
                    let risk = match operation {
                        "push" | "reset_hard" => ResourceRiskLevel::High,
                        _ => ResourceRiskLevel::Medium,
                    };
                    (operation_name, risk)
                };
                if let Err(error) = SideEffectGate::new(self.security.as_ref()).authorize_resource_operation(
                    self.name(),
                    &operation_name,
                    risk,
                    approval_grant.as_ref(),
                ) {
                    let error = if launch_definition {
                        format!(
                            "{error} (this git operation changes a workspace launch definition: hooks.json and \
                             mcp.json decide which programs run as PRX's user, so changing one is a high-risk \
                             operation of its own)"
                        )
                    } else {
                        error
                    };
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(error),
                    });
                }
            }
        }

        // Execute the requested operation
        match operation {
            "status" => self.git_status(args).await,
            "diff" => self.git_diff(args).await,
            "log" => self.git_log(args).await,
            "branch" => self.git_branch(args).await,
            "commit" => self.git_commit(args).await,
            "add" => self.git_add(args).await,
            "checkout" => self.git_checkout(args).await,
            "stash" => self.git_stash(args).await,
            "push" => self.git_push(args).await,
            "reset_hard" => self.git_reset_hard(args).await,
            _ => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Unknown operation: {operation}")),
            }),
        }
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Standard
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::DevOps]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::SecurityPolicy;
    use tempfile::TempDir;

    fn test_tool(dir: &std::path::Path) -> GitOperationsTool {
        crate::security::git_launch::hermetic_git_for_tests();
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            ..SecurityPolicy::default()
        });
        GitOperationsTool::new(security, dir.to_path_buf())
    }

    /// Initialize a git repository without blocking the test runtime.
    async fn init_git_repo(dir: &std::path::Path) {
        tokio::process::Command::new("git")
            .args(["init"])
            .current_dir(dir)
            .kill_on_drop(true)
            .output()
            .await
            .unwrap();
    }

    /// The shared action contract every action-dispatched tool is held to:
    /// the tool-list description and the `action` parameter's own description
    /// must each name every enumerated action, and no parameter may ship
    /// undescribed.
    #[tokio::test]
    async fn git_operations_action_tool_contract() {
        let tmp = TempDir::new().unwrap();
        let tool = test_tool(tmp.path());
        crate::tools::schema::assert_flat_execution_contract(&tool, &tool.execution_schema()).await;
        crate::tools::schema::action_contract::assert_action_tool_contract(
            tool.name(),
            tool.description(),
            &tool.execution_schema(),
        );
    }

    #[test]
    fn sanitize_git_blocks_injection() {
        let tmp = TempDir::new().unwrap();
        let tool = test_tool(tmp.path());

        // Should block dangerous arguments
        assert!(tool.sanitize_git_args("--exec=rm -rf /").is_err());
        assert!(tool.sanitize_git_args("$(echo pwned)").is_err());
        assert!(tool.sanitize_git_args("`malicious`").is_err());
        assert!(tool.sanitize_git_args("arg | cat").is_err());
        assert!(tool.sanitize_git_args("arg; rm file").is_err());
    }

    #[test]
    fn sanitize_git_blocks_pager_editor_injection() {
        let tmp = TempDir::new().unwrap();
        let tool = test_tool(tmp.path());

        assert!(tool.sanitize_git_args("--pager=less").is_err());
        assert!(tool.sanitize_git_args("--editor=vim").is_err());
    }

    #[test]
    fn sanitize_git_blocks_config_injection() {
        let tmp = TempDir::new().unwrap();
        let tool = test_tool(tmp.path());

        // Exact `-c` flag (config injection)
        assert!(tool.sanitize_git_args("-c core.sshCommand=evil").is_err());
        assert!(tool.sanitize_git_args("-c=core.pager=less").is_err());
    }

    #[test]
    fn sanitize_git_blocks_no_verify() {
        let tmp = TempDir::new().unwrap();
        let tool = test_tool(tmp.path());

        assert!(tool.sanitize_git_args("--no-verify").is_err());
    }

    #[test]
    fn sanitize_git_blocks_redirect_in_args() {
        let tmp = TempDir::new().unwrap();
        let tool = test_tool(tmp.path());

        assert!(tool.sanitize_git_args("file.txt > /tmp/out").is_err());
    }

    #[test]
    fn sanitize_git_cached_not_blocked() {
        let tmp = TempDir::new().unwrap();
        let tool = test_tool(tmp.path());

        // --cached must NOT be blocked by the `-c` check
        assert!(tool.sanitize_git_args("--cached").is_ok());
        // Other safe flags starting with -c prefix
        assert!(tool.sanitize_git_args("-cached").is_ok());
    }

    #[test]
    fn sanitize_git_allows_safe() {
        let tmp = TempDir::new().unwrap();
        let tool = test_tool(tmp.path());

        // Should allow safe arguments
        assert!(tool.sanitize_git_args("main").is_ok());
        assert!(tool.sanitize_git_args("feature/test-branch").is_ok());
        assert!(tool.sanitize_git_args("--cached").is_ok());
        assert!(tool.sanitize_git_args("src/main.rs").is_ok());
        assert!(tool.sanitize_git_args(".").is_ok());
    }

    #[test]
    fn requires_write_detection() {
        let tmp = TempDir::new().unwrap();
        let tool = test_tool(tmp.path());

        assert!(tool.requires_write_access("commit"));
        assert!(tool.requires_write_access("add"));
        assert!(tool.requires_write_access("checkout"));
        assert!(tool.requires_write_access("push"));
        assert!(tool.requires_write_access("reset_hard"));

        assert!(!tool.requires_write_access("status"));
        assert!(!tool.requires_write_access("diff"));
        assert!(!tool.requires_write_access("log"));
    }

    #[test]
    fn resource_operation_names_are_canonical() {
        let tmp = TempDir::new().unwrap();
        let tool = test_tool(tmp.path());

        assert_eq!(
            tool.resource_operation_name("commit", &json!({})).as_deref(),
            Some("git_operations:commit")
        );
        assert_eq!(
            tool.resource_operation_name("checkout", &json!({"branch": "main"}))
                .as_deref(),
            Some("git_operations:checkout:main")
        );
        assert_eq!(
            tool.resource_operation_name("stash", &json!({"action": "drop"}))
                .as_deref(),
            Some("git_operations:stash:drop")
        );
        assert_eq!(
            tool.resource_operation_name("push", &json!({"remote": "origin"}))
                .as_deref(),
            Some("git_operations:push:origin")
        );
        assert_eq!(
            tool.resource_operation_name("reset_hard", &json!({})).as_deref(),
            Some("git_operations:reset_hard")
        );
    }

    #[test]
    fn branch_is_not_write_gated() {
        let tmp = TempDir::new().unwrap();
        let tool = test_tool(tmp.path());

        // Branch listing is read-only; it must not require write access
        assert!(!tool.requires_write_access("branch"));
        assert!(tool.is_read_only("branch"));
    }

    #[test]
    fn is_read_only_detection() {
        let tmp = TempDir::new().unwrap();
        let tool = test_tool(tmp.path());

        assert!(tool.is_read_only("status"));
        assert!(tool.is_read_only("diff"));
        assert!(tool.is_read_only("log"));
        assert!(tool.is_read_only("branch"));

        assert!(!tool.is_read_only("commit"));
        assert!(!tool.is_read_only("add"));
    }

    #[tokio::test]
    async fn blocks_readonly_mode_for_write_ops() {
        let tmp = TempDir::new().unwrap();
        // Initialize a git repository
        init_git_repo(tmp.path()).await;

        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::ReadOnly,
            ..SecurityPolicy::default()
        });
        let tool = GitOperationsTool::new(security, tmp.path().to_path_buf());

        let result = tool
            .execute(json!({"operation": "commit", "message": "test"}))
            .await
            .unwrap();
        assert!(!result.success);
        // can_act() returns false for ReadOnly, so we get the "higher autonomy level" message
        assert!(result.error.as_deref().unwrap_or("").contains("higher autonomy"));
    }

    #[tokio::test]
    async fn supervised_write_ops_require_runtime_grant() {
        let tmp = TempDir::new().unwrap();
        init_git_repo(tmp.path()).await;

        let tool = test_tool(tmp.path());
        let result = tool
            .execute(json!({"operation": "commit", "message": "test"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("runtime approval grant"));
    }

    #[tokio::test]
    async fn supervised_push_and_reset_hard_require_runtime_grant() {
        let tmp = TempDir::new().unwrap();
        init_git_repo(tmp.path()).await;

        let tool = test_tool(tmp.path());
        let push = tool
            .execute(json!({"operation": "push", "remote": "origin"}))
            .await
            .unwrap();
        assert!(!push.success);
        assert!(push.error.as_deref().unwrap_or("").contains("runtime approval grant"));

        let reset = tool
            .execute(json!({"operation": "reset_hard", "target": "HEAD"}))
            .await
            .unwrap();
        assert!(!reset.success);
        assert!(reset.error.as_deref().unwrap_or("").contains("runtime approval grant"));
    }

    #[tokio::test]
    async fn supervised_push_accepts_matching_runtime_grant() {
        let tmp = TempDir::new().unwrap();
        init_git_repo(tmp.path()).await;

        let tool = test_tool(tmp.path());
        let grant = ApprovalGrant::for_resource_operation("git_operations", "git_operations:push:origin", "test", None);
        let result = tool
            .execute(json!({
                "operation": "push",
                "remote": "origin",
                (crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG): serde_json::to_value(grant).unwrap()
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(!result.error.as_deref().unwrap_or("").contains("runtime approval grant"));
    }

    #[tokio::test]
    async fn supervised_write_ops_accept_matching_runtime_grant() {
        let tmp = TempDir::new().unwrap();
        init_git_repo(tmp.path()).await;

        let tool = test_tool(tmp.path());
        let grant = ApprovalGrant::for_resource_operation("git_operations", "git_operations:commit", "test", None);
        let result = tool
            .execute(json!({
                "operation": "commit",
                "message": "test",
                (crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG): serde_json::to_value(grant).unwrap()
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(!result.error.as_deref().unwrap_or("").contains("runtime approval grant"));
    }

    #[tokio::test]
    async fn allows_branch_listing_in_readonly_mode() {
        let tmp = TempDir::new().unwrap();
        // Initialize a git repository so the command can succeed
        init_git_repo(tmp.path()).await;

        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::ReadOnly,
            ..SecurityPolicy::default()
        });
        let tool = GitOperationsTool::new(security, tmp.path().to_path_buf());

        let result = tool.execute(json!({"operation": "branch"})).await.unwrap();
        // Branch listing must not be blocked by read-only autonomy
        let error_msg = result.error.as_deref().unwrap_or("");
        assert!(
            !error_msg.contains("read-only") && !error_msg.contains("higher autonomy"),
            "branch listing should not be blocked in read-only mode, got: {error_msg}"
        );
    }

    #[tokio::test]
    async fn allows_readonly_ops_in_readonly_mode() {
        let tmp = TempDir::new().unwrap();
        init_git_repo(tmp.path()).await;

        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::ReadOnly,
            ..SecurityPolicy::default()
        });
        let tool = GitOperationsTool::new(security, tmp.path().to_path_buf());

        let result = tool.execute(json!({"operation": "status"})).await.unwrap();
        assert!(
            result.success,
            "read-only git status should be allowed: {:?}",
            result.error
        );
    }

    #[tokio::test]
    async fn rejects_missing_operation() {
        let tmp = TempDir::new().unwrap();
        let tool = test_tool(tmp.path());

        let error = tool.execute(json!({})).await.unwrap_err();
        let typed = error.downcast_ref::<crate::tools::schema::ToolArgumentError>().unwrap();
        assert!(typed.issues.iter().any(|issue| issue.path.contains("operation")));
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
        init_git_repo(tmp.path()).await;
        let status = tool.execute(json!({"operation":"status"})).await.unwrap();
        assert!(status.success, "{:?}", status.error);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&status.output)
                .unwrap()
                .get("clean")
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
    }

    #[tokio::test]
    async fn rejects_unknown_operation() {
        let tmp = TempDir::new().unwrap();
        // Initialize a git repository
        init_git_repo(tmp.path()).await;

        let tool = test_tool(tmp.path());

        let error = tool.execute(json!({"operation": "fetch_all"})).await.unwrap_err();
        let typed = error.downcast_ref::<crate::tools::schema::ToolArgumentError>().unwrap();
        assert!(typed.issues.iter().any(|issue| issue.path.contains("operation")));
        let status = tool.execute(json!({"operation":"status"})).await.unwrap();
        assert!(status.success, "{:?}", status.error);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&status.output)
                .unwrap()
                .get("clean")
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
    }

    // ── GIT-ENV (D-098): a repository's configuration is a launch definition ──

    /// A provider-key stand-in in PRX's own environment. No program a
    /// repository configures may see it (and none may run at all).
    #[cfg(unix)]
    const PARENT_SENTINEL_NAME: &str = "PRX_GIT_ENV_TEST_PROVIDER_KEY";
    #[cfg(unix)]
    const PARENT_SENTINEL_VALUE: &str = "git-env-parent-sentinel-5d1c";

    #[cfg(unix)]
    #[allow(unsafe_code)]
    fn plant_parent_sentinel() {
        static PLANTED: std::sync::Once = std::sync::Once::new();
        PLANTED.call_once(|| {
            // SAFETY: test-only; set once, to a name no other code reads,
            // before any git process of these tests is started.
            unsafe { std::env::set_var(PARENT_SENTINEL_NAME, PARENT_SENTINEL_VALUE) };
        });
    }

    #[cfg(unix)]
    fn full_tool(dir: &std::path::Path) -> GitOperationsTool {
        crate::security::git_launch::hermetic_git_for_tests();
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            ..SecurityPolicy::default()
        });
        GitOperationsTool::new(security, dir.to_path_buf())
    }

    /// Test-side git (repository setup), itself kept away from the
    /// developer's configuration and from what the test plants.
    async fn setup_git(dir: &std::path::Path, args: &[&str]) {
        let output = tokio::process::Command::new("git")
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "commit.gpgSign=false",
            ])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .kill_on_drop(true)
            .output()
            .await
            .expect("test: git runs");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// [`setup_git`] for a command expected to fail (a conflicting merge).
    async fn setup_git_may_fail(dir: &std::path::Path, args: &[&str]) {
        let _ = tokio::process::Command::new("git")
            .args(["-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false"])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .kill_on_drop(true)
            .output()
            .await
            .expect("test: git runs");
    }

    /// A repository at `dir` with an identity and one commit of `files`.
    async fn committed_repo(dir: &std::path::Path, files: &[(&str, &str)]) {
        std::fs::create_dir_all(dir).expect("test: workspace");
        setup_git(dir, &["init", "-q"]).await;
        setup_git(dir, &["checkout", "-q", "-b", "main"]).await;
        setup_git(dir, &["config", "user.name", "PRX Test"]).await;
        setup_git(dir, &["config", "user.email", "prx@example.invalid"]).await;
        for (name, content) in files {
            std::fs::write(dir.join(name), content).expect("test: write file");
        }
        setup_git(dir, &["add", "-A"]).await;
        setup_git(dir, &["commit", "-q", "-m", "one"]).await;
    }

    /// An executable script that dumps its environment to `marker` (and,
    /// for a filter, passes its input through).
    #[cfg(unix)]
    fn plant_script(path: &std::path::Path, marker: &std::path::Path, pass_through: bool) -> String {
        use std::os::unix::fs::PermissionsExt as _;
        let body = format!(
            "#!/bin/sh\nenv > '{}'\n{}",
            marker.display(),
            if pass_through { "cat\n" } else { "" }
        );
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("test: script directory");
        }
        std::fs::write(path, body).expect("test: write script");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("test: chmod");
        path.display().to_string()
    }

    /// The planted program never ran, and so never saw PRX's environment.
    #[cfg(unix)]
    fn assert_not_run(marker: &std::path::Path, what: &str) {
        let dump = std::fs::read_to_string(marker).ok();
        assert!(
            dump.as_deref()
                .is_none_or(|environment| !environment.contains(PARENT_SENTINEL_VALUE)),
            "{what} ran with PRX's environment: the parent sentinel reached it"
        );
        assert!(dump.is_none(), "{what} ran (with a cleared environment)");
    }

    async fn run(tool: &GitOperationsTool, args: serde_json::Value) -> ToolResult {
        // D-103: the tool's git reads a scratch global file, no system file.
        crate::security::git_launch::hermetic_git_for_tests();
        tokio::time::timeout(std::time::Duration::from_mins(1), tool.execute(args))
            .await
            .expect("test: git operation finished")
            .expect("test: git operation executes")
    }

    fn granted(operation_name: &str, mut args: serde_json::Value) -> serde_json::Value {
        let grant = ApprovalGrant::for_resource_operation("git_operations", operation_name, "test", None);
        if let Some(object) = args.as_object_mut() {
            object.insert(
                crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG.to_string(),
                serde_json::to_value(grant).expect("test: grant serialises"),
            );
        }
        args
    }

    /// RED before GIT-ENV (R-HOOKSENV F-2): a workspace `.git/config` with
    /// `core.fsmonitor` made a plain `status`, at supervised autonomy and
    /// with no approval, run that command with PRX's full environment. It
    /// must not run at all, whether set directly or through `include.path`.
    #[cfg(unix)]
    #[tokio::test]
    async fn status_does_not_run_a_workspace_fsmonitor() {
        plant_parent_sentinel();
        let tmp = TempDir::new().expect("test: temp dir");
        let workspace = tmp.path().join("ws");
        committed_repo(&workspace, &[("a.txt", "a\n")]).await;
        let marker = tmp.path().join("fsmonitor-ran");
        let script = plant_script(&tmp.path().join("bin/fsmonitor.sh"), &marker, false);
        setup_git(&workspace, &["config", "core.fsmonitor", &script]).await;
        std::fs::write(workspace.join("a.txt"), "changed\n").expect("test: modify");

        let tool = test_tool(&workspace);
        let result = run(&tool, json!({"operation": "status"})).await;
        assert!(result.success, "{:?}", result.error);
        assert!(result.output.contains("\"clean\": false"), "{}", result.output);
        assert!(result.output.contains("\"path\": \"a.txt\""), "{}", result.output);
        assert_not_run(&marker, "core.fsmonitor");

        // The same setting pulled in from an included file.
        setup_git(&workspace, &["config", "--unset", "core.fsmonitor"]).await;
        let included_marker = tmp.path().join("included-fsmonitor-ran");
        let included_script = plant_script(&tmp.path().join("bin/included.sh"), &included_marker, false);
        let included = tmp.path().join("included.cfg");
        std::fs::write(&included, format!("[core]\n\tfsmonitor = {included_script}\n")).expect("test: include");
        setup_git(&workspace, &["config", "include.path", &included.display().to_string()]).await;
        let result = run(&tool, json!({"operation": "diff"})).await;
        assert!(result.success, "{:?}", result.error);
        assert_not_run(&included_marker, "an included core.fsmonitor");
    }

    /// RED before GIT-ENV: `commit` ran the hooks of `.git/hooks` (and of a
    /// repository-chosen `core.hooksPath`) with PRX's environment.
    #[cfg(unix)]
    #[tokio::test]
    async fn commit_does_not_run_repository_hooks() {
        plant_parent_sentinel();
        let tmp = TempDir::new().expect("test: temp dir");
        let workspace = tmp.path().join("ws");
        committed_repo(&workspace, &[("a.txt", "a\n")]).await;
        let marker = tmp.path().join("hook-ran");
        for hook in [
            "pre-commit",
            "prepare-commit-msg",
            "commit-msg",
            "post-commit",
            "reference-transaction",
        ] {
            plant_script(&workspace.join(".git/hooks").join(hook), &marker, false);
        }
        let chosen_marker = tmp.path().join("chosen-hook-ran");
        plant_script(&workspace.join(".githooks/pre-commit"), &chosen_marker, false);

        let tool = full_tool(&workspace);
        std::fs::write(workspace.join("b.txt"), "b\n").expect("test: new file");
        let add = run(&tool, json!({"operation": "add", "paths": "b.txt"})).await;
        assert!(add.success, "{:?}", add.error);
        let commit = run(&tool, json!({"operation": "commit", "message": "two"})).await;
        assert!(commit.success, "{:?}", commit.error);
        assert_not_run(&marker, "a .git/hooks hook");

        setup_git(&workspace, &["config", "core.hooksPath", ".githooks"]).await;
        std::fs::write(workspace.join("b.txt"), "b2\n").expect("test: modify");
        let add = run(&tool, json!({"operation": "add", "paths": "b.txt"})).await;
        assert!(add.success, "{:?}", add.error);
        let commit = run(&tool, json!({"operation": "commit", "message": "three"})).await;
        assert!(commit.success, "{:?}", commit.error);
        assert_not_run(&chosen_marker, "a core.hooksPath hook");

        // The commits landed, under the repository's own identity.
        let log = run(&tool, json!({"operation": "log", "limit": 3})).await;
        assert!(log.success, "{:?}", log.error);
        assert!(
            log.output.contains("three") && log.output.contains("two"),
            "{}",
            log.output
        );
        assert!(log.output.contains("prx@example.invalid"), "{}", log.output);
    }

    /// RED before GIT-ENV: `add` ran a filter driver's `clean` command (and
    /// stash pop its `smudge`) named by `.gitattributes` and defined in
    /// `.git/config`. The driver is neutralised; the content passes through.
    #[cfg(unix)]
    #[tokio::test]
    async fn add_stash_and_diff_do_not_run_filter_or_diff_drivers() {
        plant_parent_sentinel();
        let tmp = TempDir::new().expect("test: temp dir");
        let workspace = tmp.path().join("ws");
        committed_repo(
            &workspace,
            &[
                ("a.txt", "a\n"),
                (".gitattributes", "*.txt filter=ev diff=ev merge=ev\n"),
            ],
        )
        .await;
        let marker = tmp.path().join("driver-ran");
        let filter = plant_script(&tmp.path().join("bin/filter.sh"), &marker, true);
        for (key, value) in [
            ("filter.ev.clean", filter.as_str()),
            ("filter.ev.smudge", filter.as_str()),
            ("filter.ev.required", "true"),
            ("diff.ev.textconv", filter.as_str()),
            ("diff.ev.command", filter.as_str()),
            ("diff.external", filter.as_str()),
            ("merge.ev.driver", filter.as_str()),
        ] {
            setup_git(&workspace, &["config", key, value]).await;
        }

        let tool = full_tool(&workspace);
        std::fs::write(workspace.join("a.txt"), "a2\n").expect("test: modify");
        let diff = run(&tool, json!({"operation": "diff"})).await;
        assert!(diff.success, "{:?}", diff.error);
        assert!(diff.output.contains("a2"), "{}", diff.output);
        let add = run(&tool, json!({"operation": "add", "paths": "a.txt"})).await;
        assert!(add.success, "{:?}", add.error);
        let commit = run(&tool, json!({"operation": "commit", "message": "two"})).await;
        assert!(commit.success, "{:?}", commit.error);
        std::fs::write(workspace.join("a.txt"), "a3\n").expect("test: modify");
        let push = run(&tool, json!({"operation": "stash", "action": "push"})).await;
        assert!(push.success, "{:?}", push.error);
        let pop = run(&tool, json!({"operation": "stash", "action": "pop"})).await;
        assert!(pop.success, "{:?}", pop.error);
        assert_eq!(
            std::fs::read_to_string(workspace.join("a.txt")).expect("test: read"),
            "a3\n"
        );
        assert_not_run(&marker, "a filter or diff driver");
    }

    /// RED before GIT-ENV: an authenticated transfer ran the repository's
    /// `credential.helper` with PRX's environment. A loopback server that
    /// answers every request with 401 makes git ask for credentials.
    #[cfg(unix)]
    #[tokio::test]
    async fn push_does_not_run_a_repository_credential_helper() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        plant_parent_sentinel();
        let tmp = TempDir::new().expect("test: temp dir");
        let workspace = tmp.path().join("ws");
        committed_repo(&workspace, &[("a.txt", "a\n")]).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("test: bind");
        let port = listener.local_addr().expect("test: address").port();
        let server = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = vec![0_u8; 8192];
                let _ = socket.read(&mut request).await;
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"prx\"\r\n\
                          Content-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
            }
        });
        let marker = tmp.path().join("credential-helper-ran");
        let helper = plant_script(&tmp.path().join("bin/credential.sh"), &marker, false);
        let url = format!("http://127.0.0.1:{port}/r.git");
        setup_git(&workspace, &["remote", "add", "origin", &url]).await;
        setup_git(&workspace, &["config", "http.proxy", ""]).await;
        setup_git(&workspace, &["config", "credential.helper", &helper]).await;
        setup_git(&workspace, &["config", &format!("credential.{url}.helper"), &helper]).await;

        let tool = full_tool(&workspace);
        let push = run(
            &tool,
            json!({"operation": "push", "remote": "origin", "branch": "main"}),
        )
        .await;
        server.abort();
        assert!(!push.success, "the push to a 401 server cannot succeed");
        assert_not_run(&marker, "credential.helper");
    }

    /// RED before GIT-ENV (R-HOOKSENV I-3): a checkout that rewrites the
    /// workspace `hooks.json` was an ordinary, medium-risk checkout, so its
    /// ordinary approval covered it. It is now a high-risk operation of its
    /// own; a checkout that leaves the launch definitions alone keeps its
    /// ordinary approval.
    #[tokio::test]
    async fn a_checkout_that_changes_hooks_json_needs_its_own_approval() {
        let tmp = TempDir::new().expect("test: temp dir");
        let workspace = tmp.path().join("ws");
        committed_repo(&workspace, &[("a.txt", "a\n"), ("hooks.json", "{\"v\":1}\n")]).await;
        setup_git(&workspace, &["checkout", "-q", "-b", "other"]).await;
        std::fs::write(workspace.join("hooks.json"), "{\"v\":2}\n").expect("test: hooks v2");
        setup_git(&workspace, &["commit", "-q", "-am", "hooks v2"]).await;
        setup_git(&workspace, &["checkout", "-q", "main"]).await;
        setup_git(&workspace, &["checkout", "-q", "-b", "same"]).await;
        std::fs::write(workspace.join("a.txt"), "a2\n").expect("test: a2");
        setup_git(&workspace, &["commit", "-q", "-am", "a2"]).await;
        setup_git(&workspace, &["checkout", "-q", "main"]).await;

        let tool = test_tool(&workspace);
        let result = run(
            &tool,
            granted(
                "git_operations:checkout:other",
                json!({"operation": "checkout", "branch": "other"}),
            ),
        )
        .await;
        assert!(
            !result.success,
            "an ordinary checkout approval covered a hooks.json change"
        );
        let error = result.error.unwrap_or_default();
        assert!(error.contains("workspace launch definition"), "{error}");
        assert_eq!(
            std::fs::read_to_string(workspace.join("hooks.json")).expect("test: read"),
            "{\"v\":1}\n"
        );

        let result = run(
            &tool,
            granted(
                "git_operations:checkout:same",
                json!({"operation": "checkout", "branch": "same"}),
            ),
        )
        .await;
        assert!(result.success, "{:?}", result.error);
        assert_eq!(
            std::fs::read_to_string(workspace.join("a.txt")).expect("test: read"),
            "a2\n"
        );
    }

    /// RED before GIT-ENV: `stash pop` bringing back a changed `mcp.json`,
    /// and `stash push` reverting a changed `hooks.json`, were ordinary
    /// stash operations.
    #[tokio::test]
    async fn a_stash_that_changes_a_launch_definition_needs_its_own_approval() {
        let tmp = TempDir::new().expect("test: temp dir");
        let workspace = tmp.path().join("ws");
        committed_repo(&workspace, &[("hooks.json", "{\"v\":1}\n"), ("mcp.json", "{}\n")]).await;
        std::fs::write(workspace.join("mcp.json"), "{\"servers\":{}}\n").expect("test: mcp v2");
        setup_git(&workspace, &["stash", "push", "-q"]).await;
        std::fs::write(workspace.join("hooks.json"), "{\"v\":2}\n").expect("test: hooks v2");

        let tool = test_tool(&workspace);
        let push = run(
            &tool,
            granted(
                "git_operations:stash:push",
                json!({"operation": "stash", "action": "push"}),
            ),
        )
        .await;
        assert!(!push.success, "an ordinary stash approval covered reverting hooks.json");
        assert!(
            push.error.unwrap_or_default().contains("workspace launch definition"),
            "the refusal names the launch definition"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("hooks.json")).expect("test: read"),
            "{\"v\":2}\n"
        );

        setup_git(&workspace, &["checkout", "-q", "--", "hooks.json"]).await;
        let pop = run(
            &tool,
            granted(
                "git_operations:stash:pop",
                json!({"operation": "stash", "action": "pop"}),
            ),
        )
        .await;
        assert!(!pop.success, "an ordinary stash approval covered restoring mcp.json");
        assert!(
            pop.error.unwrap_or_default().contains("workspace launch definition"),
            "the refusal names the launch definition"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("mcp.json")).expect("test: read"),
            "{}\n"
        );
    }

    /// R-GITENV F-1: a push to a local-path remote whose
    /// `remote.origin.receivepack` names a program (first-wins, so no `-c`
    /// replaces it) runs git's own receive-pack.
    #[cfg(unix)]
    #[tokio::test]
    async fn push_does_not_run_a_repository_receive_pack() {
        plant_parent_sentinel();
        let tmp = TempDir::new().expect("test: temp dir");
        let workspace = tmp.path().join("ws");
        committed_repo(&workspace, &[("a.txt", "a\n")]).await;
        let remote = tmp.path().join("remote.git");
        std::fs::create_dir_all(&remote).expect("test: remote");
        setup_git(&remote, &["init", "-q", "--bare"]).await;
        let marker = tmp.path().join("receive-pack-ran");
        let program = plant_script(&tmp.path().join("bin/receive.sh"), &marker, false);
        setup_git(&workspace, &["remote", "add", "origin", &remote.display().to_string()]).await;
        setup_git(&workspace, &["config", "remote.origin.receivepack", &program]).await;

        let tool = full_tool(&workspace);
        let push = run(
            &tool,
            json!({"operation": "push", "remote": "origin", "branch": "main"}),
        )
        .await;
        assert!(!push.success, "a repository receive-pack program must refuse the push");
        assert!(
            push.error.as_deref().is_some_and(|error| error.contains("receivepack")),
            "{:?}",
            push.error
        );
        assert_not_run(&marker, "remote.origin.receivepack");
        setup_git(&workspace, &["config", "--unset", "remote.origin.receivepack"]).await;
        let push = run(
            &tool,
            json!({"operation": "push", "remote": "origin", "branch": "main"}),
        )
        .await;
        assert!(push.success, "{:?}", push.error);
        setup_git(&remote, &["rev-parse", "--verify", "refs/heads/main"]).await;
    }

    /// R-GITENV F-11: the porcelain-v2 records, NUL separated.
    #[test]
    fn the_status_parser_reads_every_porcelain_v2_record() {
        let output = [
            "# branch.oid 0123",
            "# branch.head main",
            "1 M. N... 100644 100644 100644 aaaa bbbb staged.txt",
            "1 .M N... 100644 100644 100644 aaaa aaaa with space.txt",
            "2 R. N... 100644 100644 100644 aaaa aaaa R100 new name.txt",
            "old name.txt",
            "u UU N... 100644 100644 100644 100644 aaaa bbbb cccc conflict.txt",
            "? untracked file.txt",
            "! ignored.txt",
            "",
        ]
        .join("\0");
        let status = parse_porcelain_v2(&output);
        assert_eq!(status.branch, "main");
        assert_eq!(
            status.staged,
            vec![
                json!({"path": "staged.txt", "status": "M"}),
                json!({"path": "new name.txt", "status": "R", "original_path": "old name.txt"}),
            ]
        );
        assert_eq!(status.unstaged, vec![json!({"path": "with space.txt", "status": "M"})]);
        assert_eq!(status.conflicted, vec![json!({"path": "conflict.txt", "status": "UU"})]);
        assert_eq!(status.untracked, vec!["untracked file.txt".to_string()]);
    }

    /// R-GITENV F-11 through the tool: staged, unstaged, a space in a path,
    /// a rename and a conflict.
    #[tokio::test]
    async fn status_reports_staged_unstaged_renamed_and_conflicted_paths() {
        let tmp = TempDir::new().expect("test: temp dir");
        let workspace = tmp.path().join("ws");
        committed_repo(
            &workspace,
            &[
                ("a.txt", "a\n"),
                ("b c.txt", "b\n"),
                ("old.txt", "o\n"),
                ("conflict.txt", "base\n"),
            ],
        )
        .await;
        setup_git(&workspace, &["checkout", "-q", "-b", "other"]).await;
        std::fs::write(workspace.join("conflict.txt"), "other\n").expect("test: other side");
        setup_git(&workspace, &["commit", "-q", "-am", "other"]).await;
        setup_git(&workspace, &["checkout", "-q", "main"]).await;
        std::fs::write(workspace.join("conflict.txt"), "main\n").expect("test: main side");
        setup_git(&workspace, &["commit", "-q", "-am", "main"]).await;
        setup_git_may_fail(&workspace, &["merge", "-q", "other"]).await;
        std::fs::write(workspace.join("a.txt"), "a2\n").expect("test: staged");
        setup_git(&workspace, &["add", "a.txt"]).await;
        std::fs::write(workspace.join("b c.txt"), "b2\n").expect("test: unstaged");
        setup_git(&workspace, &["mv", "old.txt", "new.txt"]).await;

        let result = run(&test_tool(&workspace), json!({"operation": "status"})).await;
        assert!(result.success, "{:?}", result.error);
        let status: serde_json::Value = serde_json::from_str(&result.output).expect("test: json");
        let paths = |list: &str| {
            status
                .get(list)
                .and_then(serde_json::Value::as_array)
                .map(|entries| {
                    entries
                        .iter()
                        .filter_map(|entry| entry.get("path").and_then(serde_json::Value::as_str))
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        assert!(paths("staged").contains(&"a.txt".to_string()), "{status}");
        assert!(paths("staged").contains(&"new.txt".to_string()), "{status}");
        assert!(paths("unstaged").contains(&"b c.txt".to_string()), "{status}");
        assert_eq!(paths("conflicted"), vec!["conflict.txt".to_string()], "{status}");
        assert_eq!(status.get("clean"), Some(&json!(false)));
    }

    #[test]
    fn truncates_multibyte_commit_message_without_panicking() {
        let long = "🦀".repeat(2500);
        let truncated = GitOperationsTool::truncate_commit_message(&long);

        assert_eq!(truncated.chars().count(), 2000);
    }
}
