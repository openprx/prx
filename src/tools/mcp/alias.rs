//! Provider-safe names for discovered MCP tools.
//!
//! A discovered tool is exposed to the model as `<prefix>__<server>__<tool>`.
//! Model providers accept function names only in `^[a-zA-Z0-9_-]{1,64}$`, so the
//! server and tool parts are sanitized and an alias longer than 64 characters is
//! shortened with a hash suffix. The original tool name is still what PRX sends to
//! the server; an alias is resolved by exact lookup, never by reversing it.

use super::RuntimeState;
use sha2::Digest;
use std::collections::{HashMap, HashSet};

const MAX_ALIAS_LEN: usize = 64;
/// `_` plus eight lowercase hex characters of SHA-256.
const HASH_SUFFIX_LEN: usize = 9;

/// Aliases of every discovered tool, keyed by server, then by original tool name.
pub(crate) type AliasTable = HashMap<String, HashMap<String, String>>;

/// Replaces every character outside `[A-Za-z0-9_-]` with `_`.
pub(crate) fn sanitize_part(part: &str) -> String {
    part.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// One discovered tool while the table is built.
struct Entry<'a> {
    server: &'a str,
    tool: &'a str,
    plain: String,
    candidate: String,
}

/// Aliases for the discovered tools of every server, from `(server, prefix, tool names)`.
///
/// The candidate of a tool is its plain name, or the hashed form when the plain
/// name is longer than 64 characters. Every candidate that occurs more than once,
/// across servers or within one, is replaced by its hashed form over the original
/// `server\0tool`. A hashed form that still collides is re-hashed with a counter
/// until it is unique. Tools are handled in (server, tool) order, so the result
/// does not depend on listing order, and a name that collides with nothing keeps
/// its plain form.
pub(crate) fn alias_table<'a>(servers: impl IntoIterator<Item = (&'a str, &'a str, Vec<&'a str>)>) -> AliasTable {
    let mut entries: Vec<Entry<'a>> = Vec::new();
    for (server, prefix, tools) in servers {
        let server_part = sanitize_part(server);
        for tool in tools {
            let plain = format!("{prefix}__{server_part}__{}", sanitize_part(tool));
            let candidate = if plain.len() <= MAX_ALIAS_LEN {
                plain.clone()
            } else {
                hashed_alias(&plain, server, tool, None)
            };
            entries.push(Entry {
                server,
                tool,
                plain,
                candidate,
            });
        }
    }
    entries.sort_by(|left, right| (left.server, left.tool).cmp(&(right.server, right.tool)));

    let mut counts: HashMap<&str, usize> = HashMap::new();
    for entry in &entries {
        *counts.entry(entry.candidate.as_str()).or_default() += 1;
    }
    // A candidate that occurs once is final; the rest are contested.
    let unique: Vec<Option<String>> = entries
        .iter()
        .map(|entry| (counts.get(entry.candidate.as_str()) == Some(&1)).then(|| entry.candidate.clone()))
        .collect();
    let mut used: HashSet<String> = unique.iter().flatten().cloned().collect();

    let mut table = AliasTable::new();
    for (entry, unique) in entries.iter().zip(unique) {
        let alias = match unique {
            Some(alias) => alias,
            None => {
                let mut alias = hashed_alias(&entry.plain, entry.server, entry.tool, None);
                let mut counter = 0_u64;
                // Final uniqueness check: a hashed form already taken is re-hashed with a counter.
                while used.contains(&alias) {
                    counter += 1;
                    alias = hashed_alias(&entry.plain, entry.server, entry.tool, Some(counter));
                }
                used.insert(alias.clone());
                alias
            }
        };
        table
            .entry(entry.server.to_string())
            .or_default()
            .insert(entry.tool.to_string(), alias);
    }
    table
}

fn hashed_alias(plain: &str, server: &str, tool: &str, counter: Option<u64>) -> String {
    let mut hasher = sha2::Sha256::new();
    hasher.update(server.as_bytes());
    hasher.update([0u8]);
    hasher.update(tool.as_bytes());
    if let Some(counter) = counter {
        hasher.update([0u8]);
        hasher.update(counter.to_be_bytes());
    }
    let digest: [u8; 32] = hasher.finalize().into();
    let hex: String = digest.iter().take(4).map(|byte| format!("{byte:02x}")).collect();
    let keep_max = MAX_ALIAS_LEN - HASH_SUFFIX_LEN;
    let cut = (0..=keep_max.min(plain.len()))
        .rev()
        .find(|&index| plain.is_char_boundary(index))
        .unwrap_or(0);
    let kept = plain.get(..cut).unwrap_or_default();
    format!("{kept}_{hex}")
}

/// Whether `alias` ends in the hash suffix of a hashed alias: `_` plus eight lowercase hex characters.
pub(crate) fn is_hashed_alias(alias: &str) -> bool {
    let Some(start) = alias.len().checked_sub(HASH_SUFFIX_LEN) else {
        return false;
    };
    let Some(hex) = alias.get(start..).and_then(|suffix| suffix.strip_prefix('_')) else {
        return false;
    };
    hex.len() == 8
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// The original `(server, tool)` behind an alias the catalog exposes. Resolution
/// reads the same capability specs the catalog is built from, so an alias resolves
/// exactly when the catalog carries it. Unknown aliases resolve to `None`.
pub(super) fn resolve(state: &RuntimeState, name: &str) -> Option<(String, String)> {
    state
        .capability_specs
        .iter()
        .find(|item| item.spec.name == name)
        .map(|item| (item.server.clone(), item.tool.clone()))
}
