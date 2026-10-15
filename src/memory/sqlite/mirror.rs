//! Markdown backup mirror and database backup helpers.

use crate::memory::traits::MemoryCategory;
use chrono::Local;
use rusqlite::params;
use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

use super::SqliteMemory;

impl SqliteMemory {
    /// Create a consistent SQLite backup without stopping readers or copying a
    /// live WAL file set byte-for-byte.
    pub async fn backup_database_to(&self, target: &Path) -> anyhow::Result<()> {
        let pool = self.pool.clone();
        let target = target.to_path_buf();
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<()> {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            anyhow::ensure!(!target.exists(), "backup target already exists: {}", target.display());
            let conn = pool.write();
            conn.execute("VACUUM INTO ?1", params![target.to_string_lossy().to_string()])?;
            Ok(())
        })
        .await?
    }

    fn backup_file_path(db_path: &Path, category: &MemoryCategory) -> PathBuf {
        let memory_dir = db_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        let workspace_dir = if memory_dir.file_name().and_then(|n| n.to_str()) == Some("memory") {
            memory_dir
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| memory_dir.clone())
        } else {
            memory_dir.clone()
        };

        match category {
            MemoryCategory::Core => workspace_dir.join("MEMORY.md"),
            _ => {
                let date = Local::now().format("%Y-%m-%d").to_string();
                memory_dir.join(format!("{date}.md"))
            }
        }
    }

    pub(super) fn append_backup_entry(db_path: &Path, key: &str, content: &str, category: &MemoryCategory) {
        let path = Self::backup_file_path(db_path, category);
        if let Some(parent) = path.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                tracing::warn!("memory backup mkdir failed ({}): {error}", parent.display());
                return;
            }
        }

        // NOTE: TOCTOU benign — the exists() check only decides whether to
        // prepend a header. The file is opened with O_CREAT | O_APPEND below,
        // so a race at worst produces a missing or duplicate header (cosmetic).
        let header = if path.exists() {
            None
        } else if matches!(category, MemoryCategory::Core) {
            Some("# Long-Term Memory\n\n".to_string())
        } else {
            let date = Local::now().format("%Y-%m-%d").to_string();
            Some(format!("# Daily Log — {date}\n\n"))
        };

        let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S");
        // R-MCPB F-1: the key and the content are escaped so a writer can
        // neither start a line of its own nor embed another key's entry line.
        let key = Self::mirror_line_text(key);
        let body = format!(
            "- [{timestamp}] **{key}**: {}\n",
            Self::mirror_line_text(content.trim())
        );

        if matches!(category, MemoryCategory::Core) {
            if let Err(error) = Self::upsert_core_backup_entry(&path, &key, &body) {
                tracing::warn!("core memory backup update failed ({}): {error}", path.display());
            }
            return;
        }

        let payload = match header {
            Some(header) => format!("{header}{body}"),
            None => body,
        };

        let open_result = OpenOptions::new().create(true).append(true).open(&path);
        match open_result {
            Ok(mut file) => {
                if let Err(error) = file.write_all(payload.as_bytes()) {
                    tracing::warn!("memory backup append failed ({}): {error}", path.display());
                }
            }
            Err(error) => {
                tracing::warn!("memory backup open failed ({}): {error}", path.display());
            }
        }
    }

    /// Keep `MEMORY.md` as a projection of the latest value for each generated
    /// key. Daily logs remain append-only, but core memory keys are upserts in
    /// SQLite and must not grow a second, stale copy on every scheduled write.
    fn upsert_core_backup_entry(path: &Path, key: &str, body: &str) -> anyhow::Result<()> {
        let lock_path = path.with_extension("md.lock");
        let lock_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        fs2::FileExt::lock_exclusive(&lock_file)?;

        let existing = std::fs::read_to_string(path).unwrap_or_else(|_| "# Long-Term Memory\n\n".to_string());
        let mut blocks: Vec<(Option<String>, String)> = Vec::new();
        for line in existing.split_inclusive('\n') {
            let entry_key = Self::generated_backup_entry_key(line).map(str::to_string);
            if entry_key.is_some() || blocks.is_empty() {
                blocks.push((entry_key, line.to_string()));
            } else if let Some((_, block)) = blocks.last_mut() {
                block.push_str(line);
            }
        }

        let mut last_by_key = std::collections::HashMap::new();
        for (index, (block_key, _)) in blocks.iter().enumerate() {
            if let Some(block_key) = block_key {
                last_by_key.insert(block_key.as_str(), index);
            }
        }

        let mut rewritten = String::with_capacity(existing.len().saturating_add(body.len()));
        for (index, (block_key, block)) in blocks.iter().enumerate() {
            match block_key {
                None => rewritten.push_str(block),
                Some(block_key) if block_key != key && last_by_key.get(block_key.as_str()) == Some(&index) => {
                    rewritten.push_str(block);
                }
                Some(_) => {}
            }
        }
        if !rewritten.ends_with('\n') {
            rewritten.push('\n');
        }
        rewritten.push_str(body);

        let temp_path = path.with_extension(format!("md.tmp.{}", Uuid::new_v4()));
        std::fs::write(&temp_path, rewritten)?;
        std::fs::rename(&temp_path, path)?;
        fs2::FileExt::unlock(&lock_file)?;
        Ok(())
    }

    /// Text as it may appear inside one generated Markdown mirror line.
    ///
    /// The mirror (`MEMORY.md`, daily logs) is keyed by lines of the form
    /// `- [..] **<key>**: <content>` and `MEMORY.md` is inlined into system
    /// prompts, so mirrored text must stay on its own line and must not contain
    /// the `**` that delimits a key. Backslashes, line breaks (`\n`, `\r`,
    /// `\r\n`), `*`, and U+2028, U+2029, NEL, VT and FF (as `\u{..}`) are
    /// escaped; the escaped form is unambiguous and still readable.
    pub(super) fn mirror_line_text(text: &str) -> String {
        let mut escaped = String::with_capacity(text.len());
        let mut chars = text.chars().peekable();
        while let Some(character) = chars.next() {
            match character {
                '\\' => escaped.push_str("\\\\"),
                '*' => escaped.push_str("\\*"),
                '\n' => escaped.push_str("\\n"),
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    escaped.push_str("\\n");
                }
                // Separators a model may read as a line break (R-MCPBfg N-3).
                // Writing to a `String` cannot fail.
                '\u{2028}' | '\u{2029}' | '\u{0085}' | '\u{000B}' | '\u{000C}' => {
                    let _ = write!(escaped, "\\u{{{:04x}}}", u32::from(character));
                }
                other => escaped.push(other),
            }
        }
        escaped
    }

    pub(super) fn generated_backup_entry_key(line: &str) -> Option<&str> {
        let rest = line.strip_prefix("- [")?;
        let (_, rest) = rest.split_once("] **")?;
        let (key, _) = rest.split_once("**:")?;
        (!key.is_empty()).then_some(key)
    }
}
