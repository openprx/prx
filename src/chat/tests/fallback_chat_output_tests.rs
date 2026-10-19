#![cfg(test)]

use super::*;

#[test]
fn fallback_chat_output_preserves_lf_for_piped_stdout() {
    assert_eq!(format_fallback_chat_output_for("a\nb", false), "a\nb\n");
}

#[test]
fn fallback_chat_output_uses_crlf_for_terminal_stdout() {
    assert_eq!(format_fallback_chat_output_for("a\nb", true), "a\r\nb\r\n");
}
