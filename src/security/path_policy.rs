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

impl SecurityPolicy {
    /// Legacy callers execute relative to the workspace.
    pub fn is_path_allowed(&self, path: &str) -> bool {
        self.is_path_allowed_at(&self.workspace_dir, path)
    }

    /// Check the same absolute request path that native execution will resolve.
    pub(crate) fn is_path_allowed_at(&self, root: &Path, path: &str) -> bool {
        let Ok(target) = resolve_request_path(root, path) else {
            return false;
        };
        let lower = path.to_lowercase();
        if lower.contains("..%2f") || lower.contains("%2f..") {
            return false;
        }
        let expanded = crate::config::expand_home(Path::new(path));
        if self.workspace_only && expanded.is_absolute() {
            return false;
        }
        let fold = |path: &Path| PathBuf::from(path.to_string_lossy().to_lowercase());
        let Ok(workspace_root) = std::path::absolute(&self.workspace_dir) else {
            return false;
        };
        let workspace = fold(&workspace_root);
        for forbidden in &self.forbidden_paths {
            let Ok(forbidden) = resolve_request_path(root, forbidden) else {
                return false;
            };
            // Preserve main's allowance for a relative workspace path when the
            // workspace itself is under a default forbidden ancestor (e.g. /tmp).
            // A working root outside that workspace receives no such exemption.
            let workspace_ancestor = !expanded.is_absolute()
                && fold(&target).starts_with(&workspace)
                && workspace.starts_with(fold(&forbidden))
                && !fold(&forbidden).starts_with(&workspace);
            if !workspace_ancestor && fold(&target).starts_with(fold(&forbidden)) {
                return false;
            }
        }
        !self.workspace_only || target.starts_with(workspace_root)
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
