//! Slash command handling for `prx chat`.
//!
//! Each command is a pure function that **returns** its output (instead of
//! `println!`-ing) so the caller can route it into the ratatui mirror via
//! [`super::tui::TuiState::push_system_message`]. Raw mode + alternate screen
//! mean a bare `\n` does not auto-carriage-return; printing directly to
//! stdout from inside the TUI produces ladder-shaped garbled output (the
//! historic `/help` bug). The mirror sink takes care of `\r\n` handling and
//! line-wrapping inside its own widget layer.
//!
//! Tests below still use the legacy `classify_mode_command` helper because
//! exercising `dispatch` requires a memory backend and tool registry.
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
    /// Command was handled with no user-visible output (or output was
    /// already routed elsewhere). Caller should `continue` (skip LLM turn).
    Handled,
    /// Command was handled and produced text the caller MUST display to the
    /// user (typically via `TuiState::push_system_message`). Caller should
    /// then `continue` the loop.
    HandledWithOutput(String),
    /// Input was not a command — proceed with normal LLM turn.
    NotACommand,
    /// /quit or /exit — break the loop.
    Quit,
    /// /plan, /edit, /auto — caller should update session mode + display the
    /// confirmation message via `push_system_message`, then `continue` (skip
    /// LLM turn). The mode change is returned to the caller so [`ChatMode`]
    /// stays out of this module's `CommandContext` (which only carries
    /// immutable borrows).
    SetMode(ChatMode),
    /// /bg, /sessions, /kill (and later session commands) — `dispatch` only
    /// parses these; the chat main loop executes them because it owns the
    /// mutable session runtime state (registry handle + provider/model strings),
    /// which the immutable [`CommandContext`] cannot touch.
    SessionAction(super::sessions::SessionCommand),
    /// /resume — saved chat-session history command. The chat main loop owns
    /// the mutable session identity, provider history, reducer, and child-session
    /// registry, so this module only parses the intent.
    ResumeAction(ResumeCommand),
    /// /branch and /rewind — saved chat-session history mutation commands. The
    /// chat main loop owns persistence, approval, and holder realignment, so this
    /// module only parses the intent.
    HistoryAction(HistoryCommand),
    /// /apply — apply the latest fenced unified diff block from the conversation.
    /// The chat main loop owns the TUI-only approval gate and workspace writes.
    ApplyAction(ApplyCommand),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeCommand {
    List,
    Last,
    Id(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryCommand {
    BranchList,
    Branch(String),
    Rewind(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyCommand {
    Latest,
    Index(usize),
}

/// Context passed to command handlers (borrows from the main loop).
pub struct CommandContext<'a> {
    pub model_name: &'a str,
    pub provider_name: &'a str,
    pub chat_session: &'a session::ChatSession,
    pub tools_registry: &'a [Box<dyn Tool>],
    pub mem: &'a dyn Memory,
}

/// One authoritative slash-command metadata row used by `/help` and the TUI
/// slash menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    /// Canonical command inserted by the slash menu, including the leading `/`.
    pub name: &'static str,
    /// Alternate command spellings accepted by the parser.
    pub aliases: &'static [&'static str],
    /// Argument hint displayed after the command name.
    pub args_hint: &'static str,
    /// Operator-facing description.
    pub description: &'static str,
    /// Machine-readable first argument metadata for slash-menu drill-down.
    pub arg: CommandArgSpec,
}

/// Static argument candidate row used by second-level slash-menu drill-down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandArgCandidate {
    pub value: &'static str,
    pub description: &'static str,
}

/// Candidate source for a slash command's first/next argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandArgSource {
    /// No argument candidates; do not open a second-level menu.
    None,
    /// Argument is free text and intentionally has no candidates.
    FreeText,
    /// Fixed candidates defined directly in the command registry.
    Static(&'static [CommandArgCandidate]),
    /// Theme names accepted by the chat renderer.
    Themes,
    /// Current live child sessions from the session switcher cache.
    LiveSessions,
    /// Persisted chat sessions plus the static `last` selector.
    SavedSessions,
    /// Known provider names from the provider registry.
    Providers,
    /// Models for the current provider, if enumerable.
    CurrentProviderModels,
    /// Second provider argument: models for the provider typed as arg 1.
    ProviderModels,
}

/// Machine-readable first argument metadata for slash-menu drill-down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandArgSpec {
    pub source: CommandArgSource,
}

impl CommandSpec {
    #[must_use]
    pub fn display_name(self) -> String {
        if self.aliases.is_empty() {
            self.name.to_string()
        } else {
            format!("{} {}", self.name, self.aliases.join(" "))
        }
    }

    #[must_use]
    pub fn usage(self) -> String {
        if self.args_hint.is_empty() {
            self.display_name()
        } else {
            format!("{} {}", self.display_name(), self.args_hint)
        }
    }
}

const NO_ARG: CommandArgSpec = CommandArgSpec {
    source: CommandArgSource::None,
};

const FREE_TEXT_ARG: CommandArgSpec = CommandArgSpec {
    source: CommandArgSource::FreeText,
};

