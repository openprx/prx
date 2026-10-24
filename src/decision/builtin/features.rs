//! Language-neutral features of a decision state v1 (CDP-PLAN 1.3; SP-CDP-A
//! 5.7).
//!
//! Every feature comes from structure or trusted state, never from a
//! natural-language word: the text scan looks for URL, path and code shapes
//! and at whether any text is present at all. Inside a shape it reads a
//! letter only as a class (a letter, digit or combining mark of any script),
//! never which letter it is.
//! Replacing the letters of a request with their translation, or with random
//! letters of the same length, cannot change a feature; the metamorphic test
//! in [`super`] proves it, including for scripts written without spaces,
//! full-width punctuation and bidirectional marks. The catalog-naming signal
//! and the failure record arrive as the trusted `exact_mention` and
//! `failed_before` flags of each candidate (CDP-PLAN M16; D-061 M-3).
//!
//! Extraction is lenient: a field that is missing or has the wrong type reads
//! as its neutral value, because the builtin must answer every request. The
//! state contract itself is enforced where the state is produced (CDP-19).

use serde_json::Value;

use crate::decision::protocol::is_format_character;
use crate::util::is_combining_mark;

/// Structural signals of a request text. None of them looks at a letter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextSignals {
    /// The text carries at least one letter or digit, in any script.
    pub has_text: bool,
    /// Language-neutral request weight: one per contiguous alphanumeric run.
    /// Text in a script that normally omits spaces is conservatively treated
    /// as substantive. This distinguishes a brief conversational turn without
    /// making the result depend on the number of code points used per word.
    pub request_weight: usize,
    /// A `scheme://` URL shape.
    pub url: bool,
    /// A file-path shape: `/seg/seg` or `/seg.ext`; `~/`, `./`, `../`, `.\`
    /// or `..\` followed by a path; or a drive letter followed by `:\` or
    /// `:/` and a path.
    pub path: bool,
    /// Inline code or a code fence (a backtick).
    pub code: bool,
}

impl TextSignals {
    /// Whether the text names its target by structure (a URL, a path or
    /// code), which is decisive without any word.
    #[must_use]
    pub const fn structural(self) -> bool {
        self.url || self.path || self.code
    }
}

/// A character as the structure checks read it. The full-width forms block
/// (Unicode U+FF01..U+FF5E, typed by CJK input methods: full-width colon,
/// slash, backtick, Latin letters and digits) folds to its ASCII
/// counterpart, and the ideographic space (U+3000) to a space. Written as
/// Unicode-class ranges (D-061 L-4).
fn fold_width(character: char) -> char {
    const FULL_WIDTH: std::ops::RangeInclusive<u32> = 0xFF01..=0xFF5E;
    const FULL_WIDTH_OFFSET: u32 = 0xFEE0;
    const IDEOGRAPHIC_SPACE: u32 = 0x3000;
    let code = u32::from(character);
    if FULL_WIDTH.contains(&code) {
        char::from_u32(code - FULL_WIDTH_OFFSET).unwrap_or(character)
    } else if code == IDEOGRAPHIC_SPACE {
        ' '
    } else {
        character
    }
}

/// The text as the structure checks read it: format characters (the whole
/// Unicode category Cf, the same set the protocol refuses in identity
/// strings: bidirectional marks, zero-width characters, the word joiner
/// family, the Mongolian vowel separator, the byte-order mark) skipped, so a
/// mark an input method or RTL client inserts anywhere changes nothing;
/// full-width forms folded. A cloneable iterator, so lookahead never
/// allocates.
fn structure_chars(text: &str) -> impl Iterator<Item = char> + Clone + '_ {
    text.chars()
        .filter(|character| !is_format_character(*character))
        .map(fold_width)
}

/// How a path starts, which decides how much it must show to count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathStart {
    /// A bare `/`. Slash-separated prose (`and/or`, a list of city names,
    /// months, prices per unit, in any script) must not count, so the
    /// first character after it is ASCII, a path of two or more segments
    /// has a wholly ASCII first segment, and all-number segments (a date)
    /// are not a path (D-061 amendment N-1, amendment 2 N-5).
    Bare {
        /// A letter or digit of a script written without spaces stands
        /// directly before the `/` (a letter of any other script makes the
        /// slash part of a word, as an ASCII letter does). The
        /// first segment is then read in ASCII only, so glued prose (a month
        /// list) finds no second segment; later segments may be localized,
        /// but only a path of three or more segments or with an extension
        /// counts then, so `100 per kg per day` glued to its words stays
        /// prose (D-061 amendment 3 N-7).
        glued: bool,
    },
    /// `~/`, `./`, `../`, `.\` or `..\`: unambiguous, any script follows.
    Prefixed,
    /// A drive letter, `:` and `\` or `/`: unambiguous.
    Drive,
    /// A drive letter, `:` and a yen or won sign. The sign is also a
    /// currency sign, so an option letter, a colon and a price (`A:` sign
    /// `99`, `99` per month, `99` and up) is not a path: only yen and won
    /// signs separate its segments, and its first segment must not start
    /// with a digit (D-061 amendment 2 N-6, amendment 3 N-6b).
    CurrencyDrive,
}

/// Unicode data (D-061 amendment N-4): the yen sign U+00A5, the full-width
/// yen sign U+FFE5 and the won sign U+20A9, which Japanese and Korean
/// Windows display for the path separator `\` and users type in its place.
const YEN_WON_SEPARATORS: [u32; 3] = [0x00A5, 0xFFE5, 0x20A9];

/// Whether `character` is a yen or won sign.
fn is_yen_or_won(character: char) -> bool {
    YEN_WON_SEPARATORS.contains(&u32::from(character))
}

