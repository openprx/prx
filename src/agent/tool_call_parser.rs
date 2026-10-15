use crate::agent::diagnostics::sanitize_tool_parse_log_preview;
use crate::providers::ToolCall;
use regex::{Regex, RegexSet};
use std::sync::LazyLock;

fn parse_arguments_value(raw: Option<&serde_json::Value>) -> serde_json::Value {
    match raw {
        Some(serde_json::Value::String(s)) => serde_json::from_str::<serde_json::Value>(s)
            .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new())),
        Some(value) => value.clone(),
        None => serde_json::Value::Object(serde_json::Map::new()),
    }
}

fn parse_tool_call_value(value: &serde_json::Value) -> Option<ParsedToolCall> {
    if let Some(function) = value.get("function") {
        let name = function
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if !name.is_empty() {
            let arguments = parse_arguments_value(function.get("arguments").or_else(|| function.get("parameters")));
            return Some(ParsedToolCall { name, arguments });
        }
    }

    let name = value
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();

    if name.is_empty() {
        return None;
    }

    let arguments = parse_arguments_value(value.get("arguments").or_else(|| value.get("parameters")));
    Some(ParsedToolCall { name, arguments })
}

fn parse_tool_calls_from_json_value(value: &serde_json::Value) -> Vec<ParsedToolCall> {
    let mut calls = Vec::new();

    if let Some(tool_calls) = value.get("tool_calls").and_then(|v| v.as_array()) {
        for call in tool_calls {
            if let Some(parsed) = parse_tool_call_value(call) {
                calls.push(parsed);
            }
        }

        if !calls.is_empty() {
            return calls;
        }
    }

    if let Some(array) = value.as_array() {
        for item in array {
            if let Some(parsed) = parse_tool_call_value(item) {
                calls.push(parsed);
            }
        }
        return calls;
    }

    if let Some(parsed) = parse_tool_call_value(value) {
        calls.push(parsed);
    }

    calls
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

fn extract_first_json_value_with_end(input: &str) -> Option<(serde_json::Value, usize)> {
    let trimmed = input.trim_start();
    let trim_offset = input.len().saturating_sub(trimmed.len());

    for (byte_idx, ch) in trimmed.char_indices() {
        if ch != '{' && ch != '[' {
            continue;
        }

        let slice = &trimmed[byte_idx..];
        let mut stream = serde_json::Deserializer::from_str(slice).into_iter::<serde_json::Value>();
        if let Some(Ok(value)) = stream.next() {
            let consumed = stream.byte_offset();
            if consumed > 0 {
                return Some((value, trim_offset + byte_idx + consumed));
            }
        }
    }

    None
}

fn strip_leading_close_tags(mut input: &str) -> &str {
    loop {
        let trimmed = input.trim_start();
        if !trimmed.starts_with("</") {
            return trimmed;
        }

        let Some(close_end) = trimmed.find('>') else {
            return "";
        };
        input = &trimmed[close_end + 1..];
    }
}

/// Extract JSON values from a string.
///
/// # Security Warning
///
/// This function extracts ANY JSON objects/arrays from the input. It MUST only
/// be used on content that is already trusted to be from the LLM, such as
/// content inside `<invoke>` tags where the LLM has explicitly indicated intent
/// to make a tool call. Do NOT use this on raw user input or content that
/// could contain prompt injection payloads.
fn extract_json_values(input: &str) -> Vec<serde_json::Value> {
    let mut values = Vec::new();
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return values;
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        values.push(value);
        return values;
    }

    let char_positions: Vec<(usize, char)> = trimmed.char_indices().collect();
    let mut idx = 0;
    while let Some(&(byte_idx, ch)) = char_positions.get(idx) {
        if ch == '{' || ch == '[' {
            let slice = &trimmed[byte_idx..];
            let mut stream = serde_json::Deserializer::from_str(slice).into_iter::<serde_json::Value>();
            if let Some(Ok(value)) = stream.next() {
                let consumed = stream.byte_offset();
                if consumed > 0 {
                    values.push(value);
                    let next_byte = byte_idx + consumed;
                    while char_positions.get(idx).is_some_and(|&(pos, _)| pos < next_byte) {
                        idx += 1;
                    }
                    continue;
                }
            }
        }
        idx += 1;
    }

    values
}

