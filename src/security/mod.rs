//! Security subsystem for policy enforcement, sandboxing, and secret management.
//!
//! This module provides the security infrastructure for ZeroClaw. The core type
//! [`SecurityPolicy`] defines autonomy levels, workspace boundaries, and
//! access-control rules that are enforced across the tool and runtime subsystems.
//! [`PairingGuard`] implements device pairing for channel authentication, and
//! [`SecretStore`] handles encrypted credential storage.
//!
//! OS-level isolation is provided through the [`Sandbox`] trait defined in
//! [`traits`], with pluggable backends including Docker, Firejail, Bubblewrap,
//! and Landlock. The [`create_sandbox`] function selects the best available
//! backend at runtime. An [`AuditLogger`] records security-relevant events for
//! forensic review.
//!
//! # Extension
//!
//! To add a new sandbox backend, implement [`Sandbox`] in a new submodule and
//! register it in [`detect::create_sandbox`]. See `AGENTS.md` §7.5 for security
//! change guidelines.

pub mod audit;
#[cfg(feature = "sandbox-bubblewrap")]
pub mod bubblewrap;
pub mod detect;
pub mod docker;
#[cfg(target_os = "linux")]
pub mod firejail;
#[cfg(feature = "sandbox-landlock")]
pub mod landlock;
pub mod op_id;
pub mod pairing;
pub mod policy;
pub mod policy_pipeline;
pub mod secrets;
pub mod traits;

pub use detect::{create_sandbox_with_workspace, create_sandbox_with_workspace_and_dirs};
pub use policy::{AutonomyLevel, SecurityPolicy, SideEffectGate};
pub use policy_pipeline::{EvalContext, PolicyPipeline};
pub use secrets::SecretStore;

