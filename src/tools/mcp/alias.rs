//! Provider-safe names for discovered MCP tools.
//!
//! A discovered tool is exposed to the model as `<prefix>__<server>__<tool>`.
//! Model providers accept function names only in `^[a-zA-Z0-9_-]{1,64}$`, so the
//! server and tool parts are sanitized and an alias longer than 64 characters is
//! shortened with a hash suffix. The original tool name is still what PRX sends to
//! the server; an alias is resolved by exact lookup, never by reversing it.

use super::RuntimeState;
use sha2::Digest;
use std::collections::HashMap;

const MAX_ALIAS_LEN: usize = 64;
/// `_` plus eight lowercase hex characters of SHA-256.
const HASH_SUFFIX_LEN: usize = 9;

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

/// Aliases of one server's discovered tools, keyed by the original tool name.
///
/// A name that is already safe, fits in 64 characters and is not shared with
/// another tool of the same server stays unchanged. Otherwise the alias is the
/// plain name cut to fit plus `_` and eight hex characters of SHA-256 over the
/// original `server\0tool`, so two tools that sanitize to the same name both get
/// the suffix, independent of listing order.
pub(super) fn alias_map(prefix: &str, server: &str, tools: &[&str]) -> HashMap<String, String> {
    let server_part = sanitize_part(server);
    let plain = |tool: &str| format!("{prefix}__{server_part}__{}", sanitize_part(tool));
    let mut counts: HashMap<String, usize> = HashMap::new();
    for tool in tools {
        *counts.entry(plain(tool)).or_default() += 1;
    }
    tools
        .iter()
        .map(|tool| {
            let name = plain(tool);
            let shared = counts.get(&name).copied().unwrap_or(0) > 1;
            let alias = if !shared && name.len() <= MAX_ALIAS_LEN {
                name
            } else {
                hashed_alias(&name, server, tool)
            };
            ((*tool).to_string(), alias)
        })
        .collect()
}

fn hashed_alias(plain: &str, server: &str, tool: &str) -> String {
    let mut hasher = sha2::Sha256::new();
    hasher.update(server.as_bytes());
    hasher.update([0u8]);
    hasher.update(tool.as_bytes());
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

/// The original `(server, tool)` behind an alias that one of the server's
/// discovered tools exposes. Unknown aliases resolve to `None`.
pub(super) fn resolve(state: &RuntimeState, name: &str) -> Option<(String, String)> {
    state.discovered_tools.iter().find_map(|(server, tools)| {
        let prefix = &state.effective_config.servers.get(server)?.tool_name_prefix;
        let names: Vec<&str> = tools.keys().map(String::as_str).collect();
        alias_map(prefix, server, &names)
            .into_iter()
            .find(|(_, alias)| alias == name)
            .map(|(tool, _)| (server.clone(), tool))
    })
}