/// Find the end position of a JSON object by tracking balanced braces.
fn find_json_end(input: &str) -> Option<usize> {
    let trimmed = input.trim_start();
    let offset = input.len() - trimmed.len();

    if !trimmed.starts_with('{') {
        return None;
    }

    let mut depth = 0;
    let mut in_string = false;
    let mut escape_next = false;

    for (i, ch) in trimmed.char_indices() {
        if escape_next {
            escape_next = false;
            continue;
        }

        match ch {
            '\\' if in_string => escape_next = true,
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(offset + i + ch.len_utf8());
                }
            }
            _ => {}
        }
    }

    None
}

/// Parse GLM-style tool calls from response text.
/// GLM uses proprietary formats like:
/// - `browser_open/url>https://example.com`
/// - `shell/command>ls -la`
/// - `http_request/url>https://api.example.com`
///
/// The `browser_open`/`browser` aliases are retained even though the dedicated
/// browser tools were removed: GLM-family models still emit those hallucinated
/// tool names, and routing them to a `shell` curl invocation keeps that quirk
/// working through the agent's general shell capability.
fn map_glm_tool_alias(tool_name: &str) -> &str {
    match tool_name {
        "browser_open" | "browser" | "web_search" | "shell" | "bash" => "shell",
        "http_request" | "http" => "http_request",
        _ => tool_name,
    }
}

fn build_curl_command(url: &str) -> Option<String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }

    if url.chars().any(char::is_whitespace) {
        return None;
    }

    let escaped = url.replace('\'', r#"'\\''"#);
    Some(format!("curl -s '{}'", escaped))
}

fn parse_glm_style_tool_calls(text: &str) -> Vec<(String, serde_json::Value, Option<String>)> {
    let mut calls = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Format: tool_name/param>value or tool_name/{json}
        if let Some(pos) = line.find('/') {
            let tool_part = &line[..pos];
            let rest = &line[pos + 1..];

            if tool_part.chars().all(|c| c.is_alphanumeric() || c == '_') {
                let tool_name = map_glm_tool_alias(tool_part);

                if let Some(gt_pos) = rest.find('>') {
                    let param_name = rest[..gt_pos].trim();
                    let value = rest[gt_pos + 1..].trim();

                    let arguments = match tool_name {
                        "shell" => {
                            if param_name == "url" {
                                let Some(command) = build_curl_command(value) else {
                                    continue;
                                };
                                serde_json::json!({"command": command})
                            } else if value.starts_with("http://") || value.starts_with("https://") {
                                build_curl_command(value).map_or_else(
                                    || serde_json::json!({"command": value}),
                                    |command| serde_json::json!({"command": command}),
                                )
                            } else {
                                serde_json::json!({"command": value})
                            }
                        }
                        "http_request" => {
                            serde_json::json!({"url": value, "method": "GET"})
                        }
                        _ => serde_json::json!({param_name: value}),
                    };

                    calls.push((tool_name.to_string(), arguments, Some(line.to_string())));
                    continue;
                }

                if rest.starts_with('{') {
                    if let Ok(json_args) = serde_json::from_str::<serde_json::Value>(rest) {
                        calls.push((tool_name.to_string(), json_args, Some(line.to_string())));
                    }
                }
            }
        }

        // Plain URL
        if let Some(command) = build_curl_command(line) {
            calls.push((
                "shell".to_string(),
                serde_json::json!({"command": command}),
                Some(line.to_string()),
            ));
        }
    }

    calls
}

fn strip_markdown_fence_block(input: &str) -> String {
    let trimmed = input.trim();
    if !trimmed.starts_with("```") {
        return trimmed.to_string();
    }

    let Some(first_newline) = trimmed.find('\n') else {
        return trimmed.to_string();
    };
    let content = &trimmed[first_newline + 1..];
    let content = content.rfind("```").map_or(content, |end| &content[..end]);
    content.trim().to_string()
}