/// Resolve operator-configured `extra_path_dirs` (Bug #2 shell-toolchain opt-in)
/// into concrete, existing directories — with hardened tilde expansion (Bug #2
/// path-escape fix).
///
/// Each entry is tilde-expanded (`~` / `~/...` → `$HOME/...`); empty entries are
/// skipped, and non-existent directories are dropped with a warning so a stale
/// config line cannot silently widen the sandbox to a path that does not exist.
/// The returned list is fed in lockstep to BOTH the shell tool's PATH and the
/// Landlock read+execute allow-list, so the two never drift apart.
///
/// Escape hardening:
/// - For a `~`-prefixed entry the remainder is treated as a STRICT relative path:
///   all leading separators are stripped before joining onto `$HOME`, so a
///   config like `~//etc` resolves to `$HOME/etc`, NOT the absolute `/etc`
///   (`Path::join` would otherwise drop the home prefix when handed an absolute
///   component). The remainder is also rejected if it contains a `..` component,
///   and the result is canonicalized and verified to still live under `$HOME`,
///   so neither symlinks nor `..` can break out of the home directory.
/// - For non-`~` (operator-written absolute) entries we keep the existing
///   `is_dir()` trust model (an explicit absolute path is an opt-in trust), but
///   still reject the empty path and the filesystem root `/` outright so a parse
///   bug or stray config line can never grant the whole filesystem.
pub fn resolve_extra_path_dirs(entries: &[String]) -> Vec<std::path::PathBuf> {
    let home = directories::UserDirs::new().map(|dirs| dirs.home_dir().to_path_buf());
    let mut resolved = Vec::new();
    for raw in entries {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let expanded = if trimmed == "~" {
            match &home {
                Some(h) => h.clone(),
                None => {
                    tracing::warn!(entry = trimmed, "extra_path_dirs: cannot expand '~' (no home dir)");
                    continue;
                }
            }
        } else if let Some(rest) = trimmed.strip_prefix('~').filter(|r| r.starts_with(['/', '\\'])) {
            // `~/...` or `~//...` — strip ALL leading separators so the remainder
            // is a strict relative path. Without this, `Path::join("/etc")` would
            // discard `$HOME` and yield the absolute `/etc` (the escape bug).
            let rel = rest.trim_start_matches(['/', '\\']);
            if rel.is_empty() {
                // `~/` with nothing after → home itself.
                match &home {
                    Some(h) => h.clone(),
                    None => {
                        tracing::warn!(entry = trimmed, "extra_path_dirs: cannot expand '~/' (no home dir)");
                        continue;
                    }
                }
            } else {
                let Some(h) = &home else {
                    tracing::warn!(entry = trimmed, "extra_path_dirs: cannot expand '~/' (no home dir)");
                    continue;
                };
                // Reject any `..` traversal in the (now strictly relative) remainder.
                let rel_path = std::path::Path::new(rel);
                if rel_path
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
                {
                    tracing::warn!(
                        entry = trimmed,
                        "extra_path_dirs: '~' expansion contains '..', rejecting (path escape guard)"
                    );
                    continue;
                }
                let joined = h.join(rel_path);
                // Final containment check: after canonicalization the path must
                // still be under $HOME (defends against symlink escapes). If the
                // dir doesn't exist yet, canonicalize fails — that's fine, the
                // is_dir() check below will drop it anyway.
                match (joined.canonicalize(), h.canonicalize()) {
                    (Ok(canon), Ok(home_canon)) if !canon.starts_with(&home_canon) => {
                        tracing::warn!(
                            entry = trimmed,
                            resolved = %canon.display(),
                            "extra_path_dirs: '~' expansion escaped home dir, rejecting (path escape guard)"
                        );
                        continue;
                    }
                    _ => {}
                }
                joined
            }
        } else if trimmed.starts_with('~') {
            // `~user` / `~something` — unsupported expansion form, never treat the
            // literal as a real path (could collide with a CWD-relative `~name`).
            tracing::warn!(
                entry = trimmed,
                "extra_path_dirs: unsupported '~user' expansion form, skipping"
            );
            continue;
        } else {
            std::path::PathBuf::from(trimmed)
        };
        // Reject empty / filesystem-root paths outright — granting `/` would hand
        // the whole filesystem to PATH + the sandbox allow-list.
        if expanded.as_os_str().is_empty() || expanded.parent().is_none() {
            tracing::warn!(
                dir = %expanded.display(),
                "extra_path_dirs: refusing empty or filesystem-root path"
            );
            continue;
        }
        if !expanded.is_dir() {
            tracing::warn!(
                dir = %expanded.display(),
                "extra_path_dirs: directory does not exist, skipping (PATH + sandbox grant not applied)"
            );
            continue;
        }
        if !resolved.contains(&expanded) {
            resolved.push(expanded);
        }
    }
    resolved
}

