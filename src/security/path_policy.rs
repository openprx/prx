//! Native path resolution shared by policy, approval, and execution.
use super::SecurityPolicy;
use std::path::{Component, Path, PathBuf};

/// Select the native execution root without changing the security workspace.
pub(crate) fn effective_root<'a>(workdir: Option<&'a Path>, security: &'a SecurityPolicy) -> &'a Path {
    workdir.unwrap_or(&security.workspace_dir)
}

/// Resolve a request against its actual execution root without filesystem effects.
/// Canonical containment checks remain the caller's responsibility after resolution.
pub(crate) fn resolve_request_path(root: &Path, request: &str) -> std::io::Result<PathBuf> {
    if request.contains('\0')
        || Path::new(request)
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid file path",
        ));
    }
    Ok(std::path::absolute(root)?.join(crate::config::expand_home(Path::new(request))))
}

/// Resolve a request at Full: absolute paths and `..` are allowed. The path
/// is joined to the root and normalised lexically (a `..` above the
/// filesystem root stays at the root); the operating system decides the
/// rest. A NUL byte is still refused.
pub fn resolve_full_request_path(root: &Path, request: &str) -> std::io::Result<PathBuf> {
    if request.contains('\0') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid file path",
        ));
    }
    let joined = std::path::absolute(root)?.join(crate::config::expand_home(Path::new(request)));
    let mut normalized = PathBuf::new();
    for part in joined.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other),
        }
    }
    Ok(normalized)
}

/// The below-Full confinement of a file_write or file_edit target, run by the
/// tool's precheck and again at execute time. The outer error is a policy
/// refusal; the inner one is a target that does not resolve (reported by
/// execution). Returns the resolved target, whose parent is inside the
/// workspace and which is not a symlink that resolves outside it.
pub(crate) async fn confine_file_target(
    security: &SecurityPolicy,
    root: &Path,
    path: &str,
    allow_missing_parent: bool,
) -> Result<std::io::Result<PathBuf>, PathRefusalReason> {
    security.check_path_at(root, path)?;
    let target = match super::approval_operation::resolve_file_target(root, path, allow_missing_parent).await {
        Ok(target) => target,
        Err(error) => return Ok(Err(error)),
    };
    let Some(parent) = target.parent() else {
        return Ok(Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "file parent missing",
        )));
    };
    if !security.is_resolved_path_allowed(parent) {
        return Err(PathRefusalReason::SymlinkEscape);
    }
    // The write itself refuses any symlink (O_NOFOLLOW); one that resolves
    // outside the workspace is refused here already, so before approval.
    if tokio::fs::symlink_metadata(&target)
        .await
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        // A dangling link has no resolved target to judge; the O_NOFOLLOW
        // open refuses it at execute time.
        if let Ok(resolved) = tokio::fs::canonicalize(&target).await {
            if !security.is_resolved_path_allowed(&resolved) {
                return Err(PathRefusalReason::SymlinkEscape);
            }
        }
    }
    Ok(Ok(target))
}

/// Closed reasons a path is refused below Full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathRefusalReason {
    /// Parent traversal, encoded traversal or a NUL byte.
    InvalidPath,
    /// The path is, or lies under, a configured forbidden path.
    ForbiddenPath,
    /// The path resolves outside the workspace while `workspace_only` is set.
    OutsideWorkspace,
    /// The path is written inside the workspace, but a symlink resolves it outside.
    SymlinkEscape,
    /// A shell path argument is built from a variable or expansion.
    DynamicShellArgument,
    /// A shell argument runs an active substitution.
    ActiveShellSubstitution,
}

impl PathRefusalReason {
    /// Operator-visible refusal of a file tool path. It names the path, so it
    /// belongs in the tool result only, never in a log field.
    fn file_message(self, path: &str) -> String {
        match self {
            Self::InvalidPath => format!("Path not allowed: {path} is not a valid path (parent traversal or NUL byte)"),
            Self::ForbiddenPath => format!("Path not allowed: {path} is a forbidden path"),
            Self::OutsideWorkspace => format!("Path not allowed: {path} is outside the workspace"),
            Self::SymlinkEscape => {
                format!("Path not allowed: {path} resolves outside the workspace through a symlink")
            }
            Self::DynamicShellArgument => format!("Path not allowed: {path} is a dynamic path"),
            Self::ActiveShellSubstitution => format!("Path not allowed: {path} runs a shell substitution"),
        }
    }

