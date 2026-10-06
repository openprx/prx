//! The crate's single CommonMark fenced-code-block model.
//!
//! It lived inside the agent loop module (the former `agent/loop_.rs`) while the response cleaner was its only
//! consumer. The chat renderer needs the same rules, and a presentation module
//! must not reach into an orchestration module's internals (AGENTS.md 6.4), so
//! the model sits here as a leaf utility both sides depend on inward.

/// The opening delimiter of a CommonMark fenced code block: which character
/// opened it and how long the run was.
///
/// A single `in_fenced_block` boolean cannot represent this, and getting it
/// wrong is user-visible in both directions. It treats every fence-looking line
/// as a toggle, so a `~~~text` block whose body contains the line
/// ```` ```example ```` is "closed" by that body line and reopened by its real
/// `~~~` closer: prose after the block is then mistaken for code and a
/// fabricated marker in it survives both the cleaner and the detector. The
/// mirror image is a ```` ````text ```` block containing a bare ```` ``` ````
/// line: the short run is mistaken for a closer and the model's own code is
/// edited. Both inputs are valid CommonMark and pulldown-cmark parses them the
/// way this type does.
#[derive(Clone, Copy)]
pub(crate) struct CodeFence {
    /// The fence character, `` ` `` or `~`.
    marker: u8,
    /// Length of the opening run. A closer must be at least this long.
    length: usize,
}

impl CodeFence {
    /// `line` with its fence indentation removed, or `None` when the line is
    /// indented too far to be a fence at all: CommonMark allows a fence up to
    /// three leading spaces, and four turn the line into indented content.
    pub(crate) fn undented(line: &str) -> Option<&str> {
        let body = line.trim_start_matches(' ');
        (line.len() - body.len() <= 3).then_some(body)
    }

    /// Parse `line` as the opening fence of a code block.
    pub(crate) fn open(line: &str) -> Option<Self> {
        let body = Self::undented(line)?;
        let marker = match body.as_bytes().first()? {
            b'`' => b'`',
            b'~' => b'~',
            _ => return None,
        };
        let length = body.bytes().take_while(|byte| *byte == marker).count();
        if length < 3 {
            return None;
        }
        // A backtick fence's info string may not contain a backtick; such a
        // line is ordinary paragraph text, not a fence.
        if marker == b'`' && body[length..].contains('`') {
            return None;
        }
        Some(Self { marker, length })
    }

    /// True when `line` closes this block: the same fence character, a run at
    /// least as long as the opener's, and nothing after the run but whitespace.
    pub(crate) fn closes(self, line: &str) -> bool {
        let Some(body) = Self::undented(line) else {
            return false;
        };
        let length = body.bytes().take_while(|byte| *byte == self.marker).count();
        length >= self.length && body[length..].trim().is_empty()
    }

    /// The opening fence's info string — the text after the opening run,
    /// trimmed. Empty when the fence carries no language tag.
    ///
    /// `line` is the line this fence was opened from; any other line yields an
    /// empty string rather than a slice of unrelated text.
    // Gated because the only caller is `chat::renderer`, which `chat::mod`
    // compiles only under `terminal-tui`; ungated this is dead code there.
    #[cfg(feature = "terminal-tui")]
    pub(crate) fn info(self, line: &str) -> &str {
        Self::undented(line)
            .and_then(|body| body.get(self.length..))
            .map_or("", str::trim)
    }
}