const EXPORT_CANDIDATES: &[CommandArgCandidate] = &[
    CommandArgCandidate {
        value: "md",
        description: "Markdown transcript",
    },
    CommandArgCandidate {
        value: "json",
        description: "JSON transcript",
    },
];

const DIFF_CANDIDATES: &[CommandArgCandidate] = &[CommandArgCandidate {
    value: "--cached",
    description: "Show staged changes",
}];

/// Single source of truth for user-visible chat slash commands.
pub const COMMAND_SPECS: &[CommandSpec] = &[
    CommandSpec {
        name: "/help",
        aliases: &[],
        args_hint: "",
        description: "Show this help message",
        arg: NO_ARG,
    },
    CommandSpec {
        name: "/clear",
        aliases: &["/new"],
        args_hint: "",
        description: "Clear conversation history",
        arg: NO_ARG,
    },
    CommandSpec {
        name: "/model",
        aliases: &[],
        args_hint: "[name]",
        description: "Show or switch model",
        arg: CommandArgSpec {
            source: CommandArgSource::CurrentProviderModels,
        },
    },
    CommandSpec {
        name: "/provider",
        aliases: &[],
        args_hint: "[name [model]]",
        description: "Show or hot-switch provider",
        arg: CommandArgSpec {
            source: CommandArgSource::Providers,
        },
    },
    CommandSpec {
        name: "/tools",
        aliases: &[],
        args_hint: "",
        description: "List available tools",
        arg: NO_ARG,
    },
    CommandSpec {
        name: "/memory",
        aliases: &[],
        args_hint: "<query>",
        description: "Search memory",
        arg: FREE_TEXT_ARG,
    },
    CommandSpec {
        name: "/resume",
        aliases: &[],
        args_hint: "[last|id]",
        description: "List or switch saved chat sessions",
        arg: CommandArgSpec {
            source: CommandArgSource::SavedSessions,
        },
    },
    CommandSpec {
        name: "/branch",
        aliases: &[],
        args_hint: "[N]",
        description: "List turn boundaries or fork the first N turns",
        arg: NO_ARG,
    },
    CommandSpec {
        name: "/rewind",
        aliases: &[],
        args_hint: "<N>",
        description: "Trim this session to the first N turns with approval",
        arg: NO_ARG,
    },
    CommandSpec {
        name: "/apply",
        aliases: &[],
        args_hint: "[N]",
        description: "Apply a fenced diff block with approval",
        arg: NO_ARG,
    },
    CommandSpec {
        name: "/cost",
        aliases: &[],
        args_hint: "",
        description: "Show token usage estimate",
        arg: NO_ARG,
    },
    CommandSpec {
        name: "/compact",
        aliases: &[],
        args_hint: "",
        description: "Compact conversation context",
        arg: NO_ARG,
    },
    CommandSpec {
        name: "/export",
        aliases: &[],
        args_hint: "[md|json]",
        description: "Export conversation transcript",
        arg: CommandArgSpec {
            source: CommandArgSource::Static(EXPORT_CANDIDATES),
        },
    },
    CommandSpec {
        name: "/theme",
        aliases: &[],
        args_hint: "",
        description: "Show available chat themes",
        arg: CommandArgSpec {
            source: CommandArgSource::Themes,
        },
    },
    CommandSpec {
        name: "/plan",
        aliases: &[],
        args_hint: "",
        description: "Switch to plan mode",
        arg: NO_ARG,
    },
    CommandSpec {
        name: "/edit",
        aliases: &[],
        args_hint: "",
        description: "Switch to edit mode",
        arg: NO_ARG,
    },
    CommandSpec {
        name: "/auto",
        aliases: &[],
        args_hint: "",
        description: "Switch to auto chat mode",
        arg: NO_ARG,
    },
    CommandSpec {
        name: "/bg",
        aliases: &[],
        args_hint: "<task>",
        description: "Run a task as an agent child session",
        arg: FREE_TEXT_ARG,
    },
    CommandSpec {
        name: "/sessions",
        aliases: &[],
        args_hint: "",
        description: "List child TUI sessions",
        arg: NO_ARG,
    },
    CommandSpec {
        name: "/shell",
        aliases: &[],
        args_hint: "<command>",
        description: "Run a command as a shell child session",
        arg: FREE_TEXT_ARG,
    },
    CommandSpec {
        name: "/pty",
        aliases: &[],
        args_hint: "<command>",
        description: "Open an interactive PTY shell",
        arg: FREE_TEXT_ARG,
    },
    CommandSpec {
        name: "/transcript",
        aliases: &[],
        args_hint: "",
        description: "Open the read-only transcript child TUI",
        arg: NO_ARG,
    },
    CommandSpec {
        name: "/diff",
        aliases: &[],
        args_hint: "[--cached]",
        description: "Open the read-only workspace diff child TUI",
        arg: CommandArgSpec {
            source: CommandArgSource::Static(DIFF_CANDIDATES),
        },
    },
    CommandSpec {
        name: "/attach",
        aliases: &[],
        args_hint: "<id>",
        description: "Show a child session's recent output",
        arg: CommandArgSpec {
            source: CommandArgSource::LiveSessions,
        },
    },
    CommandSpec {
        name: "/logs",
        aliases: &[],
        args_hint: "<id>",
        description: "Dump a child session's buffered output",
        arg: CommandArgSpec {
            source: CommandArgSource::LiveSessions,
        },
    },
    CommandSpec {
        name: "/steer",
        aliases: &[],
        args_hint: "<id> <msg>",
        description: "Send a steering instruction to a child session",
        arg: CommandArgSpec {
            source: CommandArgSource::LiveSessions,
        },
    },
    CommandSpec {
        name: "/kill",
        aliases: &[],
        args_hint: "<id>",
        description: "Stop a child session",
        arg: CommandArgSpec {
            source: CommandArgSource::LiveSessions,
        },
    },
    CommandSpec {
        name: "/detach",
        aliases: &[],
        args_hint: "",
        description: "Return focus to the main chat",
        arg: NO_ARG,
    },
    CommandSpec {
        name: "/approve",
        aliases: &[],
        args_hint: "<id>",
        description: "Approve a child session approval gate",
        arg: CommandArgSpec {
            source: CommandArgSource::LiveSessions,
        },
    },
    CommandSpec {
        name: "/deny",
        aliases: &[],
        args_hint: "<id>",
        description: "Deny a child session approval gate",
        arg: CommandArgSpec {
            source: CommandArgSource::LiveSessions,
        },
    },
    CommandSpec {
        name: "/quit",
        aliases: &["/exit"],
        args_hint: "",
        description: "Exit chat",
        arg: NO_ARG,
    },
];

