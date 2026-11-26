//! Renders an over-cap `MEMORY.md` for the system prompt: the newest generated
//! entries are shown first and the older ones are only counted. Under the cap
//! this module does nothing, so the caller keeps the head cut it has always used.
use std::borrow::Cow;
use std::fmt::Write;

use crate::memory::traits::MemoryEntry;

/// The preamble (text before the first entry) may use at most `max_chars / PREAMBLE_SHARE`.
const PREAMBLE_SHARE: usize = 4;
/// One entry is kept to this many characters; a longer one is cut and marked `...`.
const ENTRY_MAX_CHARS: usize = 400;
/// Characters kept free for the footer that counts the older entries.
const FOOTER_RESERVE_CHARS: usize = 200;

/// Render the newest-first index of `content` within `max_chars` characters.
///
/// Returns `None` when `content` fits the cap or has no generated entry; the
/// caller then keeps the existing head cut. Entries are read in reverse file
/// order, because the SQLite mirror appends new entries and moves an updated key
/// to the end, so the file's last entry is the newest. Timestamps are not parsed.
pub(super) fn render_memory_file(content: &str, max_chars: usize) -> Option<String> {
    if content.chars().count() <= max_chars {
        return None;
    }
    let (preamble, blocks) = split_blocks(content)?;
    let total = blocks.len();

    let mut head = take_chars(preamble, max_chars / PREAMBLE_SHARE).0.to_owned();
    if !head.is_empty() && !head.ends_with('\n') {
        head.push('\n');
    }
    // The shown count never exceeds the total, so a header written with the
    // total is at least as long as the real one: the budget below holds.
    let header_upper = format!("[Memory index: newest first, {total} of {total} entries]\n");
    let mut used = head.chars().count() + header_upper.chars().count();

    let mut entries: Vec<Cow<'_, str>> = Vec::new();
    for block in blocks.iter().rev() {
        let rendered = render_entry(block);
        let size = rendered.chars().count();
        if used + size + FOOTER_RESERVE_CHARS > max_chars {
            break;
        }
        used += size;
        entries.push(rendered);
    }

    let shown = entries.len();
    let older = total - shown;
    let mut index = head;
    let _ = writeln!(index, "[Memory index: newest first, {shown} of {total} entries]");
    for entry in &entries {
        index.push_str(entry);
    }
    if older > 0 {
        let _ = writeln!(
            index,
            "[{older} older entries not shown. Use memory_search, or memory_get with the key, to read them.]"
        );
    }
    Some(index)
}

/// Split `content` into the preamble and one block per entry line. A line that
/// is not an entry line belongs to the entry before it. Returns `None` when the
/// file has no entry line, or when an offset is not a character boundary.
fn split_blocks(content: &str) -> Option<(&str, Vec<&str>)> {
    let mut starts = Vec::new();
    let mut offset = 0;
    for line in content.split_inclusive('\n') {
        if entry_key(line).is_some() {
            starts.push(offset);
        }
        offset += line.len();
    }
    let first = *starts.first()?;
    let preamble = content.get(..first)?;
    let mut blocks = Vec::with_capacity(starts.len());
    for (index, &start) in starts.iter().enumerate() {
        let end = starts.get(index + 1).copied().unwrap_or(content.len());
        blocks.push(content.get(start..end)?);
    }
    Some((preamble, blocks))
}

/// The key of a generated line `- [<stamp>] **<key>**:`. This is the rule of
/// `SqliteMemory::generated_backup_entry_key` (`src/memory/sqlite/mirror.rs`),
/// repeated here so the context layer does not depend on the SQLite module.
fn entry_key(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("- [")?;
    let (_, rest) = rest.split_once("] **")?;
    let (key, _) = rest.split_once("**:")?;
    (!key.is_empty()).then_some(key)
}

