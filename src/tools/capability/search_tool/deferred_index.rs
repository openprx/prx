//! A compact name-only index of the tools a window-fitted turn left out of its
//! request, carried on the `capability_search` description so the model can
//! call one by its exact name without a search round.

use std::collections::BTreeMap;

use super::DiscoveryEntry;
use crate::tools::capability::catalog::Exposure;
use crate::tools::capability::contract::CapabilityId;
use crate::tools::surface_owner::turn_port::TurnSurfacePort;

/// Most bytes the index adds to the description (about 500 tokens).
const MAX_INDEX_BYTES: usize = 2_400;
/// Most words kept from a tool's own description.
const MAX_BLURB_WORDS: usize = 5;
/// Most bytes kept from those words.
const MAX_BLURB_BYTES: usize = 40;

const INDEX_HEADER: &str = "\n\nTools left out of this request to fit the model's context window. Call one by its exact name; its schema is admitted on the call, or search for it first: ";

/// The first few words of a description, one line, no markup.
fn blurb(description: &str) -> String {
    let first = description.split(['.', '\n']).next().unwrap_or_default();
    let mut text = String::new();
    for word in first.split_whitespace().take(MAX_BLURB_WORDS) {
        let word: String = word.chars().filter(|c| !c.is_control() && !matches!(c, '(' | ')' | ';')).collect();
        if word.is_empty() {
            continue;
        }
        if text.len().saturating_add(word.len()).saturating_add(1) > MAX_BLURB_BYTES {
            break;
        }
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&word);
    }
    text
}

/// The index text for the deferred entries of `entries`, in name order so it
/// is identical on every turn of the same tool set. Empty when nothing was
/// deferred. A tool the bound does not reach is counted, never dropped from
/// reach: it stays callable by name and found by search.
pub(super) fn deferred_index(port: &dyn TurnSurfacePort, entries: &BTreeMap<CapabilityId, DiscoveryEntry>) -> String {
    let catalog = port.catalog();
    let mut items: Vec<(String, String)> = entries
        .values()
        .filter(|entry| entry.searchable && !entry.core)
        .filter_map(|entry| {
            let resolved = catalog.resolve(std::slice::from_ref(&entry.reference)).ok()?;
            let capability = resolved.first()?;
            if !matches!(capability.exposure, Exposure::Direct) {
                return None;
            }
            let description = port
                .execution()
                .current_descriptor(&capability.public_name)
                .map(|descriptor| descriptor.description)
                .unwrap_or_default();
            Some((capability.public_name.clone(), blurb(&description)))
        })
        .collect();
    if items.is_empty() {
        return String::new();
    }
    items.sort();
    let mut text = String::from(INDEX_HEADER);
    let mut listed = 0usize;
    for (name, words) in &items {
        let piece = if words.is_empty() {
            name.clone()
        } else {
            format!("{name} ({words})")
        };
        let separator = if listed == 0 { 0 } else { 2 };
        if text.len().saturating_add(separator).saturating_add(piece.len()) > MAX_INDEX_BYTES {
            break;
        }
        if listed > 0 {
            text.push_str(", ");
        }
        text.push_str(&piece);
        listed += 1;
    }
    let omitted = items.len().saturating_sub(listed);
    if omitted > 0 {
        text.push_str(&format!(", and {omitted} more found by search"));
    }
    text
}
