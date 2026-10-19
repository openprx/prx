//! Stripping of tool-call artifacts from channel responses.

use super::{HashSet, Tool};

pub(crate) fn sanitize_channel_response(response: &str, tools: &[Box<dyn Tool>]) -> String {
    let known_tool_names: HashSet<String> = tools.iter().map(|tool| tool.name().to_ascii_lowercase()).collect();
    let cleaned_tags = strip_isolated_tool_tag_artifacts(response, &known_tool_names);
    strip_isolated_tool_json_artifacts(&cleaned_tags, &known_tool_names)
}

fn is_tool_call_payload(value: &serde_json::Value, known_tool_names: &HashSet<String>) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };

    let (name, has_args) = object.get("function").and_then(|f| f.as_object()).map_or_else(
        || {
            (
                object.get("name").and_then(|v| v.as_str()),
                object.contains_key("arguments") || object.contains_key("parameters"),
            )
        },
        |function| {
            (
                function
                    .get("name")
                    .and_then(|v| v.as_str())
                    .or_else(|| object.get("name").and_then(|v| v.as_str())),
                function.contains_key("arguments")
                    || function.contains_key("parameters")
                    || object.contains_key("arguments")
                    || object.contains_key("parameters"),
            )
        },
    );

    let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) else {
        return false;
    };

    has_args && known_tool_names.contains(&name.to_ascii_lowercase())
}

fn is_tool_result_payload(object: &serde_json::Map<String, serde_json::Value>, saw_tool_call_payload: bool) -> bool {
    if !saw_tool_call_payload || !object.contains_key("result") {
        return false;
    }

    object
        .keys()
        .all(|key| matches!(key.as_str(), "result" | "id" | "tool_call_id" | "name" | "tool"))
}

fn sanitize_tool_json_value(
    value: &serde_json::Value,
    known_tool_names: &HashSet<String>,
    saw_tool_call_payload: bool,
) -> Option<(String, bool)> {
    if is_tool_call_payload(value, known_tool_names) {
        return Some((String::new(), true));
    }

    if let Some(array) = value.as_array() {
        if !array.is_empty() && array.iter().all(|item| is_tool_call_payload(item, known_tool_names)) {
            return Some((String::new(), true));
        }
        return None;
    }

    let Some(object) = value.as_object() else {
        return None;
    };

    if let Some(tool_calls) = object.get("tool_calls").and_then(|value| value.as_array()) {
        if !tool_calls.is_empty()
            && tool_calls
                .iter()
                .all(|call| is_tool_call_payload(call, known_tool_names))
        {
            let content = object
                .get("content")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            return Some((content, true));
        }
    }

    if is_tool_result_payload(object, saw_tool_call_payload) {
        return Some((String::new(), false));
    }

    None
}

fn is_line_isolated_json_segment(message: &str, start: usize, end: usize) -> bool {
    let line_start = message[..start].rfind('\n').map_or(0, |idx| idx + 1);
    let line_end = message[end..].find('\n').map_or(message.len(), |idx| end + idx);

    message[line_start..start].trim().is_empty() && message[end..line_end].trim().is_empty()
}

pub(crate) fn strip_isolated_tool_json_artifacts(message: &str, known_tool_names: &HashSet<String>) -> String {
    strip_isolated_tool_json_artifacts_inner(message, known_tool_names, true)
}

/// Streaming-safe variant of [`strip_isolated_tool_json_artifacts`].
///
/// Identical to the public function except that it preserves the original
/// chunk's leading and trailing whitespace (and newlines). This matters on
/// the streaming hot path where consuming a single chunk's surrounding
/// whitespace would corrupt code-block indentation, eat line breaks, or
/// strip the deliberate spacing around tokens like `" 2 < 3 \n"`.
///
/// Used by [`crate::agent::sanitize::sanitize_stream_chunk`]. Do not use for
/// post-hoc whole-response sanitisation — the full-document pass intentionally
/// trims surrounding whitespace and should continue to do so.
pub(crate) fn strip_isolated_tool_json_artifacts_preserve_whitespace(
    message: &str,
    known_tool_names: &HashSet<String>,
) -> String {
    strip_isolated_tool_json_artifacts_inner(message, known_tool_names, false)
}

fn strip_isolated_tool_json_artifacts_inner(message: &str, known_tool_names: &HashSet<String>, trim: bool) -> String {
    let mut cleaned = String::with_capacity(message.len());
    let mut cursor = 0usize;
    let mut saw_tool_call_payload = false;

    while cursor < message.len() {
        let Some(rel_start) = message[cursor..].find(|ch: char| ch == '{' || ch == '[') else {
            cleaned.push_str(&message[cursor..]);
            break;
        };

        let start = cursor + rel_start;
        cleaned.push_str(&message[cursor..start]);

        let candidate = &message[start..];
        let mut stream = serde_json::Deserializer::from_str(candidate).into_iter::<serde_json::Value>();

        if let Some(Ok(value)) = stream.next() {
            let consumed = stream.byte_offset();
            if consumed > 0 {
                let end = start + consumed;
                if is_line_isolated_json_segment(message, start, end) {
                    if let Some((replacement, marks_tool_call)) =
                        sanitize_tool_json_value(&value, known_tool_names, saw_tool_call_payload)
                    {
                        if marks_tool_call {
                            saw_tool_call_payload = true;
                        }
                        if !replacement.trim().is_empty() {
                            cleaned.push_str(replacement.trim());
                        }
                        cursor = end;
                        continue;
                    }
                }
            }
        }

        let Some(ch) = message[start..].chars().next() else {
            break;
        };
        cleaned.push(ch);
        cursor = start + ch.len_utf8();
    }

    let mut result = cleaned.replace("\r\n", "\n");
    while result.contains("\n\n\n") {
        result = result.replace("\n\n\n", "\n\n");
    }
    if trim { result.trim().to_string() } else { result }
}