    /// Operator-visible refusal of a shell path argument.
    fn shell_message(self, argument: &str) -> String {
        match self {
            Self::InvalidPath => format!("invalid path argument (parent traversal or NUL byte): {argument}"),
            Self::ForbiddenPath => format!("forbidden path argument: {argument}"),
            Self::OutsideWorkspace => format!("path argument is outside the workspace: {argument}"),
            Self::SymlinkEscape => {
                format!("path argument resolves outside the workspace through a symlink: {argument}")
            }
            Self::DynamicShellArgument => format!("forbidden dynamic path argument: {argument}"),
            Self::ActiveShellSubstitution => "forbidden active shell substitution".to_owned(),
        }
    }
}

/// A typed path refusal with its operator-visible text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathRefusal {
    reason: PathRefusalReason,
    message: String,
}

impl PathRefusal {
    #[must_use]
    pub fn file(reason: PathRefusalReason, path: &str) -> Self {
        Self {
            reason,
            message: reason.file_message(path),
        }
    }

    #[must_use]
    pub fn shell(reason: PathRefusalReason, argument: &str) -> Self {
        Self {
            reason,
            message: reason.shell_message(argument),
        }
    }

    #[must_use]
    pub const fn reason(&self) -> PathRefusalReason {
        self.reason
    }

    /// Operator-visible text; may name the path.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for PathRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl SecurityPolicy {
    /// Legacy callers execute relative to the workspace.
    pub fn is_path_allowed(&self, path: &str) -> bool {
        self.is_path_allowed_at(&self.workspace_dir, path)
    }

    /// Check the same absolute request path that native execution will resolve.
    pub(crate) fn is_path_allowed_at(&self, root: &Path, path: &str) -> bool {
        self.check_path_at(root, path).is_ok()
    }

    /// The lexical path policy below Full. A path is allowed exactly when it
    /// resolves inside the workspace (with `workspace_only`) and outside the
    /// forbidden paths, however it is written: relative, absolute or `~`.
    /// Symlinks are the caller's canonical check (`is_resolved_path_allowed`).
    pub(crate) fn check_path_at(&self, root: &Path, path: &str) -> Result<(), PathRefusalReason> {
        let target = resolve_request_path(root, path).map_err(|_| PathRefusalReason::InvalidPath)?;
        let lower = path.to_lowercase();
        if lower.contains("..%2f") || lower.contains("%2f..") {
            return Err(PathRefusalReason::InvalidPath);
        }
        let fold = |path: &Path| PathBuf::from(path.to_string_lossy().to_lowercase());
        let workspace_root =
            std::path::absolute(&self.workspace_dir).map_err(|_| PathRefusalReason::OutsideWorkspace)?;
        let workspace = fold(&workspace_root);
        let inside_workspace = target.starts_with(&workspace_root);
        for forbidden in &self.forbidden_paths {
            let forbidden = resolve_request_path(root, forbidden).map_err(|_| PathRefusalReason::ForbiddenPath)?;
            // A workspace under a default forbidden ancestor (e.g. /tmp) keeps
            // its own paths. A forbidden path inside the workspace still wins,
            // and a path outside the workspace receives no such exemption.
            let workspace_ancestor = inside_workspace
                && workspace.starts_with(fold(&forbidden))
                && !fold(&forbidden).starts_with(&workspace);
            if !workspace_ancestor && fold(&target).starts_with(fold(&forbidden)) {
                return Err(PathRefusalReason::ForbiddenPath);
            }
        }
        if self.workspace_only && !inside_workspace {
            return Err(PathRefusalReason::OutsideWorkspace);
        }
        Ok(())
    }

    /// Check canonical containment after resolving existing filesystem ancestors.
    pub fn is_resolved_path_allowed(&self, resolved: &Path) -> bool {
        if !self.workspace_only {
            return true;
        }
        let workspace = self
            .workspace_dir
            .canonicalize()
            .unwrap_or_else(|_| self.workspace_dir.clone());
        resolved.starts_with(workspace)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_forbidden_prefix_uses_the_selected_root() {
        let workspace = tempfile::tempdir().expect("test: workspace");
        let root = tempfile::tempdir().expect("test: working root");
        let policy = SecurityPolicy {
            workspace_dir: workspace.path().into(),
            workspace_only: false,
            forbidden_paths: vec!["blocked".into()],
            ..SecurityPolicy::default()
        };
        assert!(
            !policy.is_path_allowed("blocked/target"),
            "test: preserve relative deny entries without an override"
        );
        assert!(
            !policy.is_path_allowed_at(root.path(), "blocked/target"),
            "test: relative deny entries follow the actual root"
        );
        assert!(
            policy.is_path_allowed_at(root.path(), "allowed"),
            "test: unrelated request remains allowed"
        );
    }
}