/// Whether `character` separates path segments after a start of this kind.
fn is_separator(character: char, start: PathStart) -> bool {
    match start {
        PathStart::CurrencyDrive => is_yen_or_won(character),
        PathStart::Drive => matches!(character, '/' | '\\') || is_yen_or_won(character),
        PathStart::Bare { .. } | PathStart::Prefixed => matches!(character, '/' | '\\'),
    }
}

/// Unicode data (D-061 amendment 4): the blocks of the scripts written
/// without spaces between words, whose letters can stand directly before a
/// path. Han: CJK Symbols and Punctuation (U+3000..U+303F, for the
/// ideographic iteration and number marks), CJK Unified Ideographs and
/// Extension A (U+3400..U+4DBF, U+4E00..U+9FFF), Compatibility Ideographs
/// (U+F900..U+FAFF) and the supplementary ideographic planes
/// (U+20000..U+3134F); Bopomofo (U+3100..U+312F, U+31A0..U+31BF); Hiragana
/// and Katakana (U+3040..U+30FF, U+31F0..U+31FF, half-width U+FF66..U+FF9F);
/// Thai (U+0E00..U+0E7F); Lao (U+0E80..U+0EFF); Tibetan (U+0F00..U+0FFF);
/// Myanmar (U+1000..U+109F, U+A9E0..U+A9FF, U+AA60..U+AA7F); Khmer
/// (U+1780..U+17FF, U+19E0..U+19FF).
const NO_SPACE_SCRIPTS: [(u32, u32); 18] = [
    (0x0E00, 0x0E7F),
    (0x0E80, 0x0EFF),
    (0x0F00, 0x0FFF),
    (0x1000, 0x109F),
    (0x1780, 0x17FF),
    (0x19E0, 0x19FF),
    (0x3000, 0x303F),
    (0x3040, 0x30FF),
    (0x3100, 0x312F),
    (0x31A0, 0x31BF),
    (0x31F0, 0x31FF),
    (0x3400, 0x4DBF),
    (0x4E00, 0x9FFF),
    (0xA9E0, 0xA9FF),
    (0xAA60, 0xAA7F),
    (0xF900, 0xFAFF),
    (0xFF66, 0xFF9F),
    (0x20000, 0x3134F),
];

/// Whether `character` belongs to a script written without spaces between
/// words (see [`NO_SPACE_SCRIPTS`]).
fn is_no_space_script(character: char) -> bool {
    let code = u32::from(character);
    NO_SPACE_SCRIPTS
        .iter()
        .any(|(first, last)| (*first..=*last).contains(&code))
}

/// Whether `character` makes a following slash, colon or drive letter part
/// of a word (D-061 amendment 4): any letter or digit, except those of the
/// scripts written without spaces. So `Perú/Chile/Bolivia`, `sɛ/no/yes` and
/// `русский/English/Deutsch` are prose like `Peru/Chile/Bolivia`, while a
/// Han, Kana or Thai word can stand directly before a path.
fn is_word_character(character: char) -> bool {
    character.is_alphanumeric() && !is_no_space_script(character)
}

/// Whether `character` may appear in a path segment: ASCII letters, digits
/// and `._~-`, and unless `ascii_only`, a letter, digit or combining mark of
/// any script (localized directory names, accented names in NFC or NFD).
fn is_segment_character(character: char, ascii_only: bool) -> bool {
    character.is_ascii_alphanumeric()
        || matches!(character, '.' | '_' | '-' | '~')
        || (!ascii_only && (character.is_alphanumeric() || is_combining_mark(character)))
}

/// What a path scan saw, segment by segment.
#[derive(Debug, Clone, Copy)]
struct PathShape {
    segments: usize,
    /// A segment has a letter or digit after a `.`.
    extension: bool,
    first_ascii: bool,
    /// The first segment starts with a digit (of any script).
    first_starts_numeric: bool,
    /// A segment after the first has a character outside ASCII.
    later_unicode: bool,
    all_numeric: bool,
}

impl PathShape {
    const EMPTY: Self = Self {
        segments: 0,
        extension: false,
        first_ascii: false,
        first_starts_numeric: false,
        later_unicode: false,
        all_numeric: true,
    };

    /// Record a finished segment.
    const fn close(&mut self, ascii: bool, numeric: bool, starts_numeric: bool) {
        if self.segments == 0 {
            self.first_ascii = ascii;
            self.first_starts_numeric = starts_numeric;
        } else {
            self.later_unicode |= !ascii;
        }
        self.all_numeric &= numeric;
        self.segments += 1;
    }

    /// Whether the shape is a path for a start of this kind.
    const fn counts(self, start: PathStart) -> bool {
        match start {
            PathStart::Bare { glued } => {
                let enough = if self.segments >= 2 {
                    self.first_ascii
                } else {
                    self.segments == 1 && self.extension
                };
                let localized_after_glue_is_clear =
                    !glued || !self.later_unicode || self.segments >= 3 || self.extension;
                enough && !self.all_numeric && localized_after_glue_is_clear
            }
            PathStart::Prefixed | PathStart::Drive => self.segments >= 1,
            PathStart::CurrencyDrive => self.segments >= 1 && !self.first_starts_numeric,
        }
    }
}