#[must_use]
pub const fn command_specs() -> &'static [CommandSpec] {
    COMMAND_SPECS
}

#[must_use]
pub fn help_text() -> String {
    let mut out = String::from("Available commands:");
    for spec in command_specs() {
        out.push('\n');
        out.push_str(&format!("  {:<24} {}", spec.usage(), spec.description));
    }
    out
}

/// Dispatch a slash command. Returns `CommandResult`.
///
/// User-facing output is returned via `CommandResult::HandledWithOutput` —
/// never `println!`-ed — so the chat run loop can route it into the ratatui
/// state mirror. Raw mode swallows `\r` on bare `\n`, so any direct stdout
/// write from here would corrupt the visible TUI.
pub async fn dispatch(input: &str, ctx: &CommandContext<'_>) -> CommandResult {
    match input {
        "/help" => CommandResult::HandledWithOutput(help_text()),
        "/quit" | "/exit" => CommandResult::Quit,
        "/plan" => CommandResult::SetMode(ChatMode::Plan),
        "/edit" => CommandResult::SetMode(ChatMode::Edit),
        "/auto" => CommandResult::SetMode(ChatMode::Auto),
        "/tools" => CommandResult::HandledWithOutput(format_tools_feedback(ctx.tools_registry)),
        "/cost" => CommandResult::HandledWithOutput(format_cost_feedback(ctx.chat_session)),
        "/model" => CommandResult::HandledWithOutput(format_model_feedback(ctx.model_name)),
        "/provider" => CommandResult::HandledWithOutput(format!("Current provider: {}", ctx.provider_name)),
        "/resume" => CommandResult::ResumeAction(ResumeCommand::List),
        _ if input.starts_with("/resume ") => {
            let raw = input["/resume ".len()..].trim();
            if raw.is_empty() {
                CommandResult::ResumeAction(ResumeCommand::List)
            } else if raw.eq_ignore_ascii_case("last") {
                CommandResult::ResumeAction(ResumeCommand::Last)
            } else {
                CommandResult::ResumeAction(ResumeCommand::Id(raw.to_string()))
            }
        }
        "/branch" => CommandResult::HistoryAction(HistoryCommand::BranchList),
        _ if input.starts_with("/branch ") => {
            let raw = input["/branch ".len()..].trim();
            if raw.is_empty() {
                CommandResult::HistoryAction(HistoryCommand::BranchList)
            } else {
                CommandResult::HistoryAction(HistoryCommand::Branch(raw.to_string()))
            }
        }
        "/rewind" => CommandResult::HistoryAction(HistoryCommand::Rewind(String::new())),
        _ if input.starts_with("/rewind ") => {
            let raw = input["/rewind ".len()..].trim();
            CommandResult::HistoryAction(HistoryCommand::Rewind(raw.to_string()))
        }
        "/apply" => CommandResult::ApplyAction(ApplyCommand::Latest),
        _ if input.starts_with("/apply ") => {
            let raw = input["/apply ".len()..].trim();
            match raw.parse::<usize>() {
                Ok(index) if index > 0 => CommandResult::ApplyAction(ApplyCommand::Index(index)),
                _ => CommandResult::HandledWithOutput("Usage: /apply [N]".to_string()),
            }
        }
        _ if input.starts_with("/model ") => {
            // BUG-07: `/model <name>` is intercepted in the chat run loop (it
            // needs to mutate the live model slot + main-loop state), so this
            // arm is normally unreachable. Kept as a correct fallback for any
            // caller that routes through `dispatch` directly (e.g. tests).
            let new_model = input["/model ".len()..].trim();
            CommandResult::HandledWithOutput(format!("Switching model to {new_model}…"))
        }
        _ if input.starts_with("/provider ") => {
            // Bug #3: `/provider <name> [model]` is intercepted in the chat run loop
            // (it must rebuild the provider instance + mutate the live provider/model
            // slots), so this arm is normally unreachable. Kept as a correct fallback
            // for any caller that routes through `dispatch` directly (e.g. tests).
            let new_provider = input["/provider ".len()..]
                .split_whitespace()
                .next()
                .unwrap_or_default();
            CommandResult::HandledWithOutput(format!("Switching provider to {new_provider}…"))
        }
        _ if input.starts_with("/memory ") => {
            let query = input["/memory ".len()..].trim();
            if query.is_empty() {
                return CommandResult::HandledWithOutput("Usage: /memory <search query>".to_string());
            }
            let out = match ctx.mem.recall(query, 5, None).await {
                Ok(entries) if entries.is_empty() => format!("No memory entries found for: {query}"),
                Ok(entries) => {
                    let mut s = format!("Memory results for \"{query}\":\n");
                    for entry in &entries {
                        let score = entry.score.map(|sc| format!(" ({sc:.2})")).unwrap_or_default();
                        let preview = if entry.content.chars().count() > 80 {
                            format!("{}...", entry.content.chars().take(80).collect::<String>())
                        } else {
                            entry.content.clone()
                        };
                        s.push_str(&format!("  [{}{score}] {preview}\n", entry.key));
                    }
                    s
                }
                Err(e) => format!("Memory search error: {e}"),
            };
            CommandResult::HandledWithOutput(out)
        }
        _ if input.starts_with("/export") => {
            let format = input.strip_prefix("/export").unwrap_or_default().trim();
            let format = if format.is_empty() { "md" } else { format };
            let out = match export_session(ctx.chat_session, format) {
                Ok(path) => format!("Exported to: {path}"),
                Err(e) => format!("Export failed: {e}"),
            };
            CommandResult::HandledWithOutput(out)
        }
        "/theme" => CommandResult::HandledWithOutput(
            "Available themes: dark (default), light, monokai\nSet via: PRX_CHAT_THEME=monokai prx chat".to_string(),
        ),
        // Session runtime commands (/bg, /sessions, /kill, …). MUST come before
        // the generic unknown-slash fallback so they are not swallowed as
        // "unknown command". Parsing only here; execution happens in the chat
        // main loop (it owns the mutable session runtime state).
        _ if super::sessions::parse_session_command(input).is_some() => {
            // The guard already confirmed `Some`; on the (unreachable) `None`
            // path fall through safely rather than panic.
            super::sessions::parse_session_command(input)
                .map_or(CommandResult::NotACommand, CommandResult::SessionAction)
        }
        _ if input.starts_with('/') => {
            CommandResult::HandledWithOutput(format!("Unknown command: {input}. Type /help for available commands."))
        }
        _ => CommandResult::NotACommand,
    }
}

