//! External `$EDITOR` integration for the fullscreen chat input.

#[cfg(feature = "terminal-tui")]
use super::terminal::{CHAT_KEYBOARD_ENHANCEMENT_ACTIVE, CHAT_MOUSE_CAPTURE_ACTIVE};
use std::io::Write as _;

#[cfg(feature = "terminal-tui")]
pub(super) trait ExternalEditorTerminalMode {
    fn suspend_for_editor(&self);
    fn restore_after_editor(&self);
}

#[cfg(feature = "terminal-tui")]
pub(super) struct CrosstermExternalEditorTerminalMode;

#[cfg(feature = "terminal-tui")]
pub(super) fn write_external_editor_suspend_sequences(out: &mut dyn std::io::Write) {
    crate::chat::sessions::pty::write_chat_alt_screen_leave_for_handoff(
        out,
        CHAT_KEYBOARD_ENHANCEMENT_ACTIVE.load(std::sync::atomic::Ordering::Acquire),
    );
}

#[cfg(feature = "terminal-tui")]
pub(super) fn write_external_editor_restore_sequences(out: &mut dyn std::io::Write) {
    crate::chat::sessions::pty::write_handoff_terminal_restore(
        out,
        CHAT_KEYBOARD_ENHANCEMENT_ACTIVE.load(std::sync::atomic::Ordering::Acquire),
        CHAT_MOUSE_CAPTURE_ACTIVE.load(std::sync::atomic::Ordering::Acquire),
    );
}

#[cfg(feature = "terminal-tui")]
impl ExternalEditorTerminalMode for CrosstermExternalEditorTerminalMode {
    fn suspend_for_editor(&self) {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
        let _ = crossterm::execute!(std::io::stdout(), crossterm::cursor::Show);
        let mut out = std::io::stdout();
        write_external_editor_suspend_sequences(&mut out);
        let _ = crossterm::terminal::disable_raw_mode();
    }

    fn restore_after_editor(&self) {
        let mut out = std::io::stdout();
        write_external_editor_restore_sequences(&mut out);
        let _ = crossterm::terminal::enable_raw_mode();
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste);
        let _ = crossterm::execute!(std::io::stdout(), crossterm::cursor::Show);
    }
}

#[cfg(feature = "terminal-tui")]
pub(super) struct ExternalEditorTerminalGuard<'a> {
    pub(super) terminal: &'a dyn ExternalEditorTerminalMode,
    pub(super) active: bool,
}

#[cfg(feature = "terminal-tui")]
impl<'a> ExternalEditorTerminalGuard<'a> {
    pub(super) fn new(terminal: &'a dyn ExternalEditorTerminalMode) -> Self {
        terminal.suspend_for_editor();
        Self { terminal, active: true }
    }
}

#[cfg(feature = "terminal-tui")]
impl Drop for ExternalEditorTerminalGuard<'_> {
    fn drop(&mut self) {
        if self.active {
            self.terminal.restore_after_editor();
            self.active = false;
        }
    }
}

#[cfg(feature = "terminal-tui")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ExternalEditorResult {
    Edited(String),
    Unchanged(String),
}

#[cfg(feature = "terminal-tui")]
pub(super) fn resolve_external_editor() -> Option<String> {
    std::env::var("VISUAL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| std::env::var("EDITOR").ok().filter(|value| !value.trim().is_empty()))
}

#[cfg(feature = "terminal-tui")]
fn run_external_editor_command(editor: &str, path: &std::path::Path) -> std::io::Result<std::process::ExitStatus> {
    #[cfg(windows)]
    {
        std::process::Command::new("cmd")
            .arg("/C")
            .arg(format!("{editor} \"{}\"", path.display()))
            .status()
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("sh")
            .arg("-c")
            .arg("exec $PRX_EXTERNAL_EDITOR \"$1\"")
            .arg("prx-editor")
            .arg(path)
            .env("PRX_EXTERNAL_EDITOR", editor)
            .status()
    }
}

#[cfg(feature = "terminal-tui")]
pub(super) fn edit_text_with_external_editor(
    initial: &str,
    editor: Option<String>,
    terminal: &dyn ExternalEditorTerminalMode,
) -> ExternalEditorResult {
    edit_text_with_external_editor_with_runner(initial, editor, terminal, run_external_editor_command)
}

#[cfg(feature = "terminal-tui")]
pub(super) fn edit_text_with_external_editor_with_runner(
    initial: &str,
    editor: Option<String>,
    terminal: &dyn ExternalEditorTerminalMode,
    run_editor: impl FnOnce(&str, &std::path::Path) -> std::io::Result<std::process::ExitStatus>,
) -> ExternalEditorResult {
    let Some(editor) = editor else {
        return ExternalEditorResult::Unchanged("External editor unavailable: set VISUAL or EDITOR.".to_string());
    };
    let mut file = match tempfile::NamedTempFile::new() {
        Ok(file) => file,
        Err(err) => {
            return ExternalEditorResult::Unchanged(format!("External editor unavailable: temp file failed ({err})."));
        }
    };
    if let Err(err) = file.write_all(initial.as_bytes()) {
        return ExternalEditorResult::Unchanged(format!(
            "External editor unavailable: temp file write failed ({err})."
        ));
    }
    if let Err(err) = file.flush() {
        return ExternalEditorResult::Unchanged(format!(
            "External editor unavailable: temp file flush failed ({err})."
        ));
    }
    let path = file.path().to_path_buf();
    {
        let _terminal_guard = ExternalEditorTerminalGuard::new(terminal);
        let status = match run_editor(&editor, &path) {
            Ok(status) => status,
            Err(err) => {
                return ExternalEditorResult::Unchanged(format!("External editor failed to start: {err}."));
            }
        };
        if !status.success() {
            return ExternalEditorResult::Unchanged(format!("External editor exited with status {status}."));
        }
    }
    match std::fs::read_to_string(&path) {
        Ok(text) => ExternalEditorResult::Edited(text),
        Err(err) => ExternalEditorResult::Unchanged(format!("External editor read failed: {err}.")),
    }
}