/// Whether the characters after a path start form a path (see
/// [`PathStart`] for what each start needs). A bare `/` needs two segments
/// (`/etc/hosts`) or one with an extension (`/notes.md`), so a slash command
/// such as `/help` is not a path; a prefixed or drive start needs one
/// segment. Segments are made of [`is_segment_character`]s, separated by
/// [`is_separator`]s; the path ends at white space or any other punctuation.
/// A segment is a number when it has only digits (of any script) and `.`.
fn path_follows(ahead: impl Iterator<Item = char>, start: PathStart) -> bool {
    let mut shape = PathShape::EMPTY;
    let mut length = 0_usize;
    let mut ascii = true;
    let mut numeric = true;
    let mut starts_numeric = false;
    let mut starts_ascii = true;
    let mut after_dot = false;
    for character in ahead {
        if is_separator(character, start) {
            if length == 0 {
                break;
            }
            shape.close(ascii, numeric, starts_numeric);
            length = 0;
            ascii = true;
            numeric = true;
            after_dot = false;
            continue;
        }
        let ascii_only = match start {
            PathStart::Bare { glued } => shape.segments == 0 && (glued || length == 0),
            PathStart::Prefixed | PathStart::Drive | PathStart::CurrencyDrive => false,
        };
        // A glued path is written inside text without spaces, so the words
        // can follow it without a space too: an ASCII name that turns into
        // another script ends the path there (`/etc/hosts` followed by a
        // glued word), while a name that starts in another script is a
        // localized segment (D-061 amendment 4).
        if matches!(start, PathStart::Bare { glued: true })
            && shape.segments > 0
            && length > 0
            && starts_ascii
            && !character.is_ascii()
        {
            break;
        }
        if !is_segment_character(character, ascii_only) {
            break;
        }
        if after_dot && character.is_alphanumeric() {
            shape.extension = true;
        }
        after_dot = character == '.';
        if length == 0 {
            starts_numeric = character.is_numeric();
            starts_ascii = character.is_ascii();
        }
        ascii &= character.is_ascii();
        numeric &= character.is_numeric() || character == '.';
        length += 1;
    }
    if length > 0 {
        shape.close(ascii, numeric, starts_numeric);
    }
    shape.counts(start)
}

/// Whether `ahead` starts with `prefix`.
fn starts_with(mut ahead: impl Iterator<Item = char>, prefix: &str) -> bool {
    prefix.chars().all(|expected| ahead.next() == Some(expected))
}

/// Drop the first `count` characters of `ahead`.
fn skip(mut ahead: impl Iterator<Item = char>, count: usize) -> impl Iterator<Item = char> {
    for _ in 0..count {
        if ahead.next().is_none() {
            break;
        }
    }
    ahead
}

/// Scan `text` once, without allocating.
///
/// No check depends on what precedes a shape except that a word character
/// ([`is_word_character`]: a letter or digit of any script written with
/// spaces) directly before it makes it part of a word (`a/b`, `Perú/Chile`
/// and `русский/English` are not paths, `ratio 3:1` is not a URL). "Directly before" is the last
/// base character: combining marks are skipped, so NFC and NFD read alike
/// (D-061 amendment 3 N-8). White space, punctuation, any script without
/// spaces (CJK, Thai), full-width forms and invisible format marks around or
/// inside a shape change nothing, so a translation of the words around it
/// cannot change a signal (D-061 M-1). Localized names inside a path, an
/// internationalized host, a full-width colon before a path and the yen or
/// won separator after a drive are shapes too (D-061 amendment).
#[must_use]
pub fn text_signals(text: &str) -> TextSignals {
    let mut signals = TextSignals::default();
    let mut previous: Option<char> = None;
    let mut chars = structure_chars(text);
    while let Some(character) = chars.next() {
        let ahead = chars.clone();
        let in_word = previous.is_some_and(is_word_character);
        if character.is_alphanumeric() {
            signals.has_text = true;
            if is_no_space_script(character) {
                signals.request_weight = signals.request_weight.max(3);
            } else if !previous.is_some_and(char::is_alphanumeric) {
                signals.request_weight = signals.request_weight.saturating_add(1);
            }
        }
        match character {
            '`' => signals.code = true,
            ':' if in_word && starts_with(ahead.clone(), "//") => {
                let host = skip(ahead, 2).next();
                if host.is_some_and(|host| host.is_alphanumeric() || host == '[') {
                    signals.url = true;
                }
            }
            // Only the slashes of a literal `://` are not a path start: a
            // colon written before a path (`file: /etc/hosts`, or a
            // full-width colon glued to it) is not a URL.
            '/' if !in_word && previous != Some('/') && !(previous == Some(':') && starts_with(ahead.clone(), "/")) => {
                // `previous` is a base character (marks are skipped), and
                // every letter outside the scripts written without spaces
                // makes the slash part of a word, so a glued slash follows a
                // letter or digit of such a script.
                let glued = previous.is_some_and(|before| !before.is_ascii() && before.is_alphanumeric());
                if path_follows(ahead, PathStart::Bare { glued }) {
                    signals.path = true;
                }
            }
            '~' if !in_word && starts_with(ahead.clone(), "/") => {
                if path_follows(skip(ahead, 1), PathStart::Prefixed) {
                    signals.path = true;
                }
            }
            '.' if !in_word && previous != Some('.') => {
                let prefixed = ["/", "\\", "./", ".\\"]
                    .into_iter()
                    .find(|prefix| starts_with(ahead.clone(), prefix));
                if let Some(prefix) = prefixed {
                    if path_follows(skip(ahead, prefix.len()), PathStart::Prefixed) {
                        signals.path = true;
                    }
                }
            }
            letter if letter.is_ascii_alphabetic() && !in_word => {
                let mut rest = ahead;
                let start = match (rest.next(), rest.next()) {
                    (Some(':'), Some('\\' | '/')) => Some(PathStart::Drive),
                    (Some(':'), Some(separator)) if is_yen_or_won(separator) => Some(PathStart::CurrencyDrive),
                    _ => None,
                };
                if start.is_some_and(|start| path_follows(rest, start)) {
                    signals.path = true;
                }
            }
            _ => {}
        }
        if !is_combining_mark(character) {
            previous = Some(character);
        }
    }
    signals
}