fn parse_recipient_tool_calls(recipient: &str, body: &str) -> Vec<ParsedToolCall> {
    let mut calls = Vec::new();
    let recipient = recipient.trim();
    if recipient.is_empty() {
        return calls;
    }

    for value in extract_json_values(body) {
        let parsed_calls = parse_tool_calls_from_json_value(&value);
        if !parsed_calls.is_empty() {
            calls.extend(parsed_calls);
            continue;
        }

        if value.is_object() {
            calls.push(ParsedToolCall {
                name: recipient.to_string(),
                arguments: value,
            });
        }
    }

    if !calls.is_empty() {
        return calls;
    }

    let cleaned_body = strip_markdown_fence_block(body);
    if cleaned_body.is_empty() {
        return calls;
    }

    if recipient == "shell" || recipient == "bash" {
        calls.push(ParsedToolCall {
            name: "shell".to_string(),
            arguments: serde_json::json!({ "command": cleaned_body }),
        });
    }

    calls
}

fn parse_codex_to_style_tool_calls(text: &str) -> (String, Vec<ParsedToolCall>) {
    #[allow(clippy::expect_used)]
    static CODEX_TO_HEADER_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)^\s*(?:assistant\s+)?to=([a-zA-Z0-9_.-]+)\s+code\s*$"#)
            .expect("compile regex: codex to=recipient header")
    });

    let mut calls = Vec::new();
    let mut text_parts = Vec::new();
    let mut plain_text = String::new();
    let mut current_recipient: Option<String> = None;
    let mut current_body = String::new();

    let flush_current = |recipient: &mut Option<String>, body: &mut String, out_calls: &mut Vec<ParsedToolCall>| {
        if let Some(recipient) = recipient.take() {
            out_calls.extend(parse_recipient_tool_calls(&recipient, body));
            body.clear();
        }
    };

    for line in text.lines() {
        if let Some(cap) = CODEX_TO_HEADER_RE.captures(line) {
            flush_current(&mut current_recipient, &mut current_body, &mut calls);
            current_recipient = cap.get(1).map(|m| m.as_str().to_string());
            continue;
        }

        if current_recipient.is_some() {
            if !current_body.is_empty() {
                current_body.push('\n');
            }
            current_body.push_str(line);
        } else {
            if !plain_text.is_empty() {
                plain_text.push('\n');
            }
            plain_text.push_str(line);
        }
    }
    flush_current(&mut current_recipient, &mut current_body, &mut calls);

    if !calls.is_empty() {
        if !plain_text.trim().is_empty() {
            text_parts.push(plain_text.trim().to_string());
        }
    } else if !text.trim().is_empty() {
        text_parts.push(text.trim().to_string());
    }

    (text_parts.join("\n"), calls)
}

fn parse_assistant_recipient_tool_calls(text: &str) -> (String, Vec<ParsedToolCall>) {
    #[allow(clippy::expect_used)]
    static ASSISTANT_RECIPIENT_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?is)<assistant\b([^>]*)>(.*?)</assistant>"#).expect("compile regex: assistant XML tag pattern")
    });
    #[allow(clippy::expect_used)]
    static RECIPIENT_ATTR_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)\brecipient\s*=\s*["']?([a-zA-Z0-9_.-]+)["']?"#)
            .expect("compile regex: recipient attribute pattern")
    });

    let mut calls = Vec::new();
    let mut text_parts = Vec::new();
    let mut last_end = 0usize;

    for cap in ASSISTANT_RECIPIENT_RE.captures_iter(text) {
        let Some(full_match) = cap.get(0) else {
            continue;
        };
        let before = &text[last_end..full_match.start()];
        if !before.trim().is_empty() {
            text_parts.push(before.trim().to_string());
        }

        let attrs = cap.get(1).map_or("", |m| m.as_str());
        let body = cap.get(2).map_or("", |m| m.as_str());
        if let Some(recipient_match) = RECIPIENT_ATTR_RE.captures(attrs) {
            if let Some(recipient) = recipient_match.get(1).map(|m| m.as_str()) {
                calls.extend(parse_recipient_tool_calls(recipient, body));
            }
        }

        last_end = full_match.end();
    }

    if !calls.is_empty() {
        let tail = &text[last_end..];
        if !tail.trim().is_empty() {
            text_parts.push(tail.trim().to_string());
        }
    } else if !text.trim().is_empty() {
        text_parts.push(text.trim().to_string());
    }

    (text_parts.join("\n"), calls)
}