fn tool_call_close_tag_for_name(name: &str) -> Option<&'static str> {
    match name {
        "tool_call" => Some("</tool_call>"),
        "toolcall" => Some("</toolcall>"),
        "tool-call" => Some("</tool-call>"),
        "tool_use" => Some("</tool_use>"),
        "invoke" => Some("</invoke>"),
        _ => None,
    }
}

fn parse_tool_call_open_tag(tag: &str) -> Option<&'static str> {
    if !(tag.starts_with('<') && tag.ends_with('>')) {
        return None;
    }

    let mut inner = tag[1..tag.len() - 1].trim_start();
    if inner.starts_with('/') {
        return None;
    }
    if inner.ends_with('/') {
        inner = inner[..inner.len().saturating_sub(1)].trim_end();
    }

    let name = inner
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    tool_call_close_tag_for_name(&name)
}

fn find_first_tool_call_open_tag(haystack: &str) -> Option<(usize, usize, &'static str)> {
    let mut search_from = 0usize;
    while search_from < haystack.len() {
        let rel_lt = haystack[search_from..].find('<')?;
        let start = search_from + rel_lt;
        let rel_gt = haystack[start..].find('>')?;
        let end_exclusive = start + rel_gt + 1;
        let tag = &haystack[start..end_exclusive];
        if let Some(close_tag) = parse_tool_call_open_tag(tag) {
            return Some((start, end_exclusive, close_tag));
        }
        search_from = end_exclusive;
    }
    None
}

fn parse_first_json_value(input: &str) -> Option<serde_json::Value> {
    let trimmed = input.trim_start();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return Some(value);
    }

    for (byte_idx, ch) in trimmed.char_indices() {
        if ch != '{' && ch != '[' {
            continue;
        }
        let slice = &trimmed[byte_idx..];
        let mut stream = serde_json::Deserializer::from_str(slice).into_iter::<serde_json::Value>();
        if let Some(Ok(value)) = stream.next() {
            return Some(value);
        }
    }
    None
}

pub(crate) fn strip_isolated_tool_tag_artifacts(message: &str, known_tool_names: &HashSet<String>) -> String {
    strip_isolated_tool_tag_artifacts_inner(message, known_tool_names, true)
}

/// Streaming-safe variant of [`strip_isolated_tool_tag_artifacts`].
///
/// Identical to the public function except that it preserves the original
/// chunk's leading and trailing whitespace. See
/// [`strip_isolated_tool_json_artifacts_preserve_whitespace`] for the
/// rationale.
pub(crate) fn strip_isolated_tool_tag_artifacts_preserve_whitespace(
    message: &str,
    known_tool_names: &HashSet<String>,
) -> String {
    strip_isolated_tool_tag_artifacts_inner(message, known_tool_names, false)
}

fn strip_isolated_tool_tag_artifacts_inner(message: &str, known_tool_names: &HashSet<String>, trim: bool) -> String {
    let mut cleaned = String::with_capacity(message.len());
    let mut cursor = 0usize;

    while cursor < message.len() {
        let Some((start_rel, open_end_rel, close_tag)) = find_first_tool_call_open_tag(&message[cursor..]) else {
            cleaned.push_str(&message[cursor..]);
            break;
        };

        let start = cursor + start_rel;
        let open_end = cursor + open_end_rel;
        cleaned.push_str(&message[cursor..start]);

        let Some(close_idx_rel) = message[open_end..].find(close_tag) else {
            cleaned.push_str(&message[start..]);
            break;
        };
        let close_start = open_end + close_idx_rel;
        let close_end = close_start + close_tag.len();
        let inner = &message[open_end..close_start];

        let should_strip = if is_line_isolated_json_segment(message, start, close_end) {
            parse_first_json_value(inner)
                .as_ref()
                .is_some_and(|value| is_tool_call_payload(value, known_tool_names))
        } else {
            false
        };

        if should_strip {
            cursor = close_end;
            continue;
        }

        cleaned.push_str(&message[start..close_end]);
        cursor = close_end;
    }

    let mut result = cleaned.replace("\r\n", "\n");
    while result.contains("\n\n\n") {
        result = result.replace("\n\n\n", "\n\n");
    }
    if trim { result.trim().to_string() } else { result }
}