/// One entry as it is shown: unchanged when short, otherwise cut with `...`.
/// Every entry ends with a newline so the next one starts on its own line.
fn render_entry(block: &str) -> Cow<'_, str> {
    let (kept, cut) = take_chars(block, ENTRY_MAX_CHARS);
    match (cut, kept.ends_with('\n')) {
        (false, true) => Cow::Borrowed(kept),
        (false, false) => Cow::Owned(format!("{kept}\n")),
        (true, _) => Cow::Owned(format!("{kept}...\n")),
    }
}

/// The first `max_chars` characters of `text` and whether anything was cut.
fn take_chars(text: &str, max_chars: usize) -> (&str, bool) {
    match text
        .char_indices()
        .nth(max_chars)
        .and_then(|(index, _)| text.split_at_checked(index))
    {
        Some((kept, _)) => (kept, true),
        None => (text, false),
    }
}

/// Store index: how many newest core memories are fetched for the index.
#[allow(dead_code)] // first caller: K-M2
pub(crate) const STORE_INDEX_FETCH: usize = 200;
/// Store index: the whole section, header and footer included, in characters.
#[allow(dead_code)] // first caller: K-M2
pub(crate) const STORE_INDEX_MAX_CHARS: usize = 4_000;
/// Store index: the cap for a compact turn (the same section, smaller budget).
#[allow(dead_code)] // first caller: K-M2
pub(crate) const STORE_INDEX_COMPACT_MAX_CHARS: usize = 1_500;
/// Characters of the content shown per entry line.
#[allow(dead_code)] // first caller: K-M2
const STORE_INDEX_CONTENT_CHARS: usize = 160;
/// Characters of a timestamp shown per entry line (the date part).
#[allow(dead_code)] // first caller: K-M2
const STORE_INDEX_STAMP_CHARS: usize = 10;
#[allow(dead_code)] // first caller: K-M2
const STORE_INDEX_HEADER: &str =
    "## Memory index\nNewest first. Read one in full with memory_get <key>, or search with memory_recall.\n";

/// Render the newest-first store index of `entries` within `max_chars` characters.
///
/// `entries` are already in the order the store returned them (newest first).
/// Returns `None` for no entries. A line that would pass the budget, footer
/// included, is not shown; the footer counts the hidden entries. When
/// `truncated_at_limit` is set the store had more rows than were fetched, and
/// the footer shows the count with a `+`. Output depends only on the inputs,
/// so the same rows always render the same text.
#[allow(dead_code)] // first caller: K-M2
pub(crate) fn render_store_index(
    entries: &[MemoryEntry],
    max_chars: usize,
    truncated_at_limit: bool,
) -> Option<String> {
    if entries.is_empty() {
        return None;
    }
    let hidden_marker = if truncated_at_limit { "+" } else { "" };
    // Reserve the footer for the largest count it can show, so the total stays within the budget.
    let footer_reserve = format!("({}{hidden_marker} more not shown)\n", entries.len())
        .chars()
        .count();
    let mut index = STORE_INDEX_HEADER.to_owned();
    let mut used = index.chars().count();
    let mut shown = 0;
    for entry in entries {
        let line = store_index_line(entry);
        let size = line.chars().count();
        if used + size + footer_reserve > max_chars {
            break;
        }
        used += size;
        shown += 1;
        index.push_str(&line);
    }
    let hidden = entries.len() - shown;
    if hidden > 0 || truncated_at_limit {
        let _ = write!(index, "({hidden}{hidden_marker} more not shown)\n");
    }
    Some(index)
}

/// One entry line: `- [<date>] <key>: <content>`, newlines shown as spaces.
#[allow(dead_code)] // first caller: K-M2
fn store_index_line(entry: &MemoryEntry) -> String {
    let (stamp, _) = take_chars(&entry.timestamp, STORE_INDEX_STAMP_CHARS);
    let flat = entry.content.replace(['\r', '\n'], " ");
    let (content, _) = take_chars(&flat, STORE_INDEX_CONTENT_CHARS);
    format!("- [{stamp}] {}: {content}\n", entry.key)
}