pub(super) fn looks_like_unparsed_tool_call_syntax(text: &str) -> bool {
    // Every XML alternative requires '<'; the Codex alternative requires '='.
    // These punctuation literals are unchanged by Unicode case folding.
    // Keep the original matcher for every possible match, including malformed
    // marker-bearing input, without initializing its regexes for plain prose.
    if !text.bytes().any(|byte| matches!(byte, b'<' | b'=')) {
        return false;
    }
    #[allow(clippy::expect_used)]
    static COMPLETE_TOOL_SYNTAX: LazyLock<RegexSet> = LazyLock::new(|| {
        RegexSet::new([
            r"(?is)<tool[_-]?call\b[^>]*>\s*(?:\{[\s\S]*?\}|\[[\s\S]*?\])\s*</tool[_-]?call>",
            r"(?is)<toolcall\b[^>]*>\s*(?:\{[\s\S]*?\}|\[[\s\S]*?\])\s*</toolcall>",
            r"(?is)<tool_use\b[^>]*>\s*(?:\{[\s\S]*?\}|\[[\s\S]*?\])\s*</tool_use>",
            r"(?is)<invoke\b[^>]*>\s*(?:\{[\s\S]*?\}|\[[\s\S]*?\])\s*</invoke>",
            r#"(?is)<assistant\b[^>]*\brecipient\s*=\s*["']?[a-zA-Z0-9_.-]+["']?[^>]*>\s*(?:\{[\s\S]*?\}|\[[\s\S]*?\]|```[\s\S]*?```)\s*</assistant>"#,
            r"(?is)(?:^|\n)\s*(?:assistant\s+)?to=[a-zA-Z0-9_.-]+\s+code\s*(?:\r?\n)+\s*(?:```(?:json|tool[_-]?call|toolcall|invoke)?\s*(?:\r?\n)+)?\s*(?:\{|\[)",
        ])
        .expect("compile regex set: complete tool syntax patterns")
    });
    COMPLETE_TOOL_SYNTAX.is_match(text)
}

// ── Tool-Call Parsing ─────────────────────────────────────────────────────
// LLM responses may contain tool calls in multiple formats depending on
// the provider. Parsing follows a priority chain:
//   1. OpenAI-style JSON with `tool_calls` array (native API)
//   2. XML tags: <tool_call>, <toolcall>, <tool-call>, <tool_use>, <invoke>
//   3. Markdown code blocks with `tool_call` language
//   4. GLM-style line-based format (e.g. `shell/command>ls`)
// SECURITY: We never fall back to extracting arbitrary JSON from the
// response body, because that would enable prompt-injection attacks where
// malicious content in emails/files/web pages mimics a tool call.

