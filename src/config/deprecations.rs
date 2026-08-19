//! Configuration keys this runtime removed, kept recognisable so an existing
//! `config.toml` keeps loading.
//!
//! # Why this exists
//!
//! Configuration is never silently ignored here: [`crate::config::schema`]
//! collects every unrecognised TOML path and refuses to load, because a
//! misspelled key that reads as "default" is worse than a startup failure.
//!
//! That strictness has one bad interaction. When a knob is *deliberately*
//! removed — as the unbounded-runtime work removed the timeout and concurrency
//! ceilings — every deployment that ever configured it suddenly cannot start,
//! and the operator is told only that their key is "unknown", as if they had
//! mistyped it.
//!
//! So the two cases are separated rather than merged:
//!
//! - A key on the list below is *known to be gone*. It is dropped before
//!   deserialization with a `warn!` that says what happened and that the line
//!   is safe to delete. Startup continues.
//! - Anything else is still a hard error. A typo must never be absorbed by a
//!   mechanism built for retired keys.
//!
//! # What is not here
//!
//! Nothing rewrites the operator's files. The config directory is user data;
//! this layer reads it, reports precisely where the stale lines are, and lets
//! a human delete them.

use std::path::{Path, PathBuf};

/// Whether a retired path names one key or a whole table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeprecatedShape {
    /// A single `key = value` entry, e.g. `autonomy.max_actions_per_hour`.
    Field,
    /// An entire `[table]`, e.g. `[security.resources]` with all its keys.
    Section,
}

/// One retired configuration path plus the explanation an operator needs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DeprecatedKey {
    /// Dotted TOML path, outermost segment first.
    pub(crate) path: &'static [&'static str],
    /// Single key or whole table.
    pub(crate) shape: DeprecatedShape,
    /// Why it is gone. Written to be read by a human mid-upgrade, so it says
    /// what changed rather than naming an internal decision.
    pub(crate) reason: &'static str,
}

/// The runtime stopped rationing how much work may run at once.
const REASON_UNCAPPED: &str =
    "prx no longer caps how much work runs at once; live work is listed and ended with `prx tasks` instead";

/// The runtime stopped imposing deadlines on work it started.
const REASON_NO_TIMEOUT: &str =
    "prx no longer imposes timeouts on agent work; slow work is waited on and reported, not cancelled";

/// The key described an enforcement that never existed.
const REASON_NEVER_ENFORCED: &str = "this limit was never enforced by any code path and has been removed rather than \
                                     left as a false assurance";

/// The staged rollout this key steered has been retired along with its stages.
const REASON_ROLLOUT_RETIRED: &str = "read-only tool scheduling is no longer staged behind a rollout switch, so there is nothing left to stage or roll \
     back";