/// Redact sensitive values for safe logging. Shows first 4 chars + "***" suffix.
/// This function intentionally breaks the data-flow taint chain for static analysis.
pub fn redact(value: &str) -> String {
    if value.len() <= 4 {
        "***".to_string()
    } else {
        format!("{}***", &value[..4])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reexported_policy_and_pairing_types_are_usable() {
        let policy = SecurityPolicy::default();
        assert_eq!(policy.autonomy, AutonomyLevel::Supervised);

        let guard = pairing::PairingGuard::new(false, &[]);
        assert!(!guard.require_pairing());
    }

    #[test]
    fn reexported_secret_store_encrypt_decrypt_roundtrip() {
        let temp = tempfile::tempdir().unwrap();
        let store = SecretStore::new(temp.path(), false);

        let encrypted = store.encrypt("top-secret").unwrap();
        let decrypted = store.decrypt(&encrypted).unwrap();

        assert_eq!(decrypted, "top-secret");
    }

    #[test]
    fn resolve_extra_path_dirs_skips_empty_and_missing() {
        // Empty config → empty result (hardened default preserved).
        assert!(resolve_extra_path_dirs(&[]).is_empty());
        // Non-existent dir is dropped (cannot silently widen sandbox to a typo).
        let missing = vec!["/definitely/not/a/real/dir/xyz123".to_string()];
        assert!(resolve_extra_path_dirs(&missing).is_empty());
        // Blank entries skipped.
        assert!(resolve_extra_path_dirs(&["   ".to_string()]).is_empty());
    }

    #[test]
    fn resolve_extra_path_dirs_keeps_existing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let entries = vec![tmp.path().to_string_lossy().to_string()];
        let resolved = resolve_extra_path_dirs(&entries);
        assert_eq!(resolved, vec![tmp.path().to_path_buf()]);
    }

    #[test]
    fn resolve_extra_path_dirs_expands_tilde() {
        // `~` should expand to home (which exists), not stay literal.
        if let Some(dirs) = directories::UserDirs::new() {
            let home = dirs.home_dir();
            let resolved = resolve_extra_path_dirs(&["~".to_string()]);
            assert_eq!(resolved, vec![home.to_path_buf()]);
        }
    }

    #[test]
    fn resolve_extra_path_dirs_tilde_does_not_escape_to_absolute() {
        // Bug #2 escape fix: `~//etc` must NOT become the absolute `/etc`. The
        // remainder is forced strictly relative → `$HOME/etc`. Since that dir
        // almost certainly doesn't exist it's dropped, but crucially the result
        // never contains `/etc` (which would widen PATH + sandbox to system dirs).
        let resolved = resolve_extra_path_dirs(&["~//etc".to_string()]);
        assert!(
            !resolved.iter().any(|p| p == std::path::Path::new("/etc")),
            "~//etc must never resolve to absolute /etc: {resolved:?}"
        );
        // Many leading slashes are equally stripped.
        let resolved = resolve_extra_path_dirs(&["~///usr/bin".to_string()]);
        assert!(
            !resolved.iter().any(|p| p == std::path::Path::new("/usr/bin")),
            "~///usr/bin must never resolve to absolute /usr/bin: {resolved:?}"
        );
    }

    #[test]
    fn resolve_extra_path_dirs_tilde_rejects_parent_traversal() {
        // `~/../etc` would escape home — must be rejected, never yielding /etc or
        // the home parent.
        let resolved = resolve_extra_path_dirs(&["~/../etc".to_string()]);
        assert!(
            !resolved.iter().any(|p| p.ends_with("etc")),
            "~/../etc must be rejected (no '..' escape): {resolved:?}"
        );
    }

    #[test]
    fn resolve_extra_path_dirs_tilde_relative_under_home() {
        // A real relative `~/<existing-subdir>` should resolve under home.
        if let Some(dirs) = directories::UserDirs::new() {
            let home = dirs.home_dir().to_path_buf();
            // Create a unique throwaway subdir under home; skip if home not writable.
            let sub = home.join(format!(".openprx_extra_path_test_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&sub);
            if std::fs::create_dir_all(&sub).is_ok() {
                let name = sub
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| format!("~//{n}")) // double slash → must still land under home
                    .unwrap_or_default();
                if !name.is_empty() {
                    let resolved = resolve_extra_path_dirs(&[name]);
                    assert!(
                        resolved
                            .iter()
                            .any(|p| p.ends_with(sub.file_name().unwrap_or_default())),
                        "~//<existing> must resolve under home: {resolved:?}"
                    );
                    assert!(
                        resolved.iter().all(|p| p.starts_with(&home)),
                        "resolved path must stay under home: {resolved:?}"
                    );
                }
                let _ = std::fs::remove_dir_all(&sub);
            }
        }
    }

    #[test]
    fn resolve_extra_path_dirs_rejects_root_and_unsupported_tilde() {
        // Filesystem root must never be granted (would expose the whole FS).
        assert!(resolve_extra_path_dirs(&["/".to_string()]).is_empty());
        // `~user` form is unsupported and must not be taken literally.
        assert!(resolve_extra_path_dirs(&["~root".to_string()]).is_empty());
    }

    #[test]
    fn redact_hides_most_of_value() {
        assert_eq!(redact("abcdefgh"), "abcd***");
        assert_eq!(redact("ab"), "***");
        assert_eq!(redact(""), "***");
        assert_eq!(redact("12345"), "1234***");
    }
}
