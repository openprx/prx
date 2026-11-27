//! Opt-in debug dump of each compiled model request.
//!
//! `OPENPRX_DEBUG_REQUEST_DUMP=<dir>` makes the engine write one masked JSON
//! file per compiled request into `<dir>`. The variable is read once per
//! process; unset or empty means nothing happens. Every string that holds
//! text or arguments goes through the secret redaction before serialising.
//! A write failure logs one warning per process (error kind only) and never
//! affects the turn.

use crate::providers::ChatMessage;
use crate::security::redaction::redact_secrets;
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

const ENV_VAR: &str = "OPENPRX_DEBUG_REQUEST_DUMP";

static DUMP_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
static WARNED: AtomicBool = AtomicBool::new(false);

fn dump_dir() -> Option<&'static Path> {
    DUMP_DIR
        .get_or_init(|| {
            std::env::var_os(ENV_VAR)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .as_deref()
}

/// What one dump needs from the request site.
pub(crate) struct DumpFacts<'a> {
    pub(crate) run_id: Option<&'a str>,
    /// The round number inside the run, starting at 1.
    pub(crate) round: usize,
    pub(crate) provider: &'a str,
    pub(crate) model: &'a str,
    /// The messages as they are sent.
    pub(crate) messages: &'a [ChatMessage],
    /// Names of the tools the request publishes.
    pub(crate) tool_names: Vec<&'a str>,
}

/// Writes the dump of one request when the variable is set.
pub(crate) fn dump_request(facts: &DumpFacts<'_>) {
    let Some(dir) = dump_dir() else {
        return;
    };
    if let Err(kind) = write_dump(dir, facts) {
        if !WARNED.swap(true, Ordering::Relaxed) {
            tracing::warn!(error_kind = ?kind, "Request dump write failed; later failures are not logged");
        }
    }
}

fn file_stem(run_id: Option<&str>) -> String {
    match run_id {
        Some(id) => id
            .bytes()
            .map(|byte| {
                if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' {
                    char::from(byte)
                } else {
                    '_'
                }
            })
            .collect(),
        None => {
            let millis = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_millis());
            format!("norun-{millis}")
        }
    }
}

fn masked(text: &str) -> String {
    redact_secrets(text)
}

fn dump_json(facts: &DumpFacts<'_>) -> Value {
    let system: Vec<String> = facts
        .messages
        .iter()
        .filter(|message| message.role == "system")
        .map(|message| masked(&message.content))
        .collect();
    let messages: Vec<Value> = facts
        .messages
        .iter()
        .filter(|message| message.role != "system")
        .map(|message| json!({ "role": masked(&message.role), "content": masked(&message.content) }))
        .collect();
    let tools: Vec<String> = facts.tool_names.iter().map(|name| masked(name)).collect();
    json!({
        "model": masked(facts.model),
        "provider": masked(facts.provider),
        "system": system.join("\n\n"),
        "messages": messages,
        "tools": tools,
    })
}

fn create_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

fn create_new_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn write_dump(dir: &Path, facts: &DumpFacts<'_>) -> Result<(), std::io::ErrorKind> {
    let body = serde_json::to_vec_pretty(&dump_json(facts)).map_err(|_| std::io::ErrorKind::InvalidData)?;
    create_dir(dir).map_err(|error| error.kind())?;
    let base = format!("{}-{}", file_stem(facts.run_id), facts.round);
    let mut attempt = 0usize;
    loop {
        let name = if attempt == 0 {
            format!("{base}.json")
        } else {
            format!("{base}-{attempt}.json")
        };
        match create_new_file(&dir.join(name)) {
            Ok(mut file) => return file.write_all(&body).map_err(|error| error.kind()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                attempt = attempt.saturating_add(1);
            }
            Err(error) => return Err(error.kind()),
        }
    }
}
