//! CommonMark-aware document block parsing and chunking.
//!
//! The parser keeps original source slices, nested heading paths, and atomic
//! list/code/table blocks. Chunks are assembled from semantic blocks instead
//! of treating individual lines as independent memories.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MarkdownBlock {
    pub kind: &'static str,
    pub heading_path: Vec<String>,
    pub content: String,
    pub source_anchor: String,
}

/// A single chunk of text with metadata.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub index: usize,
    pub content: String,
    pub heading: Option<Rc<str>>,
}

struct ActiveRoot {
    range: Range<usize>,
    kind: &'static str,
    heading_level: Option<usize>,
    heading_text: String,
}

/// Parse Markdown into source-preserving CommonMark block nodes.
pub(crate) fn parse_markdown_blocks(text: &str) -> Vec<MarkdownBlock> {
    if text.trim().is_empty() {
        return Vec::new();
    }

    let parser = Parser::new_ext(text, Options::all()).into_offset_iter();
    let mut depth = 0usize;
    let mut active: Option<ActiveRoot> = None;
    let mut heading_path = Vec::<String>::new();
    let mut blocks = Vec::new();
    let mut anchor_occurrences = HashMap::<String, usize>::new();

    for (event, range) in parser {
        match event {
            Event::Start(tag) => {
                if depth == 0 {
                    active = Some(ActiveRoot {
                        range,
                        kind: block_kind(&tag),
                        heading_level: heading_level(&tag),
                        heading_text: String::new(),
                    });
                }
                depth = depth.saturating_add(1);
            }
            Event::End(_) => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    if let Some(root) = active.take() {
                        if let Some(level) = root.heading_level {
                            heading_path.truncate(level.saturating_sub(1));
                            heading_path.push(root.heading_text.trim().to_string());
                        }
                        push_block(
                            text,
                            root.range,
                            root.kind,
                            &heading_path,
                            &mut anchor_occurrences,
                            &mut blocks,
                        );
                    }
                }
            }
            Event::Text(value) | Event::Code(value) => {
                if let Some(root) = active.as_mut() {
                    if root.heading_level.is_some() {
                        root.heading_text.push_str(&value);
                    }
                }
            }
            Event::Rule | Event::TaskListMarker(_) if depth == 0 => {
                push_block(
                    text,
                    range,
                    "thematic_break",
                    &heading_path,
                    &mut anchor_occurrences,
                    &mut blocks,
                );
            }
            _ => {}
        }
    }

    blocks
}

fn push_block(
    source: &str,
    range: Range<usize>,
    kind: &'static str,
    heading_path: &[String],
    occurrences: &mut HashMap<String, usize>,
    blocks: &mut Vec<MarkdownBlock>,
) {
    let Some(raw) = source.get(range) else {
        return;
    };
    let content = raw.trim();
    if content.is_empty() {
        return;
    }

    let path_label = heading_path.join("/");
    let digest = format!(
        "{:x}",
        Sha256::digest(format!("{kind}\0{path_label}\0{content}").as_bytes())
    );
    let short_hash = digest.get(..16).unwrap_or(&digest);
    let base = format!("{}-{short_hash}", anchor_prefix(heading_path));
    let occurrence = occurrences.entry(base.clone()).or_insert(0);
    let source_anchor = if *occurrence == 0 {
        base
    } else {
        format!("{base}-{}", occurrence.saturating_add(1))
    };
    *occurrence = occurrence.saturating_add(1);

    blocks.push(MarkdownBlock {
        kind,
        heading_path: heading_path.to_vec(),
        content: content.to_string(),
        source_anchor,
    });
}

const fn heading_level(tag: &Tag<'_>) -> Option<usize> {
    let Tag::Heading { level, .. } = tag else {
        return None;
    };
    Some(match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    })
}

const fn block_kind(tag: &Tag<'_>) -> &'static str {
    match tag {
        Tag::Paragraph => "paragraph",
        Tag::Heading { .. } => "heading",
        Tag::BlockQuote(_) => "blockquote",
        Tag::CodeBlock(_) => "code_block",
        Tag::HtmlBlock => "html_block",
        Tag::List(_) => "list",
        Tag::FootnoteDefinition(_) => "footnote",
        Tag::DefinitionList => "definition_list",
        Tag::Table(_) => "table",
        Tag::MetadataBlock(_) => "frontmatter",
        _ => "block",
    }
}

fn anchor_prefix(heading_path: &[String]) -> String {
    let slug = heading_path
        .last()
        .map_or("root", String::as_str)
        .chars()
        .flat_map(char::to_lowercase)
        .map(|character| if character.is_alphanumeric() { character } else { '-' })
        .collect::<String>();
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "root".to_string()
    } else {
        slug.chars().take(48).collect()
    }
}

