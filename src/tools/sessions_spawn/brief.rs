//! Renders the optional `brief` and `references` arguments of `sessions_spawn`
//! into the child's first message, after the task text.
//!
//! Only size and shape rules apply here. No reference is checked for existence
//! or readability: the child resolves references with its own tools, which
//! report what they can read.

use serde_json::{Map, Value};

/// Maximum characters in each brief field.
const BRIEF_FIELD_MAX_CHARS: usize = 4_000;
/// Maximum characters in a reference note.
const REFERENCE_NOTE_MAX_CHARS: usize = 200;
/// Maximum number of references in one spawn.
const REFERENCES_MAX: usize = 32;

/// Brief keys in rendering order, each with its label in the child's message.
const BRIEF_FIELDS: [(&str, &str); 3] = [
    ("objective", "Objective"),
    ("expected_output", "Expected output"),
    ("boundaries", "Boundaries"),
];

/// Renders `task` followed by the `brief` and `references` present in `args`.
///
/// Returns `Ok(None)` when neither key is present or both are empty, so the
/// caller keeps `task` unchanged. Returns `Err` with a message naming the
/// offending field when a size bound or a shape rule is violated.
pub(super) fn render_task(task: &str, args: &Value) -> Result<Option<String>, String> {
    let brief_lines = render_brief(args.get("brief"))?;
    let reference_lines = render_references(args.get("references"))?;
    if brief_lines.is_empty() && reference_lines.is_empty() {
        return Ok(None);
    }
    let mut rendered = task.to_owned();
    if !brief_lines.is_empty() {
        rendered.push_str("\n\n[Brief]\n");
        rendered.push_str(&brief_lines.join("\n"));
    }
    if !reference_lines.is_empty() {
        rendered.push_str("\n\n[References]\n");
        rendered.push_str(&reference_lines.join("\n"));
    }
    Ok(Some(rendered))
}

fn render_brief(value: Option<&Value>) -> Result<Vec<String>, String> {
    let Some(value) = value.filter(|v| !is_absent(v)) else {
        return Ok(Vec::new());
    };
    let fields = value.as_object().ok_or("'brief' must be an object")?;
    if let Some(key) = fields
        .keys()
        .find(|key| !BRIEF_FIELDS.iter().any(|(name, _)| name == key))
    {
        return Err(format!("'brief.{key}' is not a brief field"));
    }
    let mut lines = Vec::new();
    for (name, label) in BRIEF_FIELDS {
        if let Some(text) = optional_text(fields, "brief", name)? {
            if text.chars().count() > BRIEF_FIELD_MAX_CHARS {
                return Err(format!(
                    "'brief.{name}' must be at most {BRIEF_FIELD_MAX_CHARS} characters"
                ));
            }
            lines.push(format!("{label}: {text}"));
        }
    }
    Ok(lines)
}

fn render_references(value: Option<&Value>) -> Result<Vec<String>, String> {
    let Some(value) = value.filter(|v| !is_absent(v)) else {
        return Ok(Vec::new());
    };
    let entries = value.as_array().ok_or("'references' must be an array")?;
    if entries.len() > REFERENCES_MAX {
        return Err(format!("'references' accepts at most {REFERENCES_MAX} entries"));
    }
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| render_reference(index, entry))
        .collect()
}

fn render_reference(index: usize, entry: &Value) -> Result<String, String> {
    let owner = format!("references[{index}]");
    let fields = entry
        .as_object()
        .ok_or_else(|| format!("'{owner}' must be an object"))?;
    let kind = optional_text(fields, &owner, "kind")?.ok_or_else(|| format!("'{owner}.kind' is required"))?;
    let line = match kind {
        "document" => {
            let id = optional_text(fields, &owner, "id")?
                .ok_or_else(|| format!("'{owner}' of kind 'document' requires 'id'"))?;
            format!("- document {id}: read it with document_get_chunk")
        }
        "file" => {
            let path = optional_text(fields, &owner, "path")?
                .ok_or_else(|| format!("'{owner}' of kind 'file' requires 'path'"))?;
            format!("- file {path}: read it with file_read")
        }
        "transcript" => {
            let session_key = optional_text(fields, &owner, "session_key")?
                .ok_or_else(|| format!("'{owner}' of kind 'transcript' requires 'session_key'"))?;
            format!("- transcript {session_key}: search it with transcript_search session_key={session_key}")
        }
        _ => {
            return Err(format!("'{owner}.kind' must be 'document', 'file' or 'transcript'"));
        }
    };
    match optional_text(fields, &owner, "note")? {
        Some(note) => {
            if note.chars().count() > REFERENCE_NOTE_MAX_CHARS {
                return Err(format!(
                    "'{owner}.note' must be at most {REFERENCE_NOTE_MAX_CHARS} characters"
                ));
            }
            Ok(format!("{line} ({note})"))
        }
        None => Ok(line),
    }
}

/// A top-level `brief` or `references` value that counts as not given: null,
/// a blank string or an empty array. Such a value renders nothing and leaves the
/// task unchanged; every other shape goes through the normal validation.
pub(super) fn is_absent(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(text) => text.trim().is_empty(),
        Value::Array(entries) => entries.is_empty(),
        _ => false,
    }
}

/// Reads an optional string field, trimmed. Absent, null and blank fields give
/// `None`; a non-string value is an error that names `owner.name`.
fn optional_text<'a>(fields: &'a Map<String, Value>, owner: &str, name: &str) -> Result<Option<&'a str>, String> {
    match fields.get(name).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(value) => {
            let text = value
                .as_str()
                .ok_or_else(|| format!("'{owner}.{name}' must be a string"))?
                .trim();
            Ok((!text.is_empty()).then_some(text))
        }
    }
}
