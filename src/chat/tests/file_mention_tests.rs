#![cfg(test)]

use super::*;
use crate::security::{AutonomyLevel, SecurityPolicy};
use crate::tools::FileReadTool;
use crate::tools::traits::{ToolCategory, ToolResult, ToolTier};
use async_trait::async_trait;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

struct MockReadTool {
    calls: Arc<AtomicUsize>,
    max_bytes_seen: Arc<Mutex<Vec<Option<u64>>>>,
}

#[async_trait]
impl Tool for MockReadTool {
    fn name(&self) -> &str {
        "file_read"
    }

    fn description(&self) -> &str {
        "mock file read"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.max_bytes_seen
            .lock()
            .push(args.get("max_bytes").and_then(serde_json::Value::as_u64));
        let path = args.get("path").and_then(serde_json::Value::as_str).unwrap_or_default();
        Ok(ToolResult {
            success: true,
            output: format!("content for {path}"),
            error: None,
        })
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Core
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::FileSystem]
    }
}

fn file_read_registry(workspace: &std::path::Path, acl_enabled: bool) -> Vec<Box<dyn Tool>> {
    let security = Arc::new(SecurityPolicy {
        autonomy: AutonomyLevel::Supervised,
        workspace_dir: workspace.to_path_buf(),
        ..SecurityPolicy::default()
    });
    vec![Box::new(FileReadTool::new(security, acl_enabled))]
}

#[test]
fn file_mentions_parse_multiple_non_ascii_and_ignore_email_bare_quote() {
    // Cyrillic stand-ins for non-ASCII text and a non-ASCII @path:
    // "\u{43f}\u{43e}\u{447}\u{442}\u{430}" (mail) and a path whose
    // directory and file name are both Cyrillic.
    let mentions = extract_file_mentions(
        "read @src/lib.rs and @./README.md \u{43f}\u{43e}\u{447}\u{442}\u{430} a@example.com bare @ \
             quoted @\"two words.txt\" @\u{43f}\u{443}\u{442}\u{44c}/\u{444}\u{430}\u{439}\u{43b}.rs",
    );

    assert_eq!(
        mentions,
        vec![
            FileMention {
                token: "@src/lib.rs".to_string(),
                path: "src/lib.rs".to_string(),
            },
            FileMention {
                token: "@./README.md".to_string(),
                path: "./README.md".to_string(),
            },
            FileMention {
                token: "@\u{43f}\u{443}\u{442}\u{44c}/\u{444}\u{430}\u{439}\u{43b}.rs".to_string(),
                path: "\u{43f}\u{443}\u{442}\u{44c}/\u{444}\u{430}\u{439}\u{43b}.rs".to_string(),
            },
        ]
    );
}

#[tokio::test]
async fn file_mention_success_uses_file_read_tool_and_preserves_original_text() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("note.txt"), "hello from mention").expect("write note");
    let registry = file_read_registry(temp.path(), false);

    let enriched = enrich_file_mentions_for_prompt("please inspect @note.txt", &registry).await;

    assert!(enriched.prompt.starts_with("please inspect @note.txt"));
    assert!(enriched.prompt.contains("hello from mention"));
    assert!(enriched.visible_note.is_none());
}

#[tokio::test]
async fn file_mention_security_negatives_are_soft_visible_and_generic() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(temp.path().join("memory")).expect("create memory");
    std::fs::write(temp.path().join("MEMORY.md"), "protected memory").expect("write memory");

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let outside = tempfile::tempdir().expect("outside tempdir");
        std::fs::write(outside.path().join("secret.txt"), "outside secret").expect("write outside");
        symlink(outside.path().join("secret.txt"), temp.path().join("escape.txt")).expect("symlink escape");

        let registry = file_read_registry(temp.path(), true);
        let enriched = enrich_file_mentions_for_prompt(
            "check @../../etc/passwd @/etc/passwd @escape.txt @MEMORY.md @missing.txt",
            &registry,
        )
        .await;

        assert!(enriched.prompt.starts_with("check @../../etc/passwd"));
        assert!(!enriched.prompt.contains("outside secret"));
        assert!(!enriched.prompt.contains("protected memory"));
        let note = enriched.visible_note.expect("visible failure notes");
        assert!(note.contains("@../../etc/passwd: unavailable (blocked by policy)"));
        assert!(note.contains("@/etc/passwd: unavailable (blocked by policy)"));
        assert!(note.contains("@escape.txt: unavailable (blocked by policy)"));
        assert!(note.contains("@MEMORY.md: unavailable (blocked by policy)"));
        assert!(note.contains("@missing.txt: unavailable (missing or inaccessible)"));
    }
}