/// The action kinds of the state v1 contract (the `as_str` codes of
/// `CausalActionKind`, CDP-PLAN 1.9). The decision module does not depend on
/// `causal_tree`, so it reads the codes itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Action {
    /// `answer`
    Answer,
    /// `retrieve`
    Retrieve,
    /// `discover`
    Discover,
    /// `use_tool`
    UseTool,
    /// `delegate`
    Delegate,
    /// `clarify`
    Clarify,
    /// `request_approval`
    RequestApproval,
    /// `refuse`
    Refuse,
}

impl Action {
    /// The action named by a state v1 code.
    #[must_use]
    pub fn from_code(code: &str) -> Option<Self> {
        Some(match code {
            "answer" => Self::Answer,
            "retrieve" => Self::Retrieve,
            "discover" => Self::Discover,
            "use_tool" => Self::UseTool,
            "delegate" => Self::Delegate,
            "clarify" => Self::Clarify,
            "request_approval" => Self::RequestApproval,
            "refuse" => Self::Refuse,
            _ => return None,
        })
    }
}

/// The features of one offered candidate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CandidateFeatures<'s> {
    /// The candidate id (`{action}-{target}[-{ns8}]`).
    pub id: &'s str,
    /// The action kind, when the code is known.
    pub action: Option<Action>,
    /// The capability target code.
    pub target: &'s str,
    /// The risk floor of the side-effect class, in `[0, 1]`.
    pub side_effect_risk: f64,
    /// The trusted side-effect class permits process execution.
    pub executes_process: bool,
    /// An approval is required and not yet satisfied.
    pub approval_required: bool,
    /// The capability is available.
    pub available: bool,
    /// The user named this capability exactly (trusted, from the catalog).
    pub exact_mention: bool,
    /// A call of this very candidate's capability failed before in this
    /// turn (the trusted state v1 field `failed_before`, D-061 M-3).
    pub failed_before: bool,
}

/// The features of a whole state.
#[derive(Debug, Clone, PartialEq)]
pub struct StateFeatures<'s> {
    /// Structural signals of the latest user turn.
    pub text: TextSignals,
    /// Retries so far.
    pub retry_count: u64,
    /// An approval is pending.
    pub approval_pending: bool,
    /// Query/tool steps that failed, were denied or were cancelled; Router
    /// training-history observations do not predict user-answer evidence.
    pub unsuccessful_steps: u64,
    /// Failed, denied or cancelled non-provider work. A prior provider attempt
    /// belongs to the earlier conversation segment and does not make a new
    /// plain user message structurally undecidable.
    pub unsuccessful_action_steps: u64,
    /// Completed retrievals or discoveries that succeeded.
    pub successful_lookups: u64,
    /// Artifacts younger than [`super::weights::FRESH_ARTIFACT_MS`].
    pub fresh_artifacts: u64,
    /// Artifacts older than [`super::weights::STALE_ARTIFACT_MS`].
    pub stale_artifacts: u64,
    /// Any query/tool step completed or any user-evidence artifact exists.
    pub has_history: bool,
    /// The admitted source belongs to a conversation with an earlier turn.
    pub has_conversation_history: bool,
    /// User-evidence artifacts present at any age.
    pub artifact_count: u64,
    /// The offered candidates, in state order.
    pub candidates: Vec<CandidateFeatures<'s>>,
}

impl<'s> StateFeatures<'s> {
    /// No candidate is named exactly and the text has no structural target:
    /// nothing is decisive.
    #[must_use]
    pub fn undecided(&self) -> bool {
        !self.text.structural() && !self.candidates.iter().any(|candidate| candidate.exact_mention)
    }

    /// The one candidate with id `id`. An id listed twice names no
    /// candidate: sealing refuses such a state (D-061 L-2), and the builtin
    /// never lets list order pick between two.
    #[must_use]
    pub fn candidate(&self, id: &str) -> Option<&CandidateFeatures<'s>> {
        let mut matching = self.candidates.iter().filter(|candidate| candidate.id == id);
        let first = matching.next()?;
        if matching.next().is_some() { None } else { Some(first) }
    }
}

/// Read the features of a state v1 object.
#[must_use]
pub fn extract(state: &Value) -> StateFeatures<'_> {
    let text = state
        .get("latest_user_turn")
        .and_then(Value::as_str)
        .map(text_signals)
        .unwrap_or_default();
    let steps = array(state, "completed_steps");
    let artifacts = array(state, "artifacts");
    let unsuccessful_steps = steps
        .iter()
        .filter(|step| {
            str_at(step, "kind") != "router_history"
                && matches!(str_at(step, "status"), "failed" | "denied" | "cancelled")
        })
        .count() as u64;
    let unsuccessful_action_steps = steps
        .iter()
        .filter(|step| {
            !matches!(str_at(step, "kind"), "provider" | "router_history")
                && matches!(str_at(step, "status"), "failed" | "denied" | "cancelled")
        })
        .count() as u64;
    let successful_lookups = steps
        .iter()
        .filter(|step| str_at(step, "status") == "ok" && matches!(str_at(step, "kind"), "retrieve" | "discover"))
        .count() as u64;
    let age = |artifact: &Value| artifact.get("age_ms").and_then(Value::as_u64);
    let fresh_artifacts = artifacts
        .iter()
        .filter(|artifact| age(artifact).is_some_and(|age| age < super::weights::FRESH_ARTIFACT_MS))
        .count() as u64;
    let stale_artifacts = artifacts
        .iter()
        .filter(|artifact| age(artifact).is_some_and(|age| age > super::weights::STALE_ARTIFACT_MS))
        .count() as u64;
    let offered = array(state, "candidates");
    let mut candidates = Vec::with_capacity(offered.len());
    for candidate in offered {
        candidates.push(CandidateFeatures {
            id: str_at(candidate, "id"),
            action: Action::from_code(str_at(candidate, "action")),
            target: str_at(candidate, "target"),
            side_effect_risk: super::weights::side_effect_risk(str_at(candidate, "side_effect")),
            executes_process: str_at(candidate, "side_effect") == "process_exec",
            approval_required: str_at(candidate, "approval") == "required",
            available: candidate.get("available").and_then(Value::as_bool).unwrap_or(true),
            exact_mention: candidate.get("exact_mention").and_then(Value::as_bool).unwrap_or(false),
            failed_before: candidate.get("failed_before").and_then(Value::as_bool).unwrap_or(false),
        });
    }
    StateFeatures {
        text,
        retry_count: state.get("retry_count").and_then(Value::as_u64).unwrap_or(0),
        approval_pending: state.get("approval_pending").and_then(Value::as_bool).unwrap_or(false),
        unsuccessful_steps,
        unsuccessful_action_steps,
        successful_lookups,
        fresh_artifacts,
        stale_artifacts,
        has_history: steps.iter().any(|step| str_at(step, "kind") != "router_history") || !artifacts.is_empty(),
        has_conversation_history: state
            .get("has_conversation_history")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        artifact_count: artifacts.len() as u64,
        candidates,
    }
}