fn format_cost_feedback(chat_session: &session::ChatSession) -> String {
    let summary = chat_session.token_usage_summary();
    let total_prefix = if summary.has_estimates() { "~" } else { "" };
    let cost = if summary.cost_is_unknown() {
        if summary.known_cost_usd > 0.0 {
            format!(
                "cost unknown (known {})",
                session::format_cost_usd(summary.known_cost_usd)
            )
        } else {
            "cost unknown".to_string()
        }
    } else {
        session::format_cost_usd(summary.known_cost_usd)
    };

    format!(
        "Session cost:\n  Turns:             {}\n  Metered requests:  {}\n  Prompt tokens:     {}\n  Completion tokens: {}\n  Total tokens:      {total_prefix}{}\n  Source split:      real {}, est ~{}\n  Cost:              {cost}",
        chat_session.turn_count(),
        summary.request_count,
        session::format_token_count_compact(summary.prompt_tokens),
        session::format_token_count_compact(summary.completion_tokens),
        session::format_token_count_compact(summary.total_tokens),
        session::format_token_count_compact(summary.reported_tokens),
        session::format_token_count_compact(summary.estimated_tokens),
    )
}

pub fn format_clear_feedback(cleared: u32) -> String {
    if cleared > 0 {
        format!("Conversation cleared (kept system prompt; {cleared} memory entries removed).")
    } else {
        "Conversation cleared (kept system prompt).".to_string()
    }
}

fn format_model_feedback(model_name: &str) -> String {
    format!("Current model: {model_name}\nSwitch live with `/model <name>` (same provider).")
}

fn format_tools_feedback(tools_registry: &[Box<dyn Tool>]) -> String {
    let mut out = format!("Available tools ({}):\n", tools_registry.len());
    for tool in tools_registry {
        out.push_str(&format!("  {:<20} {}\n", tool.name(), tool.description()));
    }
    out
}

