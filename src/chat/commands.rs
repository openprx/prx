//! Slash command handling for `prx chat`.
//!
//! Each command is a pure function that prints output and returns a
//! [`CommandResult`] so the caller knows whether to `continue` the loop.
// Chat commands: println! calls are intentional user-facing slash-command output.
#![allow(clippy::print_stdout)]

use super::session;
use crate::memory::{Memory, MemoryCategory};
use crate::tools::Tool;
use anyhow::Result;

// Re-export `ChatMode` from the lib crate so the chat slash-command parser
// and the tool-execution loop share the same type without crossing the
// lib/bin module boundary.
pub use crate::agent::loop_::ChatMode;

/// Outcome of a slash-command dispatch.
pub enum CommandResult {
    /// Command was handled — caller should `continue` (skip LLM turn).
    Handled,
    /// Input was not a command — proceed with normal LLM turn.
    NotACommand,
    /// /quit or /exit — break the loop.
    Quit,
    /// /plan, /edit, /auto — caller should update session mode + print
    /// confirmation, then `continue` (skip LLM turn). The mode change is
    /// returned to the caller so [`ChatMode`] stays out of this module's
    /// `CommandContext` (which only carries immutable borrows).
    SetMode(ChatMode),
}

/// Context passed to command handlers (borrows from the main loop).
pub struct CommandContext<'a> {
    pub model_name: &'a str,
    pub provider_name: &'a str,
    pub chat_session: &'a session::ChatSession,
    pub tools_registry: &'a [Box<dyn Tool>],
    pub mem: &'a dyn Memory,
}

/// Dispatch a slash command. Returns `CommandResult`.
pub async fn dispatch(input: &str, ctx: &CommandContext<'_>) -> CommandResult {
    match input {
        "/help" => {
            println!("Available commands:");
            println!("  /help              Show this help message");
            println!("  /clear /new        Clear conversation history");
            println!("  /model [name]      Show or switch model");
            println!("  /provider [name]   Show or switch provider");
            println!("  /tools             List available tools");
            println!("  /memory <query>    Search memory");
            println!("  /cost              Show token usage estimate");
            println!("  /export [md|json]  Export conversation");
            println!("  /plan              Switch to plan mode (read-only tools)");
            println!("  /edit              Switch to edit mode (default)");
            println!("  /auto              Switch to auto mode (no approval prompts)");
            println!("  /quit /exit        Exit chat\n");
            CommandResult::Handled
        }
        "/quit" | "/exit" => CommandResult::Quit,
        "/plan" => CommandResult::SetMode(ChatMode::Plan),
        "/edit" => CommandResult::SetMode(ChatMode::Edit),
        "/auto" => CommandResult::SetMode(ChatMode::Auto),
        "/tools" => {
            println!("Available tools:\n");
            for tool in ctx.tools_registry {
                println!("  {:<20} {}", tool.name(), tool.description());
            }
            println!();
            CommandResult::Handled
        }
        "/cost" => {
            let total_chars: usize = ctx.chat_session.turns.iter().map(|t| t.content.chars().count()).sum();
            let est_tokens = total_chars / 4;
            println!("Session cost estimate:");
            println!("  Turns:        {}", ctx.chat_session.turn_count());
            println!("  Total chars:  {total_chars}");
            println!("  Est. tokens:  ~{est_tokens}\n");
            CommandResult::Handled
        }
        "/model" => {
            println!("Current model: {}\n", ctx.model_name);
            CommandResult::Handled
        }
        "/provider" => {
            println!("Current provider: {}\n", ctx.provider_name);
            CommandResult::Handled
        }
        _ if input.starts_with("/model ") => {
            let new_model = input["/model ".len()..].trim();
            println!("Model switching requires restarting: prx chat -m {new_model}\n");
            CommandResult::Handled
        }
        _ if input.starts_with("/provider ") => {
            let new_provider = input["/provider ".len()..].trim();
            println!("Provider switching requires restarting: prx chat -p {new_provider}\n");
            CommandResult::Handled
        }
        _ if input.starts_with("/memory ") => {
            let query = input["/memory ".len()..].trim();
            if query.is_empty() {
                println!("Usage: /memory <search query>\n");
                return CommandResult::Handled;
            }
            match ctx.mem.recall(query, 5, None).await {
                Ok(entries) if entries.is_empty() => {
                    println!("No memory entries found for: {query}\n");
                }
                Ok(entries) => {
                    println!("Memory results for \"{query}\":\n");
                    for entry in &entries {
                        let score = entry.score.map(|s| format!(" ({s:.2})")).unwrap_or_default();
                        let preview = if entry.content.chars().count() > 80 {
                            format!("{}...", entry.content.chars().take(80).collect::<String>())
                        } else {
                            entry.content.clone()
                        };
                        println!("  [{}{score}] {preview}", entry.key);
                    }
                    println!();
                }
                Err(e) => {
                    println!("Memory search error: {e}\n");
                }
            }
            CommandResult::Handled
        }
        _ if input.starts_with("/export") => {
            let format = input.strip_prefix("/export").unwrap_or_default().trim();
            let format = if format.is_empty() { "md" } else { format };
            match export_session(ctx.chat_session, format) {
                Ok(path) => println!("Exported to: {path}\n"),
                Err(e) => println!("Export failed: {e}\n"),
            }
            CommandResult::Handled
        }
        "/theme" => {
            println!("Available themes: dark (default), light, monokai");
            println!("Set via: PRX_CHAT_THEME=monokai prx chat\n");
            CommandResult::Handled
        }
        _ if input.starts_with('/') => {
            println!("Unknown command: {input}. Type /help for available commands.\n");
            CommandResult::Handled
        }
        _ => CommandResult::NotACommand,
    }
}