fn array<'v>(value: &'v Value, key: &str) -> &'v [Value] {
    value.get(key).and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

fn str_at<'v>(value: &'v Value, key: &str) -> &'v str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn structural_shapes_are_found_without_reading_letters() {
        let found = |text: &str| text_signals(text);
        for path in [
            "open /var/log/app.log",
            "open /etc/hosts",
            "see /notes.md",
            "open ~/notes.md",
            "run ./build.sh",
            "run ../tools/build.sh",
            r"run .\build.bat",
            r"run ..\tools\build.bat",
            r"open C:\data\x.txt",
            "open D:/data/x.txt",
            "(/etc/hosts)",
        ] {
            assert!(found(path).path, "{path}");
        }
        for not_path in [
            "a/b c",
            "and/or",
            "use /help",
            "3/4",
            "file.txt",
            "e.g. this",
            "~",
            "./",
            "/",
        ] {
            assert!(!found(not_path).path, "{not_path}");
        }
        assert!(found("see https://example.com/a").url);
        assert!(found("(https://example.com)").url);
        assert!(!found("ratio 3:1").url);
        assert!(!found("a :// b").url);
        assert!(found("use `grep`").code);
        assert!(found("x").has_text);
        let quiet = found("just some words here");
        assert!(!quiet.structural());
        assert_eq!(found("   \n  "), TextSignals::default());
    }

    /// D-061 M-1: shapes glued to text without spaces, written in full-width
    /// forms, or preceded by invisible marks are found. The words are built
    /// from Unicode-class code points (D-061 L-4: no CJK prose in source).
    #[test]
    fn shapes_survive_unspaced_scripts_full_width_forms_and_marks() {
        // Unicode data: two Han ideographs, two Thai letters, one Arabic
        // letter; bidi marks, a zero-width space and a word joiner before
        // and inside shapes; the full-width forms of a path.
        const HAN: [u32; 2] = [0x6253, 0x5F00];
        const THAI: [u32; 2] = [0x0E40, 0x0E1B];
        const ARABIC: [u32; 1] = [0x0627];
        let word = |points: &[u32]| {
            points
                .iter()
                .filter_map(|point| char::from_u32(*point))
                .collect::<String>()
        };
        let full_width = |ascii: &str| {
            ascii
                .chars()
                .map(|character| {
                    if ('!'..='~').contains(&character) {
                        char::from_u32(character as u32 + 0xFEE0).unwrap_or(character)
                    } else {
                        character
                    }
                })
                .collect::<String>()
        };
        for text in [
            format!("{}/var/log/app.log{}", word(&HAN), word(&HAN)),
            format!("{}/etc/hosts", word(&THAI)),
            format!("{} \u{200E}/etc/hosts", word(&ARABIC)),
            format!("{}~/notes.md", word(&HAN)),
            format!("{}./build.sh", word(&THAI)),
            format!("{}{}", word(&HAN), full_width("/etc/hosts")),
            format!("{}{}", word(&HAN), full_width(r"C:\data\x.txt")),
            // A mark inside the shape is skipped, not read as its end.
            format!("{}/var/\u{200B}log/app.log", word(&HAN)),
            format!("{}C:\u{2060}\\data\\x.txt", word(&THAI)),
            format!("{}/var/\u{180E}log/app.log", word(&HAN)),
        ] {
            assert!(text_signals(&text).path, "{text:?}");
        }
        for text in [
            format!("{}https://example.com{}", word(&HAN), word(&HAN)),
            format!("{}{}", word(&HAN), full_width("https://example.com")),
            format!("{}\u{200F}https://example.com", word(&ARABIC)),
            format!("{}https\u{200E}://example.com", word(&ARABIC)),
        ] {
            assert!(text_signals(&text).url, "{text:?}");
        }
        assert!(text_signals(&format!("{}{}", word(&HAN), full_width("`ls`"))).code);
    }

    /// D-061 amendment N-1..N-4 and the shared format set: the reviewer's
    /// probes (`R-CDP06-probes.log`) that were not shapes at `a8360c02`.
    #[test]
    fn localized_names_idn_hosts_colons_and_yen_signs_are_shapes() {
        // Unicode data: Han words (open, download, report, example, China),
        // Cyrillic words (documents, report, example, the .rf domain, three
        // city names), U+00E9 (e with acute) and U+0301 (combining acute),
        // the yen sign, full-width yen sign and won sign, and the Mongolian
        // vowel separator (category Cf).
        const HAN_OPEN: [u32; 2] = [0x6253, 0x5F00];
        const HAN_DOWNLOAD: [u32; 2] = [0x4E0B, 0x8F7D];
        const HAN_REPORT: [u32; 2] = [0x62A5, 0x544A];
        const HAN_EXAMPLE: [u32; 2] = [0x4F8B, 0x5B50];
        const HAN_CHINA: [u32; 2] = [0x4E2D, 0x56FD];
        const CYR_DOCUMENTS: [u32; 9] = [0x0414, 0x043E, 0x043A, 0x0443, 0x043C, 0x0435, 0x043D, 0x0442, 0x044B];
        const CYR_REPORT: [u32; 5] = [0x043E, 0x0442, 0x0447, 0x0451, 0x0442];
        const CYR_EXAMPLE: [u32; 6] = [0x043F, 0x0440, 0x0438, 0x043C, 0x0435, 0x0440];
        const CYR_RF: [u32; 2] = [0x0440, 0x0444];
        const CYR_MOSCOW: [u32; 6] = [0x041C, 0x043E, 0x0441, 0x043A, 0x0432, 0x0430];
        const CYR_PITER: [u32; 5] = [0x041F, 0x0438, 0x0442, 0x0435, 0x0440];
        const CYR_KAZAN: [u32; 6] = [0x041A, 0x0430, 0x0437, 0x0430, 0x043D, 0x044C];
        const E_ACUTE: u32 = 0x00E9;
        const COMBINING_ACUTE: u32 = 0x0301;
        const SEPARATORS: [u32; 3] = [0x00A5, 0xFFE5, 0x20A9];
        const MONGOLIAN_VOWEL_SEPARATOR: u32 = 0x180E;
        let word = |points: &[u32]| {
            points
                .iter()
                .filter_map(|point| char::from_u32(*point))
                .collect::<String>()
        };
        let one = |point: u32| char::from_u32(point).map(String::from).unwrap_or_default();
        let full_width_colon = one(0xFF1A);

        // N-1: localized names after a prefixed or drive start, and after a
        // bare slash whose first character is ASCII, in NFC and NFD.
        for text in [
            format!("{}~/{}/{}.pdf", word(&HAN_OPEN), word(&HAN_DOWNLOAD), word(&HAN_REPORT)),
            format!("open ~/{}/{}.txt", word(&CYR_DOCUMENTS), word(&CYR_REPORT)),
            format!("open /home/{}/{}", word(&HAN_DOWNLOAD), word(&HAN_REPORT)),
            format!("open D:\\{}\\{}.docx", word(&HAN_DOWNLOAD), word(&HAN_REPORT)),
            format!("see /caf{}.txt", one(E_ACUTE)),
            format!("see /cafe{}.txt", one(COMBINING_ACUTE)),
            format!("run ./{}.sh", word(&CYR_REPORT)),
            format!(r"open ..\{}", word(&HAN_REPORT)),
        ] {
            assert!(text_signals(&text).path, "{text:?}");
        }
        // Slash-separated prose in any script is still not a path.
        for text in [
            format!("{}/{}/{}", word(&CYR_MOSCOW), word(&CYR_PITER), word(&CYR_KAZAN)),
            format!("see /{}/{}", word(&CYR_MOSCOW), word(&CYR_PITER)),
            // A bare slash still needs an ASCII first character (I-10).
            format!("see /{}.txt", word(&CYR_REPORT)),
            "London/Paris/Rome".to_string(),
            format!("abc{}/etc/hosts", one(MONGOLIAN_VOWEL_SEPARATOR)),
        ] {
            assert!(!text_signals(&text).path, "{text:?}");
        }
        // N-2: internationalized hosts.
        for text in [
            format!("see https://{}.{}/a", word(&CYR_EXAMPLE), word(&CYR_RF)),
            format!("see https://{}.{}", word(&HAN_EXAMPLE), word(&HAN_CHINA)),
        ] {
            assert!(text_signals(&text).url, "{text:?}");
        }
        // N-3: a full-width colon glued before a path.
        let colon = format!("{}{}/etc/hosts", word(&HAN_OPEN), full_width_colon);
        assert!(text_signals(&colon).path, "{colon:?}");
        assert!(text_signals("file:/etc/hosts").path);
        assert!(!text_signals("https://example.com").path);
        // N-4: yen and won signs after a drive letter, and only there.
        for separator in SEPARATORS {
            let sign = one(separator);
            let text = format!("open C:{sign}Users{sign}data{sign}x.txt");
            assert!(text_signals(&text).path, "{text:?}");
            let not_drive = format!("price 5{sign}10{sign}20");
            assert!(!text_signals(&not_drive).path, "{not_drive:?}");
        }
    }

    /// D-061 amendment 2 N-5, N-6: month lists, prices per unit, a date and
    /// prices written with a yen sign after an option letter are prose in
    /// every script, while the localized paths above stay paths. Each of
    /// these was a path at `cded6a71` (R-CDP06 round 3).
    #[test]
    fn slashed_lists_prices_and_dates_are_prose_in_any_script() {
        // Unicode data: the Han and Hangul month signs, Han "of the report",
        // "price", "yuan", "day", "plan", Katakana "plan", Devanagari digits
        // 2026, the full-width colon and comma, the yen and full-width yen
        // signs.
        const HAN_MONTH: u32 = 0x6708;
        const HANGUL_MONTH: u32 = 0xC6D4;
        const HAN_OF_REPORT: [u32; 3] = [0x7684, 0x62A5, 0x544A];
        const HAN_PRICE: [u32; 2] = [0x4EF7, 0x683C];
        const HAN_YUAN: u32 = 0x5143;
        const HAN_DAY: u32 = 0x5929;
        const HAN_PLAN: [u32; 2] = [0x5957, 0x9910];
        const KATAKANA_PLAN: [u32; 3] = [0x30D7, 0x30E9, 0x30F3];
        const DEVANAGARI_2026: [u32; 4] = [0x0968, 0x0966, 0x0968, 0x096C];
        const FULL_WIDTH_COLON: u32 = 0xFF1A;
        const FULL_WIDTH_COMMA: u32 = 0xFF0C;
        const YEN: u32 = 0x00A5;
        const FULL_WIDTH_YEN: u32 = 0xFFE5;
        let word = |points: &[u32]| {
            points
                .iter()
                .filter_map(|point| char::from_u32(*point))
                .collect::<String>()
        };
        let one = |point: u32| char::from_u32(point).map(String::from).unwrap_or_default();
        let (month, wol, colon, comma) = (
            one(HAN_MONTH),
            one(HANGUL_MONTH),
            one(FULL_WIDTH_COLON),
            one(FULL_WIDTH_COMMA),
        );
        for text in [
            // N-5: a month list, a price per unit, a date.
            format!("1{month}/2{month}/3{month}{}", word(&HAN_OF_REPORT)),
            format!("1{wol}/2{wol}/3{wol}"),
            format!("{}100{}/kg/{}", word(&HAN_PRICE), one(HAN_YUAN), one(HAN_DAY)),
            // A spaced slash list whose first segment is not wholly ASCII.
            format!("see /2{month}/3{month}"),
            format!("see /2{wol}/3{wol}/4{wol}"),
            format!("{}/09/25", word(&DEVANAGARI_2026)),
            "see /2026/09".to_string(),
            // N-6: an option letter, a full-width colon and a yen price.
            format!(
                "{}A{colon}{}99{comma}B{colon}{}199",
                word(&HAN_PLAN),
                one(YEN),
                one(YEN)
            ),
            format!("{}A{colon}{}500", word(&KATAKANA_PLAN), one(FULL_WIDTH_YEN)),
            format!("{}A{colon}{}9.99", word(&KATAKANA_PLAN), one(FULL_WIDTH_YEN)),
        ] {
            assert!(!text_signals(&text).path, "{text:?}");
        }
        // The same slashes and signs still make paths where the shape is
        // one: a glued ASCII path, a localized path under an ASCII root, a
        // yen-separated drive path, a drive path with a named first folder.
        for text in [
            format!("1{month}/var/log/app.log"),
            format!("{}/home/2{month}/3{month}", word(&HAN_PRICE)),
            format!("see /home/2{month}/3{month}"),
            format!("{}C{colon}{}Users{}x.txt", word(&HAN_PLAN), one(YEN), one(YEN)),
            format!("open C:{}data", one(YEN)),
        ] {
            assert!(text_signals(&text).path, "{text:?}");
        }
    }

    /// D-061 amendment 3 (R-CDP06 round 4): N-6b prices per unit or with a
    /// suffix after a yen sign are prose; N-7 a localized path glued to the
    /// words before it is a path when it has three segments or an
    /// extension; N-8 a list of Latin-script words is prose whatever their
    /// accents, in NFC and NFD. Each case was wrong at `1737880e`.
    #[test]
    fn round_four_prices_glued_localized_paths_and_latin_lists() {
        // Unicode data: Han "plan", "month", "year", "from", "yuan", "open",
        // "Zhang San", "report", "document"; Katakana "plan" and "file"; Han
        // "Tanaka" and "papers"; the full-width colon; the yen and
        // full-width yen signs; Latin letters with acute accents (u, e, i,
        // a) and the combining acute accent.
        const HAN_PLAN: [u32; 2] = [0x5957, 0x9910];
        const HAN_MONTH: u32 = 0x6708;
        const HAN_YEAR: u32 = 0x5E74;
        const HAN_FROM: u32 = 0x8D77;
        const HAN_YUAN: u32 = 0x5143;
        const HAN_OPEN: [u32; 2] = [0x6253, 0x5F00];
        const HAN_ZHANG_SAN: [u32; 2] = [0x5F20, 0x4E09];
        const HAN_REPORT: [u32; 2] = [0x62A5, 0x544A];
        const HAN_DOCUMENT: [u32; 2] = [0x6587, 0x6863];
        const KATAKANA_PLAN: [u32; 3] = [0x30D7, 0x30E9, 0x30F3];
        const KATAKANA_FILE: [u32; 4] = [0x30D5, 0x30A1, 0x30A4, 0x30EB];
        const HAN_TANAKA: [u32; 2] = [0x7530, 0x4E2D];
        const HAN_PAPERS: [u32; 2] = [0x66F8, 0x985E];
        const FULL_WIDTH_COLON: u32 = 0xFF1A;
        const YEN: u32 = 0x00A5;
        const FULL_WIDTH_YEN: u32 = 0xFFE5;
        const U_ACUTE: u32 = 0x00FA;
        const E_ACUTE: u32 = 0x00E9;
        const I_ACUTE: u32 = 0x00ED;
        const A_ACUTE: u32 = 0x00E1;
        const COMBINING_ACUTE: u32 = 0x0301;
        let word = |points: &[u32]| {
            points
                .iter()
                .filter_map(|point| char::from_u32(*point))
                .collect::<String>()
        };
        let one = |point: u32| char::from_u32(point).map(String::from).unwrap_or_default();
        let (colon, yen, full_yen) = (one(FULL_WIDTH_COLON), one(YEN), one(FULL_WIDTH_YEN));
        let (open, zhang, report) = (word(&HAN_OPEN), word(&HAN_ZHANG_SAN), word(&HAN_REPORT));
        for text in [
            // N-6b: per unit, per year, "from", with a currency suffix.
            format!("{}A{colon}{yen}99/{}", word(&HAN_PLAN), one(HAN_MONTH)),
            format!("{}A{colon}{full_yen}500/{}", word(&KATAKANA_PLAN), one(HAN_YEAR)),
            format!("{}A{colon}{yen}99{}", word(&HAN_PLAN), one(HAN_FROM)),
            format!("A{colon}{yen}99{}", one(HAN_YUAN)),
            // N-7 residual: a glued two-segment localized path.
            format!("{open}/home/{zhang}"),
            // N-7: after a glued slash the first segment is wholly ASCII, so
            // a mixed-script name glued to the words is not a path.
            format!("{open}/notes{report}.md"),
            // N-8: Latin-script word lists, NFC and NFD.
            format!("Per{}/Chile/Bolivia", one(U_ACUTE)),
            format!("Caf{}/Bar/Restaurant", one(E_ACUTE)),
            format!("Cafe{}/Bar/Restaurant", one(COMBINING_ACUTE)),
            format!("s{}/no/quiz{}", one(I_ACUTE), one(A_ACUTE)),
            format!("{0}t{0}/hiver/printemps", one(E_ACUTE)),
        ] {
            assert!(!text_signals(&text).path, "{text:?}");
        }
        for text in [
            // N-6b: a yen-separated drive path still counts.
            format!("open C:{yen}Users{yen}data"),
            format!("open C:{yen}data"),
            // N-7: glued localized paths with an extension or three segments.
            format!("{open}/home/{zhang}/{report}.pdf"),
            format!(
                "{}/Users/{}/{}.txt",
                word(&KATAKANA_FILE),
                word(&HAN_TANAKA),
                word(&HAN_PAPERS)
            ),
            format!("{open}/home/{zhang}/{}", word(&HAN_DOCUMENT)),
        ] {
            assert!(text_signals(&text).path, "{text:?}");
        }
    }

    /// D-061 amendment 4 (R-CDP06 round 5): only scripts written without
    /// spaces make a slash glued, so a word of any other script before a
    /// slash list is prose, as an English word is; and a glued path ends
    /// where its ASCII name turns into the glued words after it.
    #[test]
    fn spaced_scripts_are_words_and_glued_paths_end_at_the_words() {
        // Unicode data: Cyrillic "Russian" and "open", Greek "Greece",
        // Devanagari "Hindi", Arabic "Arabic", Georgian "Georgian", Hangul
        // "Korean", the IPA open e, Han "open" and "look", Katakana "file",
        // Hiragana "wo" and "aku" with Han "open", a Khmer and a Lao word.
        const CYR_RUSSIAN: [u32; 7] = [0x0440, 0x0443, 0x0441, 0x0441, 0x043A, 0x0438, 0x0439];
        const CYR_OPEN: [u32; 7] = [0x043E, 0x0442, 0x043A, 0x0440, 0x044B, 0x0442, 0x044C];
        const GREEK_GREECE: [u32; 6] = [0x0395, 0x03BB, 0x03BB, 0x03AC, 0x03B4, 0x03B1];
        const DEVANAGARI_HINDI: [u32; 5] = [0x0939, 0x093F, 0x0902, 0x0926, 0x0940];
        const ARABIC_ARABIC: [u32; 4] = [0x0639, 0x0631, 0x0628, 0x064A];
        const GEORGIAN: [u32; 7] = [0x10E5, 0x10D0, 0x10E0, 0x10D7, 0x10E3, 0x10DA, 0x10D8];
        const HANGUL_KOREAN: [u32; 3] = [0xD55C, 0xAD6D, 0xC5B4];
        const IPA_OPEN_E: u32 = 0x025B;
        const HAN_OPEN: [u32; 2] = [0x6253, 0x5F00];
        const HAN_LOOK: [u32; 2] = [0x770B, 0x770B];
        const KATAKANA_FILE: [u32; 4] = [0x30D5, 0x30A1, 0x30A4, 0x30EB];
        const KANA_OPEN: [u32; 3] = [0x3092, 0x958B, 0x304F];
        const KHMER_WORD: [u32; 3] = [0x1794, 0x17BE, 0x1780];
        const LAO_WORD: [u32; 2] = [0x0E9A, 0x0EB2];
        let word = |points: &[u32]| {
            points
                .iter()
                .filter_map(|point| char::from_u32(*point))
                .collect::<String>()
        };
        for text in [
            format!("{}/English/Deutsch", word(&CYR_RUSSIAN)),
            format!("{}/Italy/Spain", word(&GREEK_GREECE)),
            format!("{}/English/French", word(&DEVANAGARI_HINDI)),
            format!("{}/English/French", word(&ARABIC_ARABIC)),
            format!("{}/English/French", word(&GEORGIAN)),
            format!("{}/English/French", word(&HANGUL_KOREAN)),
            format!("{}/etc/hosts", word(&CYR_OPEN)),
            format!("s{}/no/yes", word(&[IPA_OPEN_E])),
        ] {
            assert!(!text_signals(&text).path, "{text:?}");
        }
        for text in [
            // A word of a spaced script, with its space, before a path.
            format!("{} /etc/hosts", word(&CYR_OPEN)),
            // Glued paths followed by glued words.
            format!("{}/etc/hosts{}", word(&HAN_OPEN), word(&HAN_LOOK)),
            format!("{}/Users/data.txt{}", word(&KATAKANA_FILE), word(&KANA_OPEN)),
            // Glued paths after Khmer and Lao words.
            format!("{}/var/log/app.log", word(&KHMER_WORD)),
            format!("{}/var/log/app.log", word(&LAO_WORD)),
        ] {
            assert!(text_signals(&text).path, "{text:?}");
        }
    }

    #[test]
    fn missing_or_mistyped_fields_read_as_neutral() {
        let state = serde_json::json!({"candidates": [{"id": 3, "failed_before": "yes"}], "retry_count": "one"});
        let features = extract(&state);
        assert_eq!(features.retry_count, 0);
        assert_eq!(features.candidates.len(), 1);
        assert_eq!(features.candidates[0].id, "");
        assert_eq!(features.candidates[0].action, None);
        assert!(features.candidates[0].available);
        assert!(!features.candidates[0].failed_before);
        assert!(!features.has_history);
        assert_eq!(extract(&serde_json::json!({})).candidates.len(), 0);
    }
}
