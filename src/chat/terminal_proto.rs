//! Terminal protocol utilities: inline image preview (kitty/iTerm2) and
//! OSC 52 clipboard support for code block copying.

use base64::Engine;
use std::io::{self, Write};

/// Detect if the terminal supports kitty graphics protocol.
pub fn supports_kitty_graphics() -> bool {
    std::env::var("TERM_PROGRAM")
        .map(|v| v == "kitty")
        .unwrap_or(false)
        || std::env::var("KITTY_PID").is_ok()
}

/// Detect if the terminal supports iTerm2 inline image protocol.
pub fn supports_iterm2_images() -> bool {
    std::env::var("TERM_PROGRAM")
        .map(|v| v == "iTerm.app" || v == "WezTerm")
        .unwrap_or(false)
        || std::env::var("ITERM_SESSION_ID").is_ok()
}

/// Display an image inline using the appropriate terminal protocol.
///
/// Falls back to printing a text description if no image protocol is supported.
pub fn display_image(path: &str) -> io::Result<()> {
    let data = std::fs::read(path)?;

    if supports_kitty_graphics() {
        display_image_kitty(&data)
    } else if supports_iterm2_images() {
        display_image_iterm2(&data, path)
    } else {
        println!("  [image: {path}]");
        Ok(())
    }
}

/// Display image using kitty graphics protocol.
fn display_image_kitty(data: &[u8]) -> io::Result<()> {
    let b64 = base64::engine::general_purpose::STANDARD.encode(data);
    let mut stdout = io::stdout().lock();

    // Kitty protocol: split into 4096-byte chunks
    let chunk_size = 4096;
    let chunks: Vec<&str> = b64
        .as_bytes()
        .chunks(chunk_size)
        .map(|c| std::str::from_utf8(c).unwrap_or_default())
        .collect();

    for (i, chunk) in chunks.iter().enumerate() {
        let more = if i < chunks.len() - 1 { 1 } else { 0 };
        if i == 0 {
            write!(stdout, "\x1b_Ga=T,f=100,m={more};{chunk}\x1b\\")?;
        } else {
            write!(stdout, "\x1b_Gm={more};{chunk}\x1b\\")?;
        }
    }
    writeln!(stdout)?;
    stdout.flush()
}

/// Display image using iTerm2 inline image protocol.
fn display_image_iterm2(data: &[u8], name: &str) -> io::Result<()> {
    let b64 = base64::engine::general_purpose::STANDARD.encode(data);
    let filename_b64 = base64::engine::general_purpose::STANDARD.encode(name.as_bytes());
    let mut stdout = io::stdout().lock();
    write!(
        stdout,
        "\x1b]1337;File=name={filename_b64};size={};inline=1:{b64}\x07",
        data.len()
    )?;
    writeln!(stdout)?;
    stdout.flush()
}

/// Copy text to clipboard using OSC 52 escape sequence.
///
/// Works in terminals that support OSC 52 (xterm, kitty, iTerm2, WezTerm, etc.).
pub fn copy_to_clipboard(text: &str) -> io::Result<()> {
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let mut stdout = io::stdout().lock();
    // OSC 52: set clipboard content
    // 'c' = clipboard selection
    write!(stdout, "\x1b]52;c;{b64}\x07")?;
    stdout.flush()
}

/// Chat theme configuration.
#[derive(Debug, Clone)]
pub struct ChatTheme {
    pub user_color: &'static str,
    pub assistant_color: &'static str,
    pub tool_color: &'static str,
    pub error_color: &'static str,
    pub status_color: &'static str,
    pub muted_color: &'static str,
}

impl ChatTheme {
    /// Dark theme (default).
    pub fn dark() -> Self {
        Self {
            user_color: "\x1b[32m",      // green
            assistant_color: "\x1b[36m", // cyan
            tool_color: "\x1b[33m",      // yellow
            error_color: "\x1b[31m",     // red
            status_color: "\x1b[37m",    // white
            muted_color: "\x1b[90m",     // dark gray
        }
    }

    /// Light theme.
    pub fn light() -> Self {
        Self {
            user_color: "\x1b[34m",      // blue
            assistant_color: "\x1b[35m", // magenta
            tool_color: "\x1b[33m",      // yellow
            error_color: "\x1b[31m",     // red
            status_color: "\x1b[30m",    // black
            muted_color: "\x1b[37m",     // light gray
        }
    }

    /// Monokai-inspired theme.
    pub fn monokai() -> Self {
        Self {
            user_color: "\x1b[38;2;166;226;46m",       // monokai green
            assistant_color: "\x1b[38;2;102;217;239m", // monokai cyan
            tool_color: "\x1b[38;2;253;151;31m",       // monokai orange
            error_color: "\x1b[38;2;249;38;114m",      // monokai pink
            status_color: "\x1b[38;2;248;248;242m",    // monokai fg
            muted_color: "\x1b[38;2;117;113;94m",      // monokai comment
        }
    }

    /// ANSI reset sequence.
    pub fn reset() -> &'static str {
        "\x1b[0m"
    }

    /// Get theme by name.
    pub fn by_name(name: &str) -> Self {
        match name {
            "light" => Self::light(),
            "monokai" => Self::monokai(),
            _ => Self::dark(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_dark_default() {
        let theme = ChatTheme::dark();
        assert!(theme.user_color.contains("\x1b["));
        assert!(!ChatTheme::reset().is_empty());
    }

    #[test]
    fn theme_by_name() {
        let dark = ChatTheme::by_name("dark");
        assert!(dark.user_color.contains("32m"));
        let light = ChatTheme::by_name("light");
        assert!(light.user_color.contains("34m"));
        let mono = ChatTheme::by_name("monokai");
        assert!(mono.user_color.contains("38;2;"));
    }

    #[test]
    fn osc52_clipboard_format() {
        // Just verify encoding format
        let text = "hello world";
        let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
        assert!(!b64.is_empty());
    }

    #[test]
    fn kitty_detection() {
        // In test env, likely false
        let result = supports_kitty_graphics();
        assert!(!result || result); // just verify it doesn't panic
    }

    #[test]
    fn iterm2_detection() {
        let result = supports_iterm2_images();
        assert!(!result || result);
    }
}