/// Split Markdown into chunks under the approximate token target where
/// possible, without breaking atomic code/list/table blocks.
pub fn chunk_markdown(text: &str, max_tokens: usize) -> Vec<Chunk> {
    let blocks = parse_markdown_blocks(text);
    if blocks.is_empty() {
        return Vec::new();
    }

    let max_chars = max_tokens.saturating_mul(4).max(1);
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_heading: Option<Rc<str>> = None;

    for block in blocks {
        let heading = (!block.heading_path.is_empty()).then(|| Rc::<str>::from(block.heading_path.join(" > ")));
        let heading_changed = current_heading != heading;
        let separator = usize::from(!current.is_empty()).saturating_mul(2);
        let would_overflow = current
            .len()
            .saturating_add(separator)
            .saturating_add(block.content.len())
            > max_chars;

        if !current.is_empty() && (heading_changed || would_overflow) {
            push_chunk(&mut chunks, &mut current, current_heading.take());
        }

        if block.content.len() > max_chars && !is_atomic_block(block.kind) {
            for piece in split_text(&block.content, max_chars) {
                if !current.is_empty() {
                    push_chunk(&mut chunks, &mut current, current_heading.take());
                }
                chunks.push(Chunk {
                    index: chunks.len(),
                    content: piece,
                    heading: heading.clone(),
                });
            }
            continue;
        }

        if !current.is_empty() {
            current.push_str("\n\n");
        }
        current.push_str(&block.content);
        current_heading = heading;
    }

    if !current.is_empty() {
        push_chunk(&mut chunks, &mut current, current_heading);
    }
    chunks
}

fn is_atomic_block(kind: &str) -> bool {
    matches!(kind, "code_block" | "list" | "table" | "html_block" | "frontmatter")
}

fn push_chunk(chunks: &mut Vec<Chunk>, current: &mut String, heading: Option<Rc<str>>) {
    let content = std::mem::take(current).trim().to_string();
    if !content.is_empty() {
        chunks.push(Chunk {
            index: chunks.len(),
            content,
            heading,
        });
    }
}

fn split_text(text: &str, max_chars: usize) -> Vec<String> {
    let mut remaining = text.trim();
    let mut pieces = Vec::new();
    while remaining.chars().count() > max_chars {
        let byte_limit = remaining
            .char_indices()
            .nth(max_chars)
            .map_or(remaining.len(), |(index, _)| index);
        let prefix = remaining.get(..byte_limit).unwrap_or(remaining);
        let split_at = prefix
            .rfind(char::is_whitespace)
            .filter(|index| *index > max_chars / 2)
            .unwrap_or(byte_limit);
        let (piece, rest) = remaining.split_at(split_at);
        if !piece.trim().is_empty() {
            pieces.push(piece.trim().to_string());
        }
        remaining = rest.trim_start();
    }
    if !remaining.is_empty() {
        pieces.push(remaining.to_string());
    }
    pieces
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text() {
        assert!(chunk_markdown("", 512).is_empty());
        assert!(chunk_markdown("   ", 512).is_empty());
    }

    #[test]
    fn parses_all_heading_levels_and_preserves_path() {
        let blocks = parse_markdown_blocks("# A\n\n#### Deep\n\ntext");
        assert!(blocks.iter().any(|block| block.content == "#### Deep"));
        let paragraph = blocks.iter().find(|block| block.content == "text").unwrap();
        assert_eq!(paragraph.heading_path, vec!["A", "Deep"]);
    }

    #[test]
    fn keeps_code_lists_tables_and_frontmatter_atomic() {
        let input = "---\ntitle: Demo\n---\n\n- one\n  - two\n\n```rust\nfn main() {}\n```\n\n|a|b|\n|-|-|\n|1|2|";
        let blocks = parse_markdown_blocks(input);
        for kind in ["frontmatter", "list", "code_block", "table"] {
            assert!(blocks.iter().any(|block| block.kind == kind), "missing {kind}");
        }
    }

    #[test]
    fn stable_anchor_survives_unrelated_prefix_insertion() {
        let original = parse_markdown_blocks("# Stable\n\nKeep me.");
        let changed = parse_markdown_blocks("Preface.\n\n# Stable\n\nKeep me.");
        let original_block = original.iter().find(|block| block.content == "Keep me.").unwrap();
        let changed_block = changed.iter().find(|block| block.content == "Keep me.").unwrap();
        assert_eq!(original_block.source_anchor, changed_block.source_anchor);
    }

    #[test]
    fn respects_max_tokens_for_splittable_paragraphs() {
        let text = "word ".repeat(500);
        let chunks = chunk_markdown(&text, 50);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|chunk| chunk.content.chars().count() <= 200));
    }

    #[test]
    fn unicode_content_is_preserved() {
        let text = "# 日本語\n\nこんにちは世界\n\n## Émojis\n\n🦀 Rust is great 🚀";
        let chunks = chunk_markdown(text, 512);
        let all = chunks
            .iter()
            .map(|chunk| chunk.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("こんにちは"));
        assert!(all.contains("🦀"));
    }

    #[test]
    fn indexes_are_sequential() {
        let chunks = chunk_markdown("# A\n\nContent A\n\n# B\n\nContent B", 512);
        assert!(chunks.iter().enumerate().all(|(index, chunk)| chunk.index == index));
    }
}