#[tokio::test]
async fn file_mention_directory_is_rejected_softly() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(temp.path().join("dir")).expect("create dir");
    let registry = file_read_registry(temp.path(), false);

    let enriched = enrich_file_mentions_for_prompt("inspect @dir", &registry).await;

    let note = enriched.visible_note.expect("visible directory note");
    assert!(note.contains("@dir: unavailable"));
    assert!(enriched.prompt.starts_with("inspect @dir"));
}

#[cfg(feature = "terminal-tui")]
#[test]
fn at_path_candidates_are_relative_sorted_and_security_filtered() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(temp.path().join("src")).expect("create src");
    std::fs::write(temp.path().join("src").join("main.rs"), "fn main() {}\n").expect("write main");
    std::fs::write(temp.path().join("setup.rs"), "// setup\n").expect("write setup");

    #[cfg(unix)]
    let outside = tempfile::tempdir().expect("outside");
    #[cfg(unix)]
    {
        std::fs::write(outside.path().join("secret.rs"), "secret\n").expect("write outside");
        std::os::unix::fs::symlink(outside.path().join("secret.rs"), temp.path().join("secret.rs")).expect("symlink");
    }

    let policy = SecurityPolicy {
        autonomy: AutonomyLevel::Supervised,
        workspace_dir: temp.path().to_path_buf(),
        forbidden_paths: Vec::new(),
        ..SecurityPolicy::default()
    };
    let mut input = tui::TuiInput::new();
    input.set_text("@s");

    let candidates = collect_at_path_candidates(&input, temp.path(), &policy);

    let paths = candidates
        .iter()
        .map(|candidate| candidate.path.as_str())
        .collect::<Vec<_>>();
    assert_eq!(paths.first().copied(), Some("src/"), "directories sort first");
    assert!(paths.contains(&"setup.rs"));
    assert!(
        !paths.contains(&"secret.rs"),
        "symlink escaping workspace must be filtered by resolved-path policy"
    );

    input.set_text("@../");
    assert!(
        collect_at_path_candidates(&input, temp.path(), &policy).is_empty(),
        "traversal filter is blocked before directory enumeration"
    );
}

#[tokio::test]
async fn file_mention_caps_file_count_and_bytes() {
    let calls = Arc::new(AtomicUsize::new(0));
    let max_bytes_seen = Arc::new(Mutex::new(Vec::new()));
    let registry: Vec<Box<dyn Tool>> = vec![Box::new(MockReadTool {
        calls: Arc::clone(&calls),
        max_bytes_seen: Arc::clone(&max_bytes_seen),
    })];

    let enriched = enrich_file_mentions_for_prompt("x @a @b @c @d @e @f", &registry).await;

    assert_eq!(calls.load(Ordering::SeqCst), FILE_MENTION_MAX_FILES);
    assert_eq!(
        max_bytes_seen.lock().as_slice(),
        &[Some(FILE_MENTION_MAX_BYTES as u64); FILE_MENTION_MAX_FILES],
        "@path expansion must pass a file_read byte cap before prompt enrichment"
    );
    assert!(enriched.prompt.contains("content for a"));
    assert!(!enriched.prompt.contains("content for f"));
    assert!(enriched.prompt.contains("1 file mention(s) skipped"));
    assert!(
        enriched
            .visible_note
            .expect("visible max-files note")
            .contains("skipped")
    );
}

#[tokio::test]
async fn file_mention_truncates_utf8_on_char_boundary() {
    let temp = tempfile::tempdir().expect("tempdir");
    // `\u{20ac}` (EURO) is 3 bytes in UTF-8, so the 64 KiB cap lands mid-character.
    std::fs::write(temp.path().join("big.txt"), "\u{20ac}".repeat(30_000)).expect("write big");
    let registry = file_read_registry(temp.path(), false);

    let enriched = enrich_file_mentions_for_prompt("read @big.txt", &registry).await;

    assert!(enriched.prompt.contains("[content truncated: 64 KiB limit]"));
    let attached_content = enriched
        .prompt
        .split("Path: big.txt\n\n")
        .nth(1)
        .expect("attached content")
        .split("\n[content truncated: 64 KiB limit]")
        .next()
        .expect("truncated content");
    assert!(
        attached_content.len() <= FILE_MENTION_MAX_BYTES,
        "attached content must be byte-capped, got {} bytes",
        attached_content.len()
    );
    assert!(attached_content.is_char_boundary(attached_content.len()));
    assert!(
        enriched
            .visible_note
            .expect("visible truncation note")
            .contains("@big.txt: content truncated to 64 KiB")
    );
}
