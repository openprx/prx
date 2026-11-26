//! Renders an over-cap `MEMORY.md` for the system prompt: the newest generated
//! entries are shown first and the older ones are only counted. Under the cap
//! this module does nothing, so the caller keeps the head cut it has always used.
use std::borrow::Cow;
use std::fmt::Write;

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
