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

/// Agentic loops now converge through completion, explicit failure, or cancellation.
const REASON_NO_ITERATION_LIMIT: &str = "prx no longer stops a progressing agent after an arbitrary number of tool rounds; use cancellation or `prx tasks kill` to end it";

/// Conversation retention is governed by the model-aware token budget.
const REASON_TOKEN_BUDGETED_HISTORY: &str = "prx no longer drops conversation history after an arbitrary message count; retention is governed by the configured context-token budget";

/// The key described an enforcement that never existed.
const REASON_NEVER_ENFORCED: &str = "this limit was never enforced by any code path and has been removed rather than \
                                     left as a false assurance";

/// The key promised a protection no code ever provided.
const REASON_NEVER_IMPLEMENTED: &str = "audit event signing was never implemented: the setting had no effect and audit \
                                        records carry no tamper evidence";

/// The staged rollout this key steered has been retired along with its stages.
const REASON_ROLLOUT_RETIRED: &str = "read-only tool scheduling is no longer staged behind a rollout switch, so there is nothing left to stage or roll \
     back";

/// The key named a choice the runtime makes itself.
const REASON_NEVER_READ: &str = "this setting was never read: prx picks the tool-call wire form from the provider's \
                                 capabilities, so the value had no effect";

/// Every configuration path removed by the unbounded-runtime work.
///
/// Ordered by section so a reader can check it against a `config.toml` by eye.
/// Adding an entry is what makes an upgrade survivable; removing one turns the
/// key back into a hard "unknown configuration path" error, which is the right
/// end state once no deployment can still be carrying it.
pub(crate) const DEPRECATED_CONFIG_KEYS: &[DeprecatedKey] = &[
    DeprecatedKey {
        path: &["agent", "max_tool_iterations"],
        shape: DeprecatedShape::Field,
        reason: REASON_NO_ITERATION_LIMIT,
    },
    DeprecatedKey {
        path: &["agent", "max_history_messages"],
        shape: DeprecatedShape::Field,
        reason: REASON_TOKEN_BUDGETED_HISTORY,
    },
    DeprecatedKey {
        path: &["runtime", "idle_hang_max_total_secs"],
        shape: DeprecatedShape::Field,
        reason: REASON_NO_TIMEOUT,
    },
    DeprecatedKey {
        path: &["agent", "tool_dispatcher"],
        shape: DeprecatedShape::Field,
        reason: REASON_NEVER_READ,
    },
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
    // ── [scheduler]: the per-cycle cron job ceiling ───────────────────────
    DeprecatedKey {
        path: &["scheduler", "max_concurrent"],
        shape: DeprecatedShape::Field,
        reason: REASON_UNCAPPED,
    },
    // ── [xin]: capacity is observable, not rationed by fixed ceilings ─────
    DeprecatedKey {
        path: &["xin", "max_concurrent"],
        shape: DeprecatedShape::Field,
        reason: REASON_UNCAPPED,
    },
    DeprecatedKey {
        path: &["xin", "max_tasks"],
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
    // ── [channels_config]: the per-turn wall clock ────────────────────────
    DeprecatedKey {
        path: &["channels_config", "message_timeout_secs"],
        shape: DeprecatedShape::Field,
        reason: REASON_NO_TIMEOUT,
    },
    // ── [security.audit]: signing that was never implemented (INT-2B) ─────
    DeprecatedKey {
        path: &["security", "audit", "sign_events"],
        shape: DeprecatedShape::Field,
        reason: REASON_NEVER_IMPLEMENTED,
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
    // Delegate agents are a user-named map, so their retired field cannot be
    // represented by the static exact-path table above. Strip every
    // `[agents.<name>].max_iterations` entry explicitly.
    let retired_agent_limits = root
        .get_mut("agents")
        .and_then(toml::Value::as_table_mut)
        .map(|agents| {
            agents
                .iter_mut()
                .filter_map(|(name, value)| {
                    value
                        .as_table_mut()
                        .and_then(|agent| agent.remove("max_iterations"))
                        .map(|_| format!("agents.{name}.max_iterations"))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for dotted in retired_agent_limits {
        tracing::warn!(
            key = %dotted,
            reason = REASON_NO_ITERATION_LIMIT,
            config = config_path.map(|path| path.display().to_string()),
            "Ignoring removed configuration key; it no longer does anything and the line is safe to delete"
        );
        stripped.push(dotted);
    }
    stripped
}

/// The warning for removed keys a structured write (`POST /api/config`,
/// `config.patch`) carries in its JSON body, or `None` when it carries none.
///
/// A file load drops such a key with a `warn!` naming its line; a structured
/// write drops it too (the typed configuration has no field for it), and
/// this makes that visible in the response instead of silent (B2f).
pub(crate) fn removed_keys_warning(body: &serde_json::Value) -> Option<String> {
    let found: Vec<String> = DEPRECATED_CONFIG_KEYS
        .iter()
        .filter(|key| {
            key.path
                .iter()
                .try_fold(body, |node, segment| node.get(*segment))
                .is_some()
        })
        .map(|key| format!("`{}` ({})", key.path.join("."), key.reason))
        .collect();
    (!found.is_empty()).then(|| {
        format!(
            "Ignoring removed configuration key(s) {}; they no longer do anything and nothing was stored for them",
            found.join(", ")
        )
    })
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
    for file in candidate_files(config_path).into_iter().rev() {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        if let Some(line) = locate_in_text(&text, key) {
            return Some(DeprecatedKeySite { file, line });
        }
    }
    None
}

/// The file among `config.toml` and its `config.d` fragments that holds the
/// key at `path` (dotted table path, e.g. `["causal_tree", "w_confidence"]`),
/// so an error names the file the operator has to edit. Best effort like
/// [`locate`]: `None` when no readable file holds it.
pub(crate) fn file_holding(config_path: &Path, path: &[&str]) -> Option<PathBuf> {
    candidate_files(config_path).into_iter().rev().find(|file| {
        std::fs::read_to_string(file)
            .ok()
            .and_then(|text| text.parse::<toml::Table>().ok())
            .is_some_and(|table| {
                let mut value: Option<&toml::Value> = None;
                for (index, key) in path.iter().enumerate() {
                    value = if index == 0 {
                        table.get(*key)
                    } else {
                        value.and_then(|v| v.get(key))
                    };
                }
                value.is_some()
            })
    })
}

/// Exactly the loader's files in merge order. Callers search in reverse so
/// diagnostics name the winning declaration, rather than a shadowed base key.
fn candidate_files(config_path: &Path) -> Vec<PathBuf> {
    let mut files = vec![config_path.to_path_buf()];
    let Ok(fragments) = super::files::list_config_fragment_paths(config_path) else {
        return files;
    };
    files.extend(fragments.into_iter().filter(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| !super::files::should_skip_fragment(name))
    }));
    files
}

/// Scan one TOML document for `key`, tracking the current table header.
///
/// Tracking the header is what keeps this honest: leaf names repeat across
/// sections (`max_concurrent` is retired under several schedulers), so
/// matching a bare name would point the operator at the wrong line.
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
    /// B2f: a structured write that carries a removed key names it.
    #[test]
    fn a_structured_body_with_a_removed_key_gets_a_warning() {
        let body = serde_json::json!({"security": {"audit": {"sign_events": true, "max_size_mb": 5}}});
        let warning = removed_keys_warning(&body).expect("test: sign_events is a removed key");
        assert!(warning.contains("security.audit.sign_events"), "{warning}");
        assert!(warning.contains("never implemented"), "{warning}");
        assert_eq!(
            removed_keys_warning(&serde_json::json!({"security": {"audit": {"max_size_mb": 5}}})),
            None
        );
    }

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
        let text = "[xin]\nmax_concurrent = 4\n\n[sessions_spawn]\nmax_concurrent = 64\n";
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