/// Parse tool calls from an LLM response that uses XML-style function calling.
///
/// Expected format (common with system-prompt-guided tool use):
/// ```text
/// <tool_call>
/// {"name": "shell", "arguments": {"command": "ls"}}
/// </tool_call>
/// ```
///
/// Also accepts common tag variants (`<toolcall>`, `<tool-call>`) for model
/// compatibility.
///
/// Also supports JSON with `tool_calls` array from OpenAI-format responses.
pub(super) fn parse_tool_calls(response: &str) -> (String, Vec<ParsedToolCall>) {
    let mut text_parts = Vec::new();
    let mut calls = Vec::new();
    let mut remaining = response;

    // First, try to parse as OpenAI-style JSON response with tool_calls array
    // This handles providers like Minimax that return tool_calls in native JSON format
    if let Ok(json_value) = serde_json::from_str::<serde_json::Value>(response.trim()) {
        calls = parse_tool_calls_from_json_value(&json_value);
        if !calls.is_empty() {
            // If we found tool_calls, extract any content field as text
            if let Some(content) = json_value.get("content").and_then(|v| v.as_str()) {
                if !content.trim().is_empty() {
                    text_parts.push(content.trim().to_string());
                }
            }
            return (text_parts.join("\n"), calls);
        }
    }

    // Fall back to XML-style tool-call tag parsing.
    while let Some((start, open_end, close_tag)) = find_first_tool_call_open_tag(remaining) {
        // Everything before the tag is text
        let before = &remaining[..start];
        if !before.trim().is_empty() {
            text_parts.push(before.trim().to_string());
        }

        let after_open = &remaining[open_end..];
        if let Some(close_idx) = after_open.find(close_tag) {
            let inner = &after_open[..close_idx];
            let mut parsed_any = false;
            let json_values = extract_json_values(inner);
            for value in json_values {
                let parsed_calls = parse_tool_calls_from_json_value(&value);
                if !parsed_calls.is_empty() {
                    parsed_any = true;
                    calls.extend(parsed_calls);
                }
            }

            if !parsed_any {
                let sanitized_preview = sanitize_tool_parse_log_preview(inner);
                tracing::error!(
                    raw_tool_call_body = sanitized_preview.as_str(),
                    "Malformed <tool_call> JSON: expected tool-call object in tag body"
                );
            }

            remaining = &after_open[close_idx + close_tag.len()..];
        } else {
            if let Some(json_end) = find_json_end(after_open) {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&after_open[..json_end]) {
                    let parsed_calls = parse_tool_calls_from_json_value(&value);
                    if !parsed_calls.is_empty() {
                        calls.extend(parsed_calls);
                        remaining = strip_leading_close_tags(&after_open[json_end..]);
                        continue;
                    }
                }
            }

            if let Some((value, consumed_end)) = extract_first_json_value_with_end(after_open) {
                let parsed_calls = parse_tool_calls_from_json_value(&value);
                if !parsed_calls.is_empty() {
                    calls.extend(parsed_calls);
                    remaining = strip_leading_close_tags(&after_open[consumed_end..]);
                    continue;
                }
            }

            remaining = &remaining[start..];
            break;
        }
    }

    // If XML tags found nothing, try markdown code blocks with tool_call language.
    // Models behind OpenRouter sometimes output ```tool_call ... ``` or hybrid
    // ```tool_call ... </tool_call> instead of structured API calls or XML tags.
    if calls.is_empty() {
        #[allow(clippy::expect_used)]
        static MD_TOOL_CALL_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(
                r"(?s)```(?:tool[_-]?call|invoke)\s*\n(.*?)(?:```|</tool[_-]?call>|</toolcall>|</tool_use>|</invoke>)",
            )
            .expect("compile regex: markdown tool_call block pattern")
        });
        let mut md_text_parts: Vec<String> = Vec::new();
        let mut last_end = 0;

        for cap in MD_TOOL_CALL_RE.captures_iter(response) {
            // cap.get(0) is the full match, always present for captures_iter results
            if let Some(full_match) = cap.get(0) {
                let before = &response[last_end..full_match.start()];
                if !before.trim().is_empty() {
                    md_text_parts.push(before.trim().to_string());
                }
                let inner = &cap[1];
                let json_values = extract_json_values(inner);
                for value in json_values {
                    let parsed_calls = parse_tool_calls_from_json_value(&value);
                    calls.extend(parsed_calls);
                }
                last_end = full_match.end();
            }
        }

        if !calls.is_empty() {
            let after = &response[last_end..];
            if !after.trim().is_empty() {
                md_text_parts.push(after.trim().to_string());
            }
            text_parts = md_text_parts;
            remaining = "";
        }
    }

    // OpenAI Codex text protocol: `assistant to=<tool> code ...`
    if calls.is_empty() {
        let (parsed_text, parsed_calls) = parse_codex_to_style_tool_calls(response);
        if !parsed_calls.is_empty() {
            calls = parsed_calls;
            if !parsed_text.is_empty() {
                text_parts = vec![parsed_text];
            } else {
                text_parts.clear();
            }
            remaining = "";
        }
    }

    // Assistant-recipient XML protocol: <assistant recipient="tool">...</assistant>
    if calls.is_empty() {
        let (parsed_text, parsed_calls) = parse_assistant_recipient_tool_calls(response);
        if !parsed_calls.is_empty() {
            calls = parsed_calls;
            if !parsed_text.is_empty() {
                text_parts = vec![parsed_text];
            } else {
                text_parts.clear();
            }
            remaining = "";
        }
    }

    // GLM-style tool calls (browser_open/url>https://..., shell/command>ls, etc.)
    if calls.is_empty() {
        let glm_calls = parse_glm_style_tool_calls(remaining);
        if !glm_calls.is_empty() {
            let mut cleaned_text = remaining.to_string();
            for (name, args, raw) in &glm_calls {
                calls.push(ParsedToolCall {
                    name: name.clone(),
                    arguments: args.clone(),
                });
                if let Some(r) = raw {
                    cleaned_text = cleaned_text.replace(r, "");
                }
            }
            if !cleaned_text.trim().is_empty() {
                text_parts.push(cleaned_text.trim().to_string());
            }
            remaining = "";
        }
    }

    // SECURITY: We do NOT fall back to extracting arbitrary JSON from the response
    // here. That would enable prompt injection attacks where malicious content
    // (e.g., in emails, files, or web pages) could include JSON that mimics a
    // tool call. Tool calls MUST be explicitly wrapped in either:
    // 1. OpenAI-style JSON with a "tool_calls" array
    // 2. OpenPRX tool-call tags (<tool_call>, <toolcall>, <tool-call>)
    // 3. Markdown code blocks with tool_call/toolcall/tool-call language
    // 4. Explicit GLM line-based call formats (e.g. `shell/command>...`)
    // This ensures only the LLM's intentional tool calls are executed.

    // Remaining text after last tool call
    if !remaining.trim().is_empty() {
        text_parts.push(remaining.trim().to_string());
    }

    (text_parts.join("\n"), calls)
}