/// Every configuration path removed by the unbounded-runtime work.
///
/// Ordered by section so a reader can check it against a `config.toml` by eye.
/// Adding an entry is what makes an upgrade survivable; removing one turns the
/// key back into a hard "unknown configuration path" error, which is the right
/// end state once no deployment can still be carrying it.
pub(crate) const DEPRECATED_CONFIG_KEYS: &[DeprecatedKey] = &[
    // ── [agent]: tool concurrency governance ──────────────────────────────
    DeprecatedKey {
        path: &["agent", "parallel_tools"],
        shape: DeprecatedShape::Field,
        reason: REASON_UNCAPPED,
    },
    DeprecatedKey {
        path: &["agent", "read_only_tool_timeout_secs"],
        shape: DeprecatedShape::Field,
        reason: REASON_NO_TIMEOUT,
    },
    DeprecatedKey {
        path: &["agent", "concurrency_kill_switch_force_serial"],
        shape: DeprecatedShape::Field,
        reason: REASON_ROLLOUT_RETIRED,
    },
    DeprecatedKey {
        path: &["agent", "concurrency_rollout_stage"],
        shape: DeprecatedShape::Field,
        reason: REASON_ROLLOUT_RETIRED,
    },
    DeprecatedKey {
        path: &["agent", "concurrency_rollout_sample_percent"],
        shape: DeprecatedShape::Field,
        reason: REASON_ROLLOUT_RETIRED,
    },
    DeprecatedKey {
        path: &["agent", "concurrency_rollout_channels"],
        shape: DeprecatedShape::Field,
        reason: REASON_ROLLOUT_RETIRED,
    },
    DeprecatedKey {
        path: &["agent", "concurrency_auto_rollback_enabled"],
        shape: DeprecatedShape::Field,
        reason: REASON_ROLLOUT_RETIRED,
    },
    DeprecatedKey {
        path: &["agent", "concurrency_rollback_timeout_rate_threshold"],
        shape: DeprecatedShape::Field,
        reason: REASON_ROLLOUT_RETIRED,
    },
    DeprecatedKey {
        path: &["agent", "concurrency_rollback_cancel_rate_threshold"],
        shape: DeprecatedShape::Field,
        reason: REASON_ROLLOUT_RETIRED,
    },
    DeprecatedKey {
        path: &["agent", "concurrency_rollback_error_rate_threshold"],
        shape: DeprecatedShape::Field,
        reason: REASON_ROLLOUT_RETIRED,
    },
    // ── [sessions_spawn]: the former anti-fork-bomb valves ─────────────────
    DeprecatedKey {
        path: &["sessions_spawn", "max_concurrent"],
        shape: DeprecatedShape::Field,
        reason: REASON_UNCAPPED,
    },
    DeprecatedKey {
        path: &["sessions_spawn", "max_spawn_depth"],
        shape: DeprecatedShape::Field,
        reason: REASON_UNCAPPED,
    },
    DeprecatedKey {
        path: &["sessions_spawn", "max_children_per_agent"],
        shape: DeprecatedShape::Field,
        reason: REASON_UNCAPPED,
    },
    // ── [autonomy]: the hourly action budget ──────────────────────────────
    DeprecatedKey {
        path: &["autonomy", "max_actions_per_hour"],
        shape: DeprecatedShape::Field,
        reason: REASON_UNCAPPED,
    },
    // ── [gateway] ─────────────────────────────────────────────────────────
    DeprecatedKey {
        path: &["gateway", "request_timeout_secs"],
        shape: DeprecatedShape::Field,
        reason: REASON_NO_TIMEOUT,
    },
    // ── [security.resources]: limits nothing ever applied ─────────────────
    DeprecatedKey {
        path: &["security", "resources"],
        shape: DeprecatedShape::Section,
        reason: REASON_NEVER_ENFORCED,
    },
    // ── [memory.events] ───────────────────────────────────────────────────
    DeprecatedKey {
        path: &["memory", "events", "retention_days"],
        shape: DeprecatedShape::Field,
        reason: REASON_NEVER_ENFORCED,
    },
];

/// Where a retired key was found, so the warning can name a line to delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeprecatedKeySite {
    /// File the key was written in (the main config or a `config.d` fragment).
    pub(crate) file: PathBuf,
    /// 1-based line number of the key or of the table header.
    pub(crate) line: usize,
}

/// Remove every retired key from `root`, warning once per key that was present.
///
/// Runs before deserialization, so the paths it strips never reach the
/// unknown-path check and never turn an upgrade into a startup failure.
/// `config_path`, when given, is used only to point the operator at the exact
/// file and line; a missing or unreadable file just costs that detail.
///
/// Returns the dotted paths that were actually stripped, so a caller can act on
/// them without re-reading the tree.
pub(crate) fn strip_deprecated_keys(root: &mut toml::Value, config_path: Option<&Path>) -> Vec<String> {
    let mut stripped = Vec::new();
    for key in DEPRECATED_CONFIG_KEYS {
        if take_path(root, key.path).is_none() {
            continue;
        }
        let dotted = key.path.join(".");
        let site = config_path.and_then(|path| locate(path, key));
        match site {
            Some(site) => tracing::warn!(
                key = %dotted,
                file = %site.file.display(),
                line = site.line,
                reason = key.reason,
                "Ignoring removed configuration key; it no longer does anything and the line is safe to delete"
            ),
            None => tracing::warn!(
                key = %dotted,
                reason = key.reason,
                "Ignoring removed configuration key; it no longer does anything and the line is safe to delete"
            ),
        }
        stripped.push(dotted);
    }
    stripped
}

/// Take a value out of the tree by path, whether it is a leaf or a whole table.
fn take_path(root: &mut toml::Value, path: &[&str]) -> Option<toml::Value> {
    let (key, rest) = path.split_first()?;
    let table = root.as_table_mut()?;
    if rest.is_empty() {
        table.remove(*key)
    } else {
        table.get_mut(*key).and_then(|value| take_path(value, rest))
    }
}

/// Find the file and line a retired key was written on.
///
/// Best effort by design: it re-reads the config files as text rather than
/// threading provenance through the merge, because the answer is only ever used
/// to make a warning more helpful. When the scan cannot place the key — an
/// inline table, an unreadable fragment — the caller falls back to naming the
/// key alone, which is still enough to `grep` for.
pub(crate) fn locate(config_path: &Path, key: &DeprecatedKey) -> Option<DeprecatedKeySite> {
    for file in candidate_files(config_path) {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        if let Some(line) = locate_in_text(&text, key) {
            return Some(DeprecatedKeySite { file, line });
        }
    }
    None
}

