#![cfg(test)]

use super::slice_display_columns;

#[test]
fn slice_display_columns_uses_terminal_width() {
    assert_eq!(slice_display_columns("○ line-1", 2, 6), "line");
    // `\u{ac00}\u{ac01}` are East Asian Wide syllables: 2 display columns each, 4 in total.
    assert_eq!(slice_display_columns("\u{ac00}\u{ac01} line", 0, 4), "\u{ac00}\u{ac01}");
}