/// Unparseable arguments become `{}` here only so the call keeps its slot and
/// id in the transcript; [`unparseable_native_arguments`] refuses to run it.
pub(super) fn parse_structured_tool_calls(tool_calls: &[ToolCall]) -> Vec<ParsedToolCall> {
    tool_calls
        .iter()
        .map(|call| ParsedToolCall {
            name: call.name.clone(),
            arguments: serde_json::from_str::<serde_json::Value>(&call.arguments)
                .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new())),
        })
        .collect()
}

/// The error a native tool call gets instead of running when its arguments are
/// not a complete JSON object: invalid JSON (what a truncated or interrupted
/// provider stream leaves behind) or a non-object value such as `[1]`, which
/// no tool accepts and Anthropic rejects as `input`. Running it as `{}` would
/// execute a call the model never finished; the transcript replays it as `{}`
/// and this result says why. Empty arguments stay a call with no arguments.
pub(super) fn unparseable_native_arguments(call: &ToolCall) -> Option<String> {
    let raw = call.arguments.trim();
    if raw.is_empty() {
        return None;
    }
    let why = match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(serde_json::Value::Object(_)) => return None,
        Ok(serde_json::Value::Array(_)) => "got an array".to_string(),
        Ok(serde_json::Value::String(_)) => "got a string".to_string(),
        Ok(serde_json::Value::Number(_)) => "got a number".to_string(),
        Ok(serde_json::Value::Bool(_)) => "got a boolean".to_string(),
        Ok(serde_json::Value::Null) => "got null".to_string(),
        Err(error) => error.to_string(),
    };
    Some(format!(
        "Error: tool '{}' was not executed: its arguments were not a complete JSON object ({why}), so the \
         transcript shows them as {{}}. The output was probably truncated or interrupted; issue the call again \
         with complete arguments.",
        call.name
    ))
}

#[derive(Debug, Clone)]
pub(super) struct ParsedToolCall {
    pub(super) name: String,
    pub(super) arguments: serde_json::Value,
}

#[path = "turn_tests/tool_call_parser.rs"]
mod tests;