/// The main config file followed by every `config.d/*.toml` fragment.
fn candidate_files(config_path: &Path) -> Vec<PathBuf> {
    let mut files = vec![config_path.to_path_buf()];
    let Some(parent) = config_path.parent() else {
        return files;
    };
    let Ok(entries) = std::fs::read_dir(parent.join("config.d")) else {
        return files;
    };
    let mut fragments: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("toml"))
        .collect();
    fragments.sort();
    files.extend(fragments);
    files
}

/// Scan one TOML document for `key`, tracking the current table header.
///
/// Tracking the header is what keeps this honest: leaf names repeat across
/// sections (`max_concurrent` also exists under `[scheduler]` and `[xin]`), so
/// matching a bare name would point the operator at a line that is still valid.
fn locate_in_text(text: &str, key: &DeprecatedKey) -> Option<usize> {
    let (table_path, leaf) = match key.shape {
        DeprecatedShape::Field => key.path.split_last().map(|(leaf, head)| (head, Some(*leaf)))?,
        DeprecatedShape::Section => (key.path, None),
    };
    let wanted_table = table_path.join(".");

    let mut current_table = String::new();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(header) = table_header(line) {
            current_table = header.to_string();
            if leaf.is_none() && current_table == wanted_table {
                return Some(index + 1);
            }
            continue;
        }
        let Some(name) = leaf else {
            continue;
        };
        if current_table == wanted_table && line_assigns(line, name) {
            return Some(index + 1);
        }
    }
    None
}

/// Extract `a.b` from a `[a.b]` header line, ignoring array-of-table headers.
///
/// `[[a.b]]` is deliberately not matched: no retired key lives in an array of
/// tables, and treating one as a plain header would mis-attribute the keys that
/// follow it.
fn table_header(line: &str) -> Option<&str> {
    if line.starts_with("[[") {
        return None;
    }
    let inner = line.strip_prefix('[')?;
    let end = inner.find(']')?;
    Some(inner[..end].trim())
}

/// True when `line` assigns to bare key `name` (quoted or not).
fn line_assigns(line: &str, name: &str) -> bool {
    let Some((left, _)) = line.split_once('=') else {
        return false;
    };
    let key = left.trim().trim_matches('"').trim_matches('\'');
    key == name
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every listed path must be non-empty and unique; a duplicate would warn
    /// twice and a bare path could strip the whole document.
    #[test]
    fn deprecated_key_table_is_well_formed() {
        let mut seen = std::collections::HashSet::new();
        for key in DEPRECATED_CONFIG_KEYS {
            assert!(!key.path.is_empty(), "a retired path must name something");
            assert!(!key.reason.trim().is_empty(), "{:?} needs a reason", key.path);
            assert!(
                seen.insert(key.path.join(".")),
                "duplicate retired path: {}",
                key.path.join(".")
            );
        }
    }

    #[test]
    fn locates_a_field_under_its_own_table() {
        let text = "[scheduler]\nmax_concurrent = 4\n\n[sessions_spawn]\nmax_concurrent = 64\n";
        let key = DeprecatedKey {
            path: &["sessions_spawn", "max_concurrent"],
            shape: DeprecatedShape::Field,
            reason: "test",
        };
        assert_eq!(
            locate_in_text(text, &key),
            Some(5),
            "a repeated leaf name must resolve against its table, not the first match"
        );
    }

    #[test]
    fn locates_a_section_header() {
        let text = "[security]\n[security.resources]\nmax_memory_mb = 512\n";
        let key = DeprecatedKey {
            path: &["security", "resources"],
            shape: DeprecatedShape::Section,
            reason: "test",
        };
        assert_eq!(locate_in_text(text, &key), Some(2));
    }

    #[test]
    fn ignores_commented_and_array_of_table_lines() {
        let text = "[[agents]]\nmax_actions_per_hour = 9\n\n[autonomy]\n# max_actions_per_hour = 1\nlevel = \"full\"\n";
        let key = DeprecatedKey {
            path: &["autonomy", "max_actions_per_hour"],
            shape: DeprecatedShape::Field,
            reason: "test",
        };
        assert_eq!(
            locate_in_text(text, &key),
            None,
            "neither an array-of-tables entry nor a commented line is a live key"
        );
    }
}