/// Handle the /clear command (needs mutable access to history, so handled separately).
///
/// Saved chat sessions (`chat_session:*`) are never deleted by `/clear`.
/// The command clears transient conversation/daily memory while preserving the
/// user's resumeable session list.
pub async fn handle_clear(mem: &dyn Memory, _session_id: Option<&str>) -> u32 {
    let mut cleared = 0u32;
    for category in [MemoryCategory::Conversation, MemoryCategory::Daily] {
        let entries = mem.list(Some(&category), None).await.unwrap_or_default();
        for entry in entries {
            if entry.key.starts_with(super::session::SESSION_MEMORY_PREFIX) {
                continue;
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
    use super::CommandContext;
    use super::{ChatMode, CommandResult, ResumeCommand};
    use crate::memory::NoneMemory;
    use crate::memory::{Memory, MemoryCategory, MemoryEntry};
    use crate::tools::{Tool, ToolResult};
    use async_trait::async_trait;
    use parking_lot::Mutex;
    use std::collections::BTreeSet;

    struct TestTool;

    struct ClearTestMemory {
        entries: Vec<MemoryEntry>,
        forgotten: Mutex<BTreeSet<String>>,
    }

    impl ClearTestMemory {
        fn new(entries: Vec<MemoryEntry>) -> Self {
            Self {
                entries,
                forgotten: Mutex::new(BTreeSet::new()),
            }
        }

        fn was_forgotten(&self, key: &str) -> bool {
            self.forgotten.lock().contains(key)
        }
    }

    fn test_entry(key: &str, category: MemoryCategory) -> MemoryEntry {
        MemoryEntry {
            id: key.to_string(),
            key: key.to_string(),
            content: String::new(),
            category,
            timestamp: "2026-05-19T00:00:00Z".to_string(),
            session_id: None,
            score: None,
            tags: None,
            access_count: None,
            useful_count: None,
            source: None,
            source_confidence: None,
            verification_status: None,
            lifecycle_state: None,
            compressed_from: None,
        }
    }

    #[async_trait]
    impl Memory for ClearTestMemory {
        fn name(&self) -> &str {
            "clear-test"
        }

        async fn store(
            &self,
            _key: &str,
            _content: &str,
            _category: MemoryCategory,
            _session_id: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn recall(
            &self,
            _query: &str,
            _limit: usize,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn get(&self, _key: &str) -> anyhow::Result<Option<MemoryEntry>> {
            Ok(None)
        }

        async fn list(
            &self,
            category: Option<&MemoryCategory>,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(self
                .entries
                .iter()
                .filter(|entry| category.is_none_or(|cat| &entry.category == cat))
                .cloned()
                .collect())
        }

        async fn forget(&self, key: &str) -> anyhow::Result<bool> {
            self.forgotten.lock().insert(key.to_string());
            Ok(true)
        }

        async fn count(&self) -> anyhow::Result<usize> {
            Ok(self.entries.len())
        }

        async fn health_check(&self) -> bool {
            true
        }
    }

    #[async_trait]
    impl Tool for TestTool {
        fn name(&self) -> &str {
            "test_tool"
        }

        fn description(&self) -> &str {
            "Test tool description"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }

        async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                success: true,
                output: String::new(),
                error: None,
            })
        }
    }

    fn command_output(result: CommandResult) -> String {
        match result {
            CommandResult::HandledWithOutput(text) => text,
            _ => panic!("expected slash command to produce user-visible output"),
        }
    }

    #[cfg(feature = "terminal-tui")]
    fn assert_system_message_added(text: String, expected: &str) {
        use crate::chat::action::Action;
        use crate::chat::state::ChatState;
        use crate::chat::tui::ConversationLine;
        use std::sync::Arc;
        use tokio_util::sync::CancellationToken;

        let mut state = ChatState::new(Arc::from("provider"), Arc::from("model"), CancellationToken::new());
        let _ = state.reduce(Action::SystemMessageAdded { text });
        assert!(
            state
                .ui
                .conversation_lines
                .iter()
                .any(|line| matches!(line, ConversationLine::System { content } if content.contains(expected))),
            "SystemMessageAdded should render expected slash feedback"
        );
    }

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

    /// Guard: ensure `CommandResult::HandledWithOutput` survives future
    /// refactors. The P3 chat TUI rearch depends on this variant existing
    /// so command output gets routed through the ratatui mirror instead of
    /// raw `println!` (which corrupts the alt-screen under raw mode — see
    /// `chat-tui-rootcause-2026-05-13.md` root cause B).
    #[test]
    fn handled_with_output_variant_is_constructible() {
        let r = CommandResult::HandledWithOutput("hello".to_string());
        match r {
            CommandResult::HandledWithOutput(s) => assert_eq!(s, "hello"),
            _ => panic!("expected HandledWithOutput variant"),
        }
    }

    #[test]
    fn command_registry_covers_all_known_parser_commands() {
        let mut commands = BTreeSet::new();
        for spec in super::command_specs() {
            assert!(commands.insert(spec.name), "duplicate command spec: {}", spec.name);
            for alias in spec.aliases {
                assert!(commands.insert(*alias), "duplicate command alias: {alias}");
            }
        }
        for cmd in [
            "/help",
            "/quit",
            "/exit",
            "/plan",
            "/edit",
            "/auto",
            "/tools",
            "/cost",
            "/model",
            "/provider",
            "/resume",
            "/branch",
            "/rewind",
            "/apply",
            "/memory",
            "/export",
            "/theme",
            "/clear",
            "/new",
            "/compact",
            "/bg",
            "/sessions",
            "/shell",
            "/pty",
            "/transcript",
            "/diff",
            "/attach",
            "/logs",
            "/steer",
            "/kill",
            "/detach",
            "/approve",
            "/deny",
        ] {
            assert!(commands.contains(cmd), "registry must list parser command {cmd}");
        }
    }

    #[test]
    fn help_text_is_generated_from_command_registry() {
        let help = super::help_text();
        for spec in super::command_specs() {
            assert!(help.contains(spec.name), "/help must list {}: {help}", spec.name);
            assert!(
                help.contains(spec.description),
                "/help must include description for {}: {help}",
                spec.name
            );
            for alias in spec.aliases {
                assert!(help.contains(alias), "/help must list alias {alias}: {help}");
            }
        }
        assert!(help.contains("/theme"), "drift regression: /theme must be in help");
        assert!(help.contains("/approve"), "drift regression: /approve must be in help");
        assert!(help.contains("/deny"), "drift regression: /deny must be in help");
    }

    #[tokio::test]
    async fn slash_clear_preserves_saved_chat_sessions() {
        let memory = ClearTestMemory::new(vec![
            test_entry(
                &format!("{}:current", crate::chat::session::SESSION_MEMORY_PREFIX),
                MemoryCategory::Conversation,
            ),
            test_entry(
                &format!("{}:other", crate::chat::session::SESSION_MEMORY_PREFIX),
                MemoryCategory::Conversation,
            ),
            test_entry("transient-conversation", MemoryCategory::Conversation),
            test_entry("daily-note", MemoryCategory::Daily),
        ]);

        let cleared = super::handle_clear(&memory, Some("current")).await;

        assert_eq!(cleared, 2);
        assert!(!memory.was_forgotten("chat_session:current"));
        assert!(!memory.was_forgotten("chat_session:other"));
        assert!(memory.was_forgotten("transient-conversation"));
        assert!(memory.was_forgotten("daily-note"));
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn slash_clear_emits_system_message() {
        let text = super::format_clear_feedback(0);

        assert!(text.contains("Conversation cleared"));
        assert!(text.contains("kept system prompt"));
        assert_system_message_added(text, "Conversation cleared");
    }

    #[cfg(feature = "terminal-tui")]
    #[tokio::test]
    async fn slash_model_show_current_emits_system_message() {
        let memory = NoneMemory::new();
        let tools: Vec<Box<dyn Tool>> = Vec::new();
        let session = crate::chat::session::ChatSession::new("kimi-code", "kimi-code");
        let ctx = CommandContext {
            model_name: "kimi-code",
            provider_name: "kimi-code",
            chat_session: &session,
            tools_registry: &tools,
            mem: &memory,
        };

        let text = command_output(super::dispatch("/model", &ctx).await);

        assert!(text.contains("Current model: kimi-code"));
        // BUG-07: bare `/model` now advertises the live-switch command.
        assert!(text.contains("/model <name>"));
        assert_system_message_added(text, "Current model: kimi-code");
    }

    /// Bug #3: the `dispatch` fallback for `/provider <name>` must no longer tell
    /// the user to restart (hot-switch is now wired in the chat run loop). It
    /// reports an in-session switch instead.
    #[cfg(feature = "terminal-tui")]
    #[tokio::test]
    async fn slash_provider_switch_does_not_require_restart() {
        let memory = NoneMemory::new();
        let tools: Vec<Box<dyn Tool>> = Vec::new();
        let session = crate::chat::session::ChatSession::new("kimi-code", "kimi-code");
        let ctx = CommandContext {
            model_name: "kimi-code",
            provider_name: "kimi-code",
            chat_session: &session,
            tools_registry: &tools,
            mem: &memory,
        };

        let text = command_output(super::dispatch("/provider openrouter", &ctx).await);

        assert!(
            !text.to_lowercase().contains("restart"),
            "provider switch must not ask the user to restart: {text}"
        );
        assert!(
            text.contains("openrouter"),
            "feedback should name the target provider: {text}"
        );
    }

    #[cfg(feature = "terminal-tui")]
    #[tokio::test]
    async fn slash_tools_lists_registered_emits_system_message() {
        let memory = NoneMemory::new();
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(TestTool)];
        let session = crate::chat::session::ChatSession::new("kimi-code", "kimi-code");
        let ctx = CommandContext {
            model_name: "kimi-code",
            provider_name: "kimi-code",
            chat_session: &session,
            tools_registry: &tools,
            mem: &memory,
        };

        let text = command_output(super::dispatch("/tools", &ctx).await);

        assert!(text.contains("Available tools (1):"));
        assert!(text.contains("test_tool"));
        assert!(text.contains("Test tool description"));
        assert_system_message_added(text, "Available tools (1):");
    }

    /// BUG-08 round-2: `/export md` must contain the actual conversation bodies,
    /// not just the header. Build a session with real turns (mirroring what the
    /// ReduxDriver `Completed` arm now populates) and assert the exported
    /// Markdown carries every user/assistant message body.
    #[test]
    fn export_md_includes_all_turn_bodies() {
        let mut session = crate::chat::session::ChatSession::new("kimi-code", "kimi2.6");
        session.add_user_turn("EXPORT_USER_MSG_ALPHA");
        session.add_assistant_turn("EXPORT_ASSISTANT_MSG_BETA", Vec::new());
        session.add_user_turn("EXPORT_USER_MSG_GAMMA");
        session.add_assistant_turn("EXPORT_ASSISTANT_MSG_DELTA", Vec::new());

        let path = super::export_session(&session, "md").expect("test: export should succeed");
        let body = std::fs::read_to_string(&path).expect("test: exported file should be readable");
        let _ = std::fs::remove_file(&path);

        // Every turn body present (not just the Provider/Model/Date header).
        assert!(body.contains("EXPORT_USER_MSG_ALPHA"), "user turn 1 missing: {body}");
        assert!(body.contains("EXPORT_ASSISTANT_MSG_BETA"), "assistant turn 1 missing");
        assert!(body.contains("EXPORT_USER_MSG_GAMMA"), "user turn 2 missing");
        assert!(body.contains("EXPORT_ASSISTANT_MSG_DELTA"), "assistant turn 2 missing");
        assert!(body.contains("**You**"), "user role label missing");
        assert!(body.contains("**PRX**"), "assistant role label missing");
        // Header present and file is materially larger than the empty-session case.
        assert!(body.contains("kimi2.6"), "model header missing");
        assert!(body.len() > 200, "export looks truncated/empty: {} bytes", body.len());
    }

    /// BUG-08 round-2: JSON export round-trips every turn body.
    #[test]
    fn export_json_includes_all_turn_bodies() {
        let mut session = crate::chat::session::ChatSession::new("kimi-code", "kimi2.6");
        session.add_user_turn("JSON_USER_EPSILON");
        session.add_assistant_turn("JSON_ASSISTANT_ZETA", Vec::new());

        let path = super::export_session(&session, "json").expect("test: json export should succeed");
        let body = std::fs::read_to_string(&path).expect("test: exported json should be readable");
        let _ = std::fs::remove_file(&path);

        assert!(body.contains("JSON_USER_EPSILON"));
        assert!(body.contains("JSON_ASSISTANT_ZETA"));
        let parsed = crate::chat::session::ChatSession::from_json(&body).expect("test: json must re-parse");
        assert_eq!(parsed.turn_count(), 2);
    }

    /// Phase 3: `/cost` reports metered prompt/completion/total tokens and cost,
    /// not transcript chars/4.
    #[cfg(feature = "terminal-tui")]
    #[tokio::test]
    async fn slash_cost_reports_metered_tokens_and_cost() {
        let memory = NoneMemory::new();
        let tools: Vec<Box<dyn Tool>> = Vec::new();
        let mut session = crate::chat::session::ChatSession::new("kimi-code", "kimi2.6");
        session.add_user_turn("a fairly long user question that should produce a measurable char count");
        session.add_assistant_turn(
            "an equally substantial assistant reply with plenty of characters",
            Vec::new(),
        );
        session
            .token_usage_records
            .push(crate::chat::session::MainSessionTokenUsageRecord {
                provider: "openai".to_string(),
                model: "gpt-4o-mini".to_string(),
                prompt_tokens: 1_000,
                completion_tokens: 500,
                total_tokens: 1_500,
                source: crate::llm::route_decision::TokenUsageSource::Reported,
                cost_usd: Some(0.0105),
            });
        let ctx = CommandContext {
            model_name: "kimi2.6",
            provider_name: "kimi-code",
            chat_session: &session,
            tools_registry: &tools,
            mem: &memory,
        };

        let text = command_output(super::dispatch("/cost", &ctx).await);

        assert!(
            text.contains("Turns:             2"),
            "cost should report 2 turns: {text}"
        );
        assert!(
            text.contains("Prompt tokens:     1.0k"),
            "prompt tokens missing: {text}"
        );
        assert!(
            text.contains("Completion tokens: 500"),
            "completion tokens missing: {text}"
        );
        assert!(text.contains("Total tokens:      1.5k"), "total tokens missing: {text}");
        assert!(
            text.contains("Source split:      real 1.5k, est ~0"),
            "source split missing: {text}"
        );
        assert!(
            text.contains("Cost:              $0.0105"),
            "display cost missing: {text}"
        );
        assert!(
            !text.contains("Total chars"),
            "chars/4 estimate must not be reported: {text}"
        );
    }

    /// Phase 3: estimated records are visibly marked with `~`.
    #[cfg(feature = "terminal-tui")]
    #[tokio::test]
    async fn slash_cost_marks_estimated_usage() {
        let memory = NoneMemory::new();
        let tools: Vec<Box<dyn Tool>> = Vec::new();
        let mut session = crate::chat::session::ChatSession::new("kimi-code", "kimi2.6");
        session
            .token_usage_records
            .push(crate::chat::session::MainSessionTokenUsageRecord {
                provider: "other".to_string(),
                model: "estimated-model".to_string(),
                prompt_tokens: 0,
                completion_tokens: 250,
                total_tokens: 250,
                source: crate::llm::route_decision::TokenUsageSource::Estimated,
                cost_usd: Some(0.0001),
            });
        let ctx = CommandContext {
            model_name: "kimi2.6",
            provider_name: "kimi-code",
            chat_session: &session,
            tools_registry: &tools,
            mem: &memory,
        };

        let text = command_output(super::dispatch("/cost", &ctx).await);

        assert!(
            text.contains("Total tokens:      ~250"),
            "estimated total should be marked: {text}"
        );
        assert!(
            text.contains("Source split:      real 0, est ~250"),
            "estimated split missing: {text}"
        );
    }

    /// Phase 3: no-price models surface unknown cost instead of `$0`.
    #[cfg(feature = "terminal-tui")]
    #[tokio::test]
    async fn slash_cost_reports_unknown_for_unpriced_usage() {
        let memory = NoneMemory::new();
        let tools: Vec<Box<dyn Tool>> = Vec::new();
        let mut session = crate::chat::session::ChatSession::new("ollama", "llama3");
        session
            .token_usage_records
            .push(crate::chat::session::MainSessionTokenUsageRecord {
                provider: "ollama".to_string(),
                model: "llama3".to_string(),
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                source: crate::llm::route_decision::TokenUsageSource::Reported,
                cost_usd: None,
            });
        let ctx = CommandContext {
            model_name: "llama3",
            provider_name: "ollama",
            chat_session: &session,
            tools_registry: &tools,
            mem: &memory,
        };

        let text = command_output(super::dispatch("/cost", &ctx).await);

        assert!(
            text.contains("Cost:              cost unknown"),
            "unknown cost missing: {text}"
        );
        assert!(
            !text.contains("Cost:              $0"),
            "unknown cost must not be rendered as zero: {text}"
        );
    }

    /// BUG-06 round-2: empty session still reports zero.
    #[cfg(feature = "terminal-tui")]
    #[tokio::test]
    async fn slash_cost_reports_zero_for_empty_session() {
        let memory = NoneMemory::new();
        let tools: Vec<Box<dyn Tool>> = Vec::new();
        let session = crate::chat::session::ChatSession::new("kimi-code", "kimi2.6");
        let ctx = CommandContext {
            model_name: "kimi2.6",
            provider_name: "kimi-code",
            chat_session: &session,
            tools_registry: &tools,
            mem: &memory,
        };

        let text = command_output(super::dispatch("/cost", &ctx).await);

        assert!(
            text.contains("Turns:             0"),
            "empty session cost must be 0 turns: {text}"
        );
        assert!(
            text.contains("Total tokens:      0"),
            "empty session should report 0 tokens: {text}"
        );
        assert!(
            text.contains("Cost:              $0.0000"),
            "empty session should report zero known cost: {text}"
        );
    }

    #[tokio::test]
    async fn slash_resume_parses_list_last_and_id() {
        let memory = NoneMemory::new();
        let tools: Vec<Box<dyn Tool>> = Vec::new();
        let session = crate::chat::session::ChatSession::new("kimi-code", "kimi2.6");
        let ctx = CommandContext {
            model_name: "kimi2.6",
            provider_name: "kimi-code",
            chat_session: &session,
            tools_registry: &tools,
            mem: &memory,
        };

        assert!(matches!(
            super::dispatch("/resume", &ctx).await,
            CommandResult::ResumeAction(ResumeCommand::List)
        ));
        assert!(matches!(
            super::dispatch("/resume last", &ctx).await,
            CommandResult::ResumeAction(ResumeCommand::Last)
        ));
        assert!(matches!(
            super::dispatch("/resume abc-123", &ctx).await,
            CommandResult::ResumeAction(ResumeCommand::Id(id)) if id == "abc-123"
        ));
    }

    #[tokio::test]
    async fn slash_branch_and_rewind_parse_history_actions() {
        let memory = NoneMemory::new();
        let tools: Vec<Box<dyn Tool>> = Vec::new();
        let session = crate::chat::session::ChatSession::new("kimi-code", "kimi2.6");
        let ctx = CommandContext {
            model_name: "kimi2.6",
            provider_name: "kimi-code",
            chat_session: &session,
            tools_registry: &tools,
            mem: &memory,
        };

        assert!(matches!(
            super::dispatch("/branch", &ctx).await,
            CommandResult::HistoryAction(super::HistoryCommand::BranchList)
        ));
        assert!(matches!(
            super::dispatch("/branch 2", &ctx).await,
            CommandResult::HistoryAction(super::HistoryCommand::Branch(n)) if n == "2"
        ));
        assert!(matches!(
            super::dispatch("/rewind 1", &ctx).await,
            CommandResult::HistoryAction(super::HistoryCommand::Rewind(n)) if n == "1"
        ));
        assert!(matches!(
            super::dispatch("/rewind", &ctx).await,
            CommandResult::HistoryAction(super::HistoryCommand::Rewind(n)) if n.is_empty()
        ));
    }
}
