//! `@file` mention extraction and prompt enrichment.

use crate::tools::Tool;

pub(super) const FILE_MENTION_MAX_FILES: usize = 5;

pub(super) const FILE_MENTION_MAX_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FileMention {
    pub(super) token: String,
    pub(super) path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FileMentionEnrichment {
    pub(super) prompt: String,
    pub(super) visible_note: Option<String>,
}

pub(super) fn extract_file_mentions(input: &str) -> Vec<FileMention> {
    let mut mentions = Vec::new();
    let mut iter = input.char_indices().peekable();

    while let Some((idx, ch)) = iter.next() {
        if ch != '@' {
            continue;
        }

        if idx > 0
            && input[..idx]
                .chars()
                .next_back()
                .is_some_and(is_file_mention_email_prefix)
        {
            continue;
        }

        let Some(&(start, first)) = iter.peek() else {
            continue;
        };
        if first.is_whitespace() || first == '@' || first == '"' || first == '\'' {
            continue;
        }

        let mut end = input.len();
        let mut saw_char = false;
        for (next_idx, next_ch) in input[start..].char_indices() {
            if next_ch.is_whitespace() {
                end = start + next_idx;
                break;
            }
            saw_char = true;
        }
        if !saw_char {
            continue;
        }

        let raw_path = &input[start..end];
        let path = raw_path.trim_end_matches(is_file_mention_trailing_punctuation);
        if path.is_empty() {
            continue;
        }

        mentions.push(FileMention {
            token: format!("@{path}"),
            path: path.to_string(),
        });
    }

    mentions
}

const fn is_file_mention_email_prefix(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '+')
}

const fn is_file_mention_trailing_punctuation(ch: char) -> bool {
    matches!(ch, ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}')
}

pub(super) async fn enrich_file_mentions_for_prompt(
    user_input: &str,
    tools_registry: &[Box<dyn Tool>],
) -> FileMentionEnrichment {
    let mentions = extract_file_mentions(user_input);
    if mentions.is_empty() {
        return FileMentionEnrichment {
            prompt: user_input.to_string(),
            visible_note: None,
        };
    }

    let mut sections = Vec::new();
    let mut visible_notes = Vec::new();
    let file_read = tools_registry.iter().find(|tool| tool.supports_name("file_read"));

    for mention in mentions.iter().take(FILE_MENTION_MAX_FILES) {
        match file_read {
            Some(tool) => {
                let args = serde_json::json!({
                    "path": mention.path.as_str(),
                    "max_bytes": FILE_MENTION_MAX_BYTES,
                });
                match tool.execute_named("file_read", args).await {
                    Ok(result) if result.success => {
                        let (content, truncated) = truncate_utf8_to_byte_cap(&result.output, FILE_MENTION_MAX_BYTES);
                        let mut section = format!("### {}\nPath: {}\n\n{}", mention.token, mention.path, content);
                        if truncated {
                            section.push_str("\n[content truncated: 64 KiB limit]");
                            visible_notes.push(format!("{}: content truncated to 64 KiB", mention.token));
                        }
                        sections.push(section);
                    }
                    Ok(result) => {
                        let note = file_mention_failure_note(&mention.token, result.error.as_deref());
                        sections.push(format!("### {}\nPath: {}\n\n{note}", mention.token, mention.path));
                        visible_notes.push(note);
                    }
                    Err(e) => {
                        let note = format!("{}: unavailable ({e})", mention.token);
                        sections.push(format!("### {}\nPath: {}\n\n{note}", mention.token, mention.path));
                        visible_notes.push(note);
                    }
                }
            }
            None => {
                let note = format!("{}: unavailable (file_read tool is not registered)", mention.token);
                sections.push(format!("### {}\nPath: {}\n\n{note}", mention.token, mention.path));
                visible_notes.push(note);
            }
        }
    }

    if mentions.len() > FILE_MENTION_MAX_FILES {
        let skipped = mentions.len().saturating_sub(FILE_MENTION_MAX_FILES);
        let note = format!("{skipped} file mention(s) skipped: maximum {FILE_MENTION_MAX_FILES} per message");
        sections.push(note.clone());
        visible_notes.push(note);
    }

    let prompt = if sections.is_empty() {
        user_input.to_string()
    } else {
        format!(
            "{user_input}\n\n[Attached file context from @path mentions]\n{}\n[End attached file context]",
            sections.join("\n\n")
        )
    };

    let visible_note = if visible_notes.is_empty() {
        None
    } else {
        Some(format!("File mention note: {}", visible_notes.join("; ")))
    };

    FileMentionEnrichment { prompt, visible_note }
}

fn file_mention_failure_note(token: &str, error: Option<&str>) -> String {
    let Some(error) = error else {
        return format!("{token}: unavailable");
    };

    if error.contains("not allowed")
        || error.contains("escapes workspace")
        || error.contains("Access denied")
        || error.contains("security policy")
    {
        format!("{token}: unavailable (blocked by policy)")
    } else if error.contains("Failed to resolve")
        || error.contains("No such file")
        || error.contains("not found")
        || error.contains("No such file or directory")
    {
        format!("{token}: unavailable (missing or inaccessible)")
    } else if error.contains("Is a directory") || error.contains("directory") {
        format!("{token}: unavailable (not a file)")
    } else if error.contains("File too large") {
        format!("{token}: unavailable (file too large)")
    } else {
        format!("{token}: unavailable")
    }
}

pub(super) fn truncate_utf8_to_byte_cap(input: &str, max_bytes: usize) -> (String, bool) {
    if input.len() <= max_bytes {
        return (input.to_string(), false);
    }

    let mut end = 0usize;
    for (idx, ch) in input.char_indices() {
        let next = idx.saturating_add(ch.len_utf8());
        if next > max_bytes {
            break;
        }
        end = next;
    }

    (input[..end].to_string(), true)
}