/// Walk `text` line by line, reporting for each line whether it belongs to a
/// fenced code block. The fence delimiters themselves count as code because
/// they too must be copied out verbatim.
///
/// Single source of truth for the fence model. The cleaner
/// `strip_assistant_fabricated_tool_markers` walks the text with this
/// function and the detector `contains_assistant_fabricated_tool_marker` is
/// defined in terms of the cleaner, so the retry decision and the delivered
/// text can never disagree about where code starts and ends. An unterminated
/// fence runs to the end of the text, and `str::lines` gives CRLF input exactly
/// the lines LF input gets.
pub(crate) fn walk_markdown_lines(text: &str) -> impl Iterator<Item = (&str, bool)> {
    let mut fence: Option<CodeFence> = None;
    text.lines().map(move |line| {
        let in_code = match fence {
            Some(open) => {
                if open.closes(line) {
                    fence = None;
                }
                true
            }
            None => {
                fence = CodeFence::open(line);
                fence.is_some()
            }
        };
        (line, in_code)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_requires_a_run_of_three_and_a_clean_info_string() {
        assert!(CodeFence::open("```").is_some(), "a bare run of three opens a fence");
        assert!(CodeFence::open("~~~~text").is_some(), "a tilde run opens a fence");
        assert!(
            CodeFence::open("   ```rust").is_some(),
            "three spaces still open a fence"
        );
        assert!(CodeFence::open("``rust").is_none(), "a run of two is not a fence");
        assert!(
            CodeFence::open("    ```rust").is_none(),
            "four spaces make the line indented content"
        );
        assert!(
            CodeFence::open("```rust `x`").is_none(),
            "a backtick info string is paragraph text"
        );
        assert!(CodeFence::open("plain prose").is_none());
    }

    #[test]
    fn closes_requires_the_same_marker_and_at_least_the_opening_run() {
        let tilde = CodeFence::open("~~~text").expect("test: the tilde fence opens");
        assert!(
            !tilde.closes("```example"),
            "a backtick line cannot close a tilde fence"
        );
        assert!(tilde.closes("~~~"), "the matching marker closes it");

        let long = CodeFence::open("````rust").expect("test: the long fence opens");
        assert!(!long.closes("```"), "a shorter run is content, not a closer");
        assert!(long.closes("`````"), "a longer run closes it");
        assert!(!long.closes("````rust"), "a closer may not carry an info string");
        assert!(long.closes("   ````   "), "indent and trailing spaces are tolerated");
    }

    // Follows the gate on `CodeFence::info` itself: without `terminal-tui` the
    // method does not exist, so neither can its unit test.
    #[cfg(feature = "terminal-tui")]
    #[test]
    fn info_returns_the_trimmed_info_string_of_the_opening_fence() {
        let plain = CodeFence::open("```").expect("test: the bare fence opens");
        assert_eq!(plain.info("```"), "");

        let tagged = CodeFence::open("```rust  ").expect("test: the tagged fence opens");
        assert_eq!(tagged.info("```rust  "), "rust");

        let long = CodeFence::open("   ````rust,ignore title=\"demo\"").expect("test: the long fence opens");
        assert_eq!(
            long.info("   ````rust,ignore title=\"demo\""),
            "rust,ignore title=\"demo\""
        );

        let opener = CodeFence::open("`````").expect("test: the five-run fence opens");
        assert_eq!(opener.info("```"), "", "a line shorter than the run yields nothing");
    }

    #[test]
    fn walk_markdown_lines_keeps_a_shorter_inner_run_inside_the_block() {
        let walked =
            walk_markdown_lines("````text\n```\nstill code\n````\nprose after\n").collect::<Vec<(&str, bool)>>();
        assert_eq!(
            walked,
            vec![
                ("````text", true),
                ("```", true),
                ("still code", true),
                ("````", true),
                ("prose after", false),
            ]
        );
    }

    /// R-B4 F5: the fence model's own module pins that CRLF input walks the
    /// same lines and fences LF input does; before, only a `loop_.rs`
    /// cleaner table covered it.
    ///
    /// MUTATION GUARD: walk with `split('\n')` instead of `lines()` and each
    /// CRLF line keeps its `\r`, so this goes red.
    #[test]
    fn walk_markdown_lines_walks_crlf_input_like_lf() {
        let lf = "~~~text\n```example\n~~~\nprose after\n";
        let crlf = lf.replace('\n', "\r\n");
        let walked_lf = walk_markdown_lines(lf).collect::<Vec<(&str, bool)>>();
        assert_eq!(
            walked_lf,
            vec![
                ("~~~text", true),
                ("```example", true),
                ("~~~", true),
                ("prose after", false),
            ]
        );
        assert_eq!(walk_markdown_lines(&crlf).collect::<Vec<(&str, bool)>>(), walked_lf);
    }
}