/// Handle the /clear command (needs mutable access to history, so handled separately).
///
/// When `session_id` is provided, only deletes conversation-scoped memory entries
/// that do **not** belong to other saved sessions (`chat_session:*` keys for other IDs
/// are preserved). This prevents `/clear` from wiping unrelated sessions.
pub async fn handle_clear(mem: &dyn Memory, session_id: Option<&str>) -> u32 {
    let mut cleared = 0u32;
    for category in [MemoryCategory::Conversation, MemoryCategory::Daily] {
        let entries = mem.list(Some(&category), None).await.unwrap_or_default();
        for entry in entries {
            // When session-scoped, preserve other sessions' data.
            if let Some(sid) = session_id {
                if entry.key.starts_with(super::session::SESSION_MEMORY_PREFIX) && !entry.key.ends_with(sid) {
                    continue; // belongs to a different session — skip
                }
            }
            if mem.forget(&entry.key).await.unwrap_or(false) {
                cleared += 1;
            }
        }
    }
    cleared
}

/// Export a chat session to a file (Markdown or JSON).
fn export_session(session: &session::ChatSession, format: &str) -> Result<String> {
    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let ext = match format {
        "json" => "json",
        _ => "md",
    };
    let filename = format!("prx_chat_{timestamp}.{ext}");

    let content = match format {
        "json" => session.to_json().map_err(|e| anyhow::anyhow!("JSON serialize: {e}"))?,
        _ => {
            let mut md = String::new();
            md.push_str(&format!(
                "# {}\n\n",
                if session.title.is_empty() {
                    "PRX Chat Export"
                } else {
                    &session.title
                }
            ));
            md.push_str(&format!(
                "**Provider**: {} | **Model**: {} | **Date**: {}\n\n---\n\n",
                session.provider,
                session.model,
                session.created_at.format("%Y-%m-%d %H:%M")
            ));
            for turn in &session.turns {
                match turn.role.as_str() {
                    "user" => {
                        md.push_str(&format!("**You**: {}\n\n", turn.content));
                    }
                    "assistant" => {
                        md.push_str(&format!("**PRX**: {}\n\n", turn.content));
                    }
                    _ => {
                        md.push_str(&format!("*{}*: {}\n\n", turn.role, turn.content));
                    }
                }
            }
            md
        }
    };

    std::fs::write(&filename, &content).map_err(|e| anyhow::anyhow!("write {filename}: {e}"))?;
    Ok(filename)
}

#[cfg(test)]
mod mode_tests {
    //! Parser-level coverage for `/plan` `/edit` `/auto`. The full dispatch
    //! path needs a `CommandContext` (tools registry + memory backend) so
    //! these tests exercise the pure mode-classification helpers, plus a
    //! pattern-match shim that mirrors what `dispatch` returns for the three
    //! mode-switching commands.
    use super::{ChatMode, CommandResult};

    /// Pure classification helper mirroring the `dispatch` match arms for the
    /// mode-switching commands. Returns `Some(mode)` for `/plan|/edit|/auto`,
    /// `None` otherwise. Keeps the test independent of async `dispatch`
    /// machinery (memory backend, tools registry).
    fn classify_mode_command(input: &str) -> Option<ChatMode> {
        match input {
            "/plan" => Some(ChatMode::Plan),
            "/edit" => Some(ChatMode::Edit),
            "/auto" => Some(ChatMode::Auto),
            _ => None,
        }
    }

    #[test]
    fn plan_command_parses_to_plan_mode() {
        assert_eq!(classify_mode_command("/plan"), Some(ChatMode::Plan));
    }

    #[test]
    fn edit_command_parses_to_edit_mode() {
        assert_eq!(classify_mode_command("/edit"), Some(ChatMode::Edit));
    }

    #[test]
    fn auto_command_parses_to_auto_mode() {
        assert_eq!(classify_mode_command("/auto"), Some(ChatMode::Auto));
    }

    #[test]
    fn unknown_slash_command_does_not_match_mode() {
        assert_eq!(classify_mode_command("/banana"), None);
        assert_eq!(classify_mode_command("/help"), None);
        assert_eq!(classify_mode_command("/planz"), None);
        assert_eq!(classify_mode_command(""), None);
    }

    #[test]
    fn default_mode_is_edit() {
        assert_eq!(ChatMode::default(), ChatMode::Edit);
    }

    #[test]
    fn mode_labels_are_stable() {
        assert_eq!(ChatMode::Plan.label(), "plan");
        assert_eq!(ChatMode::Edit.label(), "edit");
        assert_eq!(ChatMode::Auto.label(), "auto");
    }

    #[test]
    fn only_plan_intercepts_writes() {
        assert!(ChatMode::Plan.intercepts_writes());
        assert!(!ChatMode::Edit.intercepts_writes());
        assert!(!ChatMode::Auto.intercepts_writes());
    }

    /// Guard: ensure `CommandResult::SetMode` is publicly constructible — this
    /// prevents accidental loss of the variant during future refactors.
    #[test]
    fn set_mode_variant_is_constructible() {
        let r = CommandResult::SetMode(ChatMode::Plan);
        match r {
            CommandResult::SetMode(m) => assert_eq!(m, ChatMode::Plan),
            _ => panic!("expected SetMode variant"),
        }
    }
}
