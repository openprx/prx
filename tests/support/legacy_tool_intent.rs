//! Historical keyword-routing baseline, compiled only in library tests.
//! This is not the decision-plane runtime or a CDP acceptance oracle.
//! Retained until positive coverage is migrated to scoped catalog discovery.
//!
//! Intent-based tool filtering for intelligent context management.
//!
//! Uses lightweight keyword matching (<1ms) to classify user messages into
//! semantic categories, then filters the tool registry so only relevant tools
//! are sent to the LLM — reducing context noise and improving response quality.
//!
//! The keyword table is English-only by project policy. Narrowing therefore
//! only ever happens on evidence: a message that activates no category at all
//! (a non-English request, or an English one that names no capability) is
//! *unrouted* and the router refuses to decide, returning
//! [`RoutingOutcome::Unrouted`] instead of collapsing to the [`ToolTier::Core`]
//! floor. Trimming a catalog on the strength of a keyword table that cannot
//! read the request would silently amputate capabilities the user asked for,
//! which is a far worse failure than paying for a few extra tool schemas.
//!
//! What an unrouted turn publishes is the *caller's* call, expressed as an
//! [`UnroutedToolPolicy`]. A stateless entry point publishes the whole allowed
//! registry; a session that already carries a cumulative exposure
//! ([`SessionToolExposure`]) keeps exactly what it had, because a turn that
//! names nothing new must add nothing. Folding both into the router made a
//! single greeting absorb the full catalog into a chat session's union and hold
//! it there for the rest of the session.
//!
//! "Names nothing new" is itself a judgement the keyword table cannot make, so
//! a session makes it structurally instead: a substantive unrouted turn — one
//! that points at a concrete object, or carries the weight of a real request in
//! any script — gets one *discovery* turn at the whole allowed registry
//! (`unrouted_turn_requests_discovery`). That surface is published for the
//! turn and never absorbed, so a session can reach a capability its wording
//! never named in English without one unreadable line pinning it at the full
//! catalog. Greetings, closings and one-word follow-ups keep publishing the
//! session set unchanged.

use std::collections::HashSet;
use std::sync::LazyLock;

use crate::tools::capability::search::character_is_unspaced_script;
use crate::tools::traits::{Tool, ToolCategory, ToolTier};

/// Keyword-to-category mapper for user intent classification.
struct IntentClassifier {
    entries: Vec<(String, ToolCategory)>,
}

impl IntentClassifier {
    fn new() -> Self {
        let entries: Vec<(&str, ToolCategory)> = vec![
            // WebBrowsing
            ("search", ToolCategory::WebBrowsing),
            ("google", ToolCategory::WebBrowsing),
            ("webpage", ToolCategory::WebBrowsing),
            ("url", ToolCategory::WebBrowsing),
            ("fetch", ToolCategory::WebBrowsing),
            ("browse", ToolCategory::WebBrowsing),
            ("website", ToolCategory::WebBrowsing),
            ("http", ToolCategory::WebBrowsing),
            // Scheduling
            ("cron", ToolCategory::Scheduling),
            ("schedule", ToolCategory::Scheduling),
            ("recurring", ToolCategory::Scheduling),
            ("timer", ToolCategory::Scheduling),
            ("heartbeat", ToolCategory::Scheduling),
            // Communication
            ("send", ToolCategory::Communication),
            ("push", ToolCategory::Communication),
            ("notify", ToolCategory::Communication),
            ("message", ToolCategory::Communication),
            ("voice", ToolCategory::Communication),
            ("audio", ToolCategory::Communication),
            ("telegram", ToolCategory::Communication),
            ("discord", ToolCategory::Communication),
            ("slack", ToolCategory::Communication),
            // Memory
            ("remember", ToolCategory::Memory),
            ("forget", ToolCategory::Memory),
            ("memory", ToolCategory::Memory),
            ("memorize", ToolCategory::Memory),
            ("store", ToolCategory::Memory),
            ("save", ToolCategory::Memory),
            // DevOps
            ("git", ToolCategory::DevOps),
            ("commit", ToolCategory::DevOps),
            ("deploy", ToolCategory::DevOps),
            ("api", ToolCategory::DevOps),
            ("branch", ToolCategory::DevOps),
            ("merge", ToolCategory::DevOps),
            ("repository", ToolCategory::DevOps),
            // Media
            ("image", ToolCategory::Media),
            ("screenshot", ToolCategory::Media),
            ("photo", ToolCategory::Media),
            ("picture", ToolCategory::Media),
            // System / Automation
            ("session", ToolCategory::System),
            ("config", ToolCategory::System),
            ("proxy", ToolCategory::System),
            ("node", ToolCategory::System),
            ("gateway", ToolCategory::System),
            ("skill", ToolCategory::System),
            ("mcp", ToolCategory::Automation),
            ("plugin", ToolCategory::Automation),
            ("wasm", ToolCategory::Automation),
            ("composio", ToolCategory::Automation),
            ("delegate", ToolCategory::Automation),
            ("agent", ToolCategory::Automation),
            ("spawn", ToolCategory::Automation),
            // FileSystem (extra activation)
            ("file", ToolCategory::FileSystem),
            ("directory", ToolCategory::FileSystem),
            ("folder", ToolCategory::FileSystem),
        ];
        Self {
            entries: entries.into_iter().map(|(k, v)| (k.to_lowercase(), v)).collect(),
        }
    }

    fn classify(&self, message: &str) -> HashSet<ToolCategory> {
        let lower = message.to_lowercase();
        let mut cats = HashSet::new();
        for (pattern, category) in &self.entries {
            if lower
                .match_indices(pattern.as_str())
                .any(|(index, _)| !keyword_is_in_negative_instruction(&lower, index))
            {
                cats.insert(*category);
            }
        }
        cats
    }
}

/// Return whether a keyword occurrence belongs to an explicit negative tool
/// instruction such as "do not use browser or MCP".
///
/// Capability routing happens before the model sees the tool catalog, so a
/// plain substring match on a forbidden capability is particularly harmful:
/// the request "do not use MCP" used to cold-start every configured MCP server.
/// Keep this deliberately narrow and clause-local. Positive mentions in a
/// later sentence or after an explicit contrast ("but use MCP")
/// still activate the category.
fn keyword_is_in_negative_instruction(message: &str, keyword_index: usize) -> bool {
    const CLAUSE_BOUNDARIES: &[char] = &['.', '!', '?', ';', '\n', '\r'];
    const NEGATIVE_MARKERS: &[&str] = &[
        "do not use",
        "don't use",
        "dont use",
        "must not use",
        "without using",
        "without",
        "avoid using",
        "avoid",
        "no ",
        "never use",
        "do not call",
        "don't call",
        "dont call",
        "must not call",
        "never call",
        "no need to use",
        "no need to call",
        "refrain from using",
        "refrain from calling",
    ];
    const POSITIVE_PIVOTS: &[&str] = &[" but ", " instead ", " however ", " rather ", " except "];

    let before = &message[..keyword_index];
    let clause_start = before
        .char_indices()
        .rev()
        .find_map(|(index, ch)| CLAUSE_BOUNDARIES.contains(&ch).then_some(index + ch.len_utf8()))
        .unwrap_or(0);
    let prefix = &message[clause_start..keyword_index];

    let Some(negative_index) = NEGATIVE_MARKERS.iter().filter_map(|marker| prefix.rfind(marker)).max() else {
        return false;
    };
    !POSITIVE_PIVOTS
        .iter()
        .filter_map(|pivot| prefix.rfind(pivot))
        .any(|pivot_index| pivot_index > negative_index)
}

static CLASSIFIER: LazyLock<IntentClassifier> = LazyLock::new(IntentClassifier::new);

/// The capability router's verdict for a single turn.
///
/// Routing is deliberately *not* allowed to answer "everything" on its own: the
/// right surface for a turn the keyword table cannot read depends on whether
/// the caller remembers what it published before, which routing cannot know.
pub enum RoutingOutcome<'a> {
    /// The keyword table read the request. These are the tools it selected,
    /// with `always_include` / `always_exclude` already applied.
    Routed(Vec<&'a dyn Tool>),
    /// The classifier activated no category at all — a request in a language
    /// the table does not cover, or an English one that names no capability.
    /// No tool set is implied; the caller decides, see [`UnroutedToolPolicy`].
    Unrouted,
}

impl std::fmt::Debug for RoutingOutcome<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Routed(selected) => f
                .debug_tuple("Routed")
                .field(&selected.iter().map(|tool| tool.name()).collect::<Vec<_>>())
                .finish(),
            Self::Unrouted => f.write_str("Unrouted"),
        }
    }
}

impl<'a> RoutingOutcome<'a> {
    /// The routed set, or `None` when the router had nothing to say.
    #[must_use]
    pub fn routed(self) -> Option<Vec<&'a dyn Tool>> {
        match self {
            Self::Routed(selected) => Some(selected),
            Self::Unrouted => None,
        }
    }
}

/// What an entry point publishes on a turn the keyword table cannot read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnroutedToolPolicy {
    /// Publish everything operator policy allows. The default, and the only
    /// safe answer for an entry point that keeps no memory across turns
    /// (channels, gateway, console, delegated and session-worker turns):
    /// narrowing on a table that cannot read the request would amputate
    /// capabilities the user actually asked for.
    #[default]
    PublishEverything,
    /// Publish the set the caller already pinned through `always_include`. For
    /// a session that carries its own cumulative exposure: an unrouted turn
    /// names nothing new, so it must add nothing. Widening to the whole
    /// registry here is what let a single greeting pull the entire catalog into
    /// a chat session's union and keep it there for the rest of the session.
    /// A session that decides the turn *is* a request answers
    /// [`Self::PublishEverything`] for that turn instead, without absorbing it.
    KeepPinnedExposure,
}

/// Weight of one word of a script that separates words with spaces.
const SPACED_WORD_WEIGHT: usize = 5;

/// Weight of one character of a script that writes words without spaces
/// between them. Those scripts pack roughly one and a half characters into a
/// word, so a character is worth a little over half of one.
const UNSPACED_CHARACTER_WEIGHT: usize = 3;

/// Eight spaced words, or about fourteen unspaced characters: the point where
/// a line stops reading like a pleasantry and starts reading like a request.
const DISCOVERY_REQUEST_WEIGHT: usize = 8 * SPACED_WORD_WEIGHT;

/// Whether a turn the keyword table could not read is asking for something new.
///
/// An unrouted turn is one of two very different things: a request the
/// English-only table cannot read (another language, or a paraphrase that names
/// no capability), or an ordinary conversational line that asks for nothing at
/// all. Treating both as "nothing new" left an established session unable to
/// ever reach a capability it had not already named in English, which is the
/// defect this closes; treating both as a new request would let one greeting
/// pull the whole catalog back in.
///
/// The evidence is structural rather than lexical, so it carries across
/// languages and cannot be extended into a multilingual keyword list:
///
/// - a **concrete object** the turn points at — a URL, a filesystem path, a
///   file name, a command flag, or a backtick code span — which makes even a
///   short line a real request;
/// - the **request weight** of the text: a clause long enough to carry a verb
///   and an object reads as a request, while a greeting, a closing or a
///   one-word acknowledgement does not. Weight counts words for scripts that
///   separate them and characters for scripts that do not, so the threshold
///   means roughly the same thing in both.
///
/// Both halves are deliberately coarse, and the asymmetry is deliberate too:
/// answering "yes" too often costs one turn of wider tool schemas, while
/// answering "no" costs the user a capability they explicitly asked for.
fn unrouted_turn_requests_discovery(message: &str) -> bool {
    mentions_concrete_object(message) || request_weight(message) >= DISCOVERY_REQUEST_WEIGHT
}

/// Size of the request in language-independent units: one word of a spaced
/// script, or one character of an unspaced one.
fn request_weight(message: &str) -> usize {
    let mut weight = 0;
    let mut inside_word = false;
    for character in message.chars() {
        if character_is_unspaced_script(character) {
            if inside_word {
                weight += SPACED_WORD_WEIGHT;
                inside_word = false;
            }
            weight += UNSPACED_CHARACTER_WEIGHT;
        } else if character.is_alphanumeric() {
            inside_word = true;
        } else if inside_word {
            weight += SPACED_WORD_WEIGHT;
            inside_word = false;
        }
    }
    if inside_word {
        weight += SPACED_WORD_WEIGHT;
    }
    weight
}

/// Whether the message points at something concrete enough to act on. A short
/// line is still a request when it names a URL, a path, a file, a command flag
/// or a quoted command.
fn mentions_concrete_object(message: &str) -> bool {
    message.contains("://")
        || message.matches('`').count() >= 2
        || message.split_whitespace().any(token_names_a_concrete_object)
}

/// Punctuation that is part of a path, file name or flag rather than prose, so
/// it must survive the trim that strips ordinary sentence punctuation.
const OBJECT_PUNCTUATION: &[char] = &['/', '\\', '.', '-', '_', '~'];

fn token_names_a_concrete_object(token: &str) -> bool {
    let trimmed =
        token.trim_matches(|character: char| !character.is_alphanumeric() && !OBJECT_PUNCTUATION.contains(&character));
    if trimmed.chars().count() < 2 || !trimmed.chars().any(char::is_alphanumeric) {
        return false;
    }

    // A long command flag: `--all-features`.
    if trimmed
        .strip_prefix("--")
        .is_some_and(|rest| rest.starts_with(char::is_alphanumeric))
    {
        return true;
    }

    // A path: rooted, relative, nested, or a Windows-style one. A bare
    // `and/or` is prose, not a path, so one interior slash is not enough.
    let looks_like_a_path = trimmed.rsplit_once('/').map_or_else(
        || trimmed.split('\\').filter(|segment| !segment.is_empty()).count() >= 2,
        |(head, tail)| trimmed.starts_with(['/', '.', '~']) || head.contains('/') || tail.contains('.'),
    );
    if looks_like_a_path {
        return true;
    }

    // A file or host name: a stem, a dot, and a short alphabetic suffix.
    trimmed.rsplit_once('.').is_some_and(|(stem, suffix)| {
        !stem.is_empty()
            && (2..=8).contains(&suffix.chars().count())
            && suffix.chars().all(|character| character.is_ascii_alphabetic())
    })
}

/// Every tool operator policy allows, in registry order, ignoring intent.
#[must_use]
pub fn full_tool_surface<'a>(all_tools: &'a [Box<dyn Tool>], always_exclude: &[String]) -> Vec<&'a dyn Tool> {
    all_tools
        .iter()
        .filter(|tool| !always_exclude.iter().any(|excluded| excluded == tool.name()))
        .map(|tool| tool.as_ref())
        .collect()
}

/// The tools a caller pinned by name through `always_include`, minus anything
/// `always_exclude` forbids — operator policy still outranks the pin.
#[must_use]
pub fn pinned_tool_surface<'a>(
    all_tools: &'a [Box<dyn Tool>],
    always_include: &[String],
    always_exclude: &[String],
) -> Vec<&'a dyn Tool> {
    all_tools
        .iter()
        .filter(|tool| {
            let name = tool.name();
            !always_exclude.iter().any(|excluded| excluded == name)
                && always_include.iter().any(|included| included == name)
        })
        .map(|tool| tool.as_ref())
        .collect()
}

/// Route one turn and resolve an unrouted verdict with `unrouted`.
///
/// This is what every entry point calls; [`select_tools_for_intent`] is the
/// undecided half, kept separate so a caller can see the verdict itself.
#[must_use]
pub fn resolve_tools_for_intent<'a>(
    all_tools: &'a [Box<dyn Tool>],
    user_message: &str,
    always_include: &[String],
    always_exclude: &[String],
    unrouted: UnroutedToolPolicy,
) -> Vec<&'a dyn Tool> {
    match select_tools_for_intent(all_tools, user_message, always_include, always_exclude) {
        RoutingOutcome::Routed(selected) => selected,
        RoutingOutcome::Unrouted => match unrouted {
            UnroutedToolPolicy::PublishEverything => full_tool_surface(all_tools, always_exclude),
            UnroutedToolPolicy::KeepPinnedExposure => {
                let pinned = pinned_tool_surface(all_tools, always_include, always_exclude);
                if pinned.is_empty() {
                    // Nothing pinned yet — the caller has no earlier decision to
                    // keep, so it lands on the same floor as everyone else.
                    full_tool_surface(all_tools, always_exclude)
                } else {
                    pinned
                }
            }
        },
    }
}

/// Filter tools based on user intent. Core tools always included.
/// Standard tools included if any of their categories match (or if no categories are set).
/// Extended tools only included on explicit category match.
///
/// When the classifier activates **no** category the turn is *unrouted* and
/// this returns [`RoutingOutcome::Unrouted`] rather than a set: the keyword
/// table produced no evidence about this request, and the fallback is the
/// caller's decision. Use [`resolve_tools_for_intent`] to apply one.
///
/// The `always_include` / `always_exclude` lists (tool names) are applied after
/// tier-based filtering to allow user overrides.
#[must_use]
pub fn select_tools_for_intent<'a>(
    all_tools: &'a [Box<dyn Tool>],
    user_message: &str,
    always_include: &[String],
    always_exclude: &[String],
) -> RoutingOutcome<'a> {
    let activated = CLASSIFIER.classify(user_message);
    if activated.is_empty() {
        tracing::debug!("capability routing found no evidence in this turn; deferring to the caller");
        return RoutingOutcome::Unrouted;
    }

    let selected = all_tools
        .iter()
        .filter(|tool| {
            let name = tool.name();

            // always_exclude takes highest priority
            if always_exclude.iter().any(|n| n == name) {
                return false;
            }

            // always_include overrides tier logic
            if always_include.iter().any(|n| n == name) {
                return true;
            }

            match tool.tier() {
                ToolTier::Core => true,
                ToolTier::Standard => {
                    let cats = tool.categories();
                    // Tools without categories in Standard tier are always included
                    cats.is_empty() || cats.iter().any(|c| activated.contains(c))
                }
                ToolTier::Extended => {
                    let cats = tool.categories();
                    cats.iter().any(|c| activated.contains(c))
                }
            }
        })
        .map(|t| t.as_ref())
        .collect::<Vec<_>>();

    if tracing::enabled!(tracing::Level::DEBUG) {
        let selected_names = selected.iter().map(|tool| tool.name()).collect::<HashSet<_>>();
        let mut active = selected_names.iter().copied().collect::<Vec<_>>();
        let mut rejected = all_tools
            .iter()
            .map(|tool| tool.name())
            .filter(|name| !selected_names.contains(name))
            .collect::<Vec<_>>();
        active.sort_unstable();
        rejected.sort_unstable();
        tracing::debug!(?active, ?rejected, "capability routing decision");
    }

    RoutingOutcome::Routed(selected)
}

#[cfg(test)]
use crate::tools::capability::policy::{core_dependency_is_available, model_allows_tool_name};

/// Intersect a selected capability surface with its exact-model allowlist.
///
/// Keeping this as a second-stage intersection preserves the global
/// `always_exclude` boundary and prevents a model allowlist from enabling a
/// capability that selection or operator policy already removed.
pub fn apply_model_tool_allowlist<'a, S: std::hash::BuildHasher>(
    selected: Vec<&'a dyn Tool>,
    model: &str,
    model_allowlists: &std::collections::HashMap<String, Vec<String>, S>,
) -> Vec<&'a dyn Tool> {
    let Some(allowlist) = model_allowlists.get(model) else {
        return selected;
    };

    let filtered = selected
        .into_iter()
        .filter(|tool| allowlist.iter().any(|name| tool.supports_name(name)))
        .collect::<Vec<_>>();

    tracing::debug!(model, allowed = ?allowlist, filtered = filtered.len(), "model tool allowlist applied");
    filtered
}

/// Session-scoped, monotonically growing tool exposure.
///
/// Intent routing is a per-turn decision, but on a provider that caches request
/// prefixes (OpenAI, Kimi, Anthropic) the `tools` array sits *inside* the
/// cacheable prefix together with the system prompt. Re-deciding the exposed
/// set on every turn therefore invalidates the prefix — and with it the whole
/// conversation's prefill — every time the user rephrases. A long chat session
/// pays far more for those misses than it saves on tool schemas.
///
/// This keeps one union per chat session: each turn exposes the union of every
/// set routed so far, so the prefix changes only the first time a new intent
/// appears and is byte-identical on every turn that names nothing new.
/// `reset` is the session boundary (`/new`, `/clear`, a new session identity).
#[derive(Debug, Clone, Default)]
pub struct SessionToolExposure {
    names: std::sync::Arc<parking_lot::Mutex<std::collections::BTreeSet<String>>>,
}

impl SessionToolExposure {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop everything absorbed so far. Called when the session identity
    /// changes; the next turn starts from that turn's routing decision alone.
    pub fn reset(&self) {
        self.names.lock().clear();
    }

    /// Names exposed so far, in canonical order.
    #[must_use]
    pub fn snapshot(&self) -> Vec<String> {
        self.names.lock().iter().cloned().collect()
    }

    /// Absorb this turn's routing decision and return the surface that exposes
    /// the session's cumulative set.
    ///
    /// The union is handed back through the existing `always_include` knob, so
    /// the tool loop keeps running exactly one selector. `always_exclude` still
    /// outranks `always_include` inside [`select_tools_for_intent`], so a name
    /// operator policy forbids can never be resurrected by stickiness.
    ///
    /// An **unrouted** turn is absorbed only when the session has nothing yet:
    /// the very first turn of a session that names no capability still gets the
    /// whole registry, because there is no earlier decision to fall back on.
    /// Once the session holds a set, an unrouted turn adds nothing to it — it
    /// re-publishes what is already there. Absorbing the fallback every time
    /// made one unreadable turn (a greeting, a non-English request) swallow the
    /// entire registry and pin the session at full width from then on.
    ///
    /// An unrouted turn that is nonetheless a *request*
    /// (`unrouted_turn_requests_discovery`) is the third case: the returned
    /// surface publishes everything operator policy allows for this one turn,
    /// while the union stays exactly where it was. Without it an established
    /// session could never reach a capability it had not already named in
    /// English — a request in another language, or a paraphrase the table does
    /// not recognise, activated nothing and therefore changed nothing.
    #[must_use]
    pub fn sticky_surface(
        &self,
        base: &crate::config::ToolTieringConfig,
        all_tools: &[Box<dyn Tool>],
        routing_input: &str,
    ) -> SessionToolSurface {
        let outcome = select_tools_for_intent(all_tools, routing_input, &base.always_include, &base.always_exclude);
        let mut names = self.names.lock();
        let mut unrouted = UnroutedToolPolicy::KeepPinnedExposure;
        let absorbed = match outcome {
            RoutingOutcome::Routed(routed) => Some(routed),
            RoutingOutcome::Unrouted if names.is_empty() => Some(full_tool_surface(all_tools, &base.always_exclude)),
            RoutingOutcome::Unrouted => {
                if unrouted_turn_requests_discovery(routing_input) {
                    // Discovery turn: publish everything operator policy allows
                    // for this turn only, and leave the union untouched. The
                    // session can reach a capability its wording never named,
                    // without one unreadable request pinning it at the whole
                    // catalog for the rest of its life.
                    unrouted = UnroutedToolPolicy::PublishEverything;
                }
                None
            }
        };
        for tool in absorbed.into_iter().flatten() {
            if !names.contains(tool.name()) {
                names.insert(tool.name().to_string());
            }
        }
        let mut tiering = base.clone();
        for name in names.iter() {
            if !tiering.always_include.iter().any(|existing| existing == name) {
                tiering.always_include.push(name.clone());
            }
        }
        SessionToolSurface { tiering, unrouted }
    }
}

/// The tiering policy a session publishes for one turn, together with the
/// fallback the shared tool loop must apply if it re-routes the same turn and
/// finds nothing. Both halves travel together so the loop cannot re-widen a
/// surface the session deliberately held still.
#[derive(Debug, Clone)]
pub struct SessionToolSurface {
    /// Tiering config whose `always_include` carries the session's union.
    pub tiering: crate::config::ToolTieringConfig,
    /// What to publish when this turn routes to nothing.
    pub unrouted: UnroutedToolPolicy,
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        clippy::unwrap_used,
        clippy::print_stdout,
        clippy::print_stderr,
        clippy::disallowed_types,
        clippy::disallowed_methods
    )]
    use super::*;
    use crate::memory::{Memory, SqliteMemory};
    use crate::security::SecurityPolicy;
    use crate::tools::ChatProfileUpdateTool;
    use std::sync::Arc;
    use tempfile::TempDir;

    #[test]
    fn a_message_that_names_no_capability_activates_nothing() {
        assert!(
            CLASSIFIER.classify("hi there, how are you doing today?").is_empty(),
            "a greeting names no capability"
        );
        assert!(
            CLASSIFIER.classify("hola, \u{bf}c\u{f3}mo est\u{e1}s hoy?").is_empty(),
            "the keyword table is English-only, so a non-English request activates nothing"
        );
    }

    #[test]
    fn classify_web_search() {
        let cats = CLASSIFIER.classify("search for the Rust book");
        assert!(cats.contains(&ToolCategory::WebBrowsing));
    }

    #[test]
    fn classify_scheduling() {
        let cats = CLASSIFIER.classify("set up a cron job for this");
        assert!(cats.contains(&ToolCategory::Scheduling));
    }

    #[test]
    fn classify_memory() {
        let cats = CLASSIFIER.classify("remember that I prefer Rust");
        assert!(cats.contains(&ToolCategory::Memory));
        assert!(
            CLASSIFIER
                .classify("store this for later")
                .contains(&ToolCategory::Memory),
            "'store' is the English replacement for the removed non-English write verbs"
        );
    }

    #[test]
    fn classify_devops() {
        let cats = CLASSIFIER.classify("commit these changes to git");
        assert!(cats.contains(&ToolCategory::DevOps));
    }

    #[test]
    fn classify_mixed() {
        let cats = CLASSIFIER.classify("search the docs and then remember the key facts");
        assert!(cats.contains(&ToolCategory::WebBrowsing));
        assert!(cats.contains(&ToolCategory::Memory));
    }

    #[test]
    fn media_and_communication_keywords_have_english_coverage() {
        assert!(
            CLASSIFIER
                .classify("take a screenshot of it")
                .contains(&ToolCategory::Media),
            "screenshot must classify as Media"
        );
        assert!(
            CLASSIFIER
                .classify("send a voice note to the group")
                .contains(&ToolCategory::Communication),
            "voice must classify as Communication"
        );
    }

    #[test]
    fn negative_capability_instructions_do_not_activate_categories() {
        let cats = CLASSIFIER.classify("Use the PDF skill. Do not use browser, MCP, HTTP, plugins, or WASM tools.");
        assert!(cats.contains(&ToolCategory::System));
        assert!(!cats.contains(&ToolCategory::WebBrowsing));
        assert!(!cats.contains(&ToolCategory::Automation));
    }

    /// The negative-instruction guard clamps on byte offsets inside the
    /// lowercased message, so it also has to hold when the surrounding prose is
    /// multi-byte: those markers are the only thing standing between
    /// "do not touch MCP" and a cold MCP start.
    #[test]
    fn negative_capability_instructions_survive_multibyte_surroundings() {
        let cats = CLASSIFIER.classify("Ol\u{e1}, read the PDF skill; do not call MCP, plugin \u{f6}r WASM.");
        assert!(cats.contains(&ToolCategory::System));
        assert!(!cats.contains(&ToolCategory::Automation));
    }

    #[test]
    fn positive_capability_after_contrast_still_activates_category() {
        let cats = CLASSIFIER.classify("Do not use HTTP, but use MCP to operate the browser");
        assert!(cats.contains(&ToolCategory::Automation));
    }

    #[test]
    fn chat_profile_update_is_offered_without_memory_keywords() {
        let tmp = TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(ChatProfileUpdateTool::new(
            memory,
            Arc::new(SecurityPolicy::default()),
        ))];

        let selected = select_tools_for_intent(&tools, "search the release notes", &[], &[])
            .routed()
            .expect("a search request routes");
        let names: HashSet<&str> = selected.iter().map(|tool| tool.name()).collect();

        assert!(
            names.contains("chat_profile_update"),
            "chat_profile_update must be visible even when the message has no memory keywords"
        );
        assert_eq!(tools[0].tier(), ToolTier::Core);
    }

    #[test]
    fn model_allowlist_intersects_intent_selected_tools() {
        let tmp = TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(ChatProfileUpdateTool::new(
                Arc::clone(&memory),
                Arc::new(SecurityPolicy::default()),
            )),
            Box::new(crate::tools::ShellTool::new(
                Arc::new(crate::runtime::NativeRuntime::new()),
                std::path::PathBuf::from("."),
            )),
        ];
        // `run shell` names no category, so the stateless fallback is what a
        // gateway/console turn would publish here.
        let selected = resolve_tools_for_intent(&tools, "run shell", &[], &[], UnroutedToolPolicy::PublishEverything);
        let allowlists = std::collections::HashMap::from([("small-model".to_string(), vec!["shell".to_string()])]);

        let filtered = apply_model_tool_allowlist(selected, "small-model", &allowlists);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name(), "shell");

        let unconfigured =
            resolve_tools_for_intent(&tools, "run shell", &[], &[], UnroutedToolPolicy::PublishEverything);
        assert_eq!(
            apply_model_tool_allowlist(unconfigured, "other-model", &allowlists).len(),
            2
        );
        assert!(model_allows_tool_name("small-model", "shell", &allowlists));
        assert!(!model_allows_tool_name(
            "small-model",
            "chat_profile_update",
            &allowlists
        ));
        assert!(model_allows_tool_name(
            "other-model",
            "chat_profile_update",
            &allowlists
        ));
    }

    struct TieredProbe {
        name: &'static str,
        tier: ToolTier,
        categories: &'static [ToolCategory],
    }

    #[async_trait::async_trait]
    impl Tool for TieredProbe {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "routing probe"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        async fn execute(&self, _arguments: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
            Ok(crate::tools::ToolResult {
                success: true,
                output: String::new(),
                error: None,
            })
        }
        fn tier(&self) -> ToolTier {
            self.tier
        }
        fn categories(&self) -> &'static [ToolCategory] {
            self.categories
        }
    }

    fn probe_registry() -> Vec<Box<dyn Tool>> {
        vec![
            Box::new(TieredProbe {
                name: "probe_core",
                tier: ToolTier::Core,
                categories: &[],
            }) as Box<dyn Tool>,
            Box::new(TieredProbe {
                name: "probe_devops",
                tier: ToolTier::Standard,
                categories: &[ToolCategory::DevOps],
            }) as Box<dyn Tool>,
            Box::new(TieredProbe {
                name: "probe_web",
                tier: ToolTier::Extended,
                categories: &[ToolCategory::WebBrowsing],
            }) as Box<dyn Tool>,
        ]
    }

    /// The same registry with a **second** Extended-tier tool that operator
    /// policy does not exclude.
    ///
    /// Discovery is only observable when the discovery surface and the routed
    /// surface differ. With a single Extended tool, excluding it makes the two
    /// sets identical, and an exclusion assertion written against that registry
    /// cannot tell "discovery ran and correctly withheld the excluded tool"
    /// from "discovery never ran at all" — it passes either way. `probe_media`
    /// is the difference: it belongs to no category the routed turn names, so
    /// only a discovery turn can publish it.
    fn discovery_probe_registry() -> Vec<Box<dyn Tool>> {
        let mut tools = probe_registry();
        tools.push(Box::new(TieredProbe {
            name: "probe_media",
            tier: ToolTier::Extended,
            categories: &[ToolCategory::Media],
        }) as Box<dyn Tool>);
        tools
    }

    /// What a stateless entry point (channel, gateway, console, worker)
    /// publishes for this turn.
    fn selected_names(
        tools: &[Box<dyn Tool>],
        message: &str,
        tiering: &crate::config::ToolTieringConfig,
    ) -> Vec<String> {
        policy_names(tools, message, tiering, UnroutedToolPolicy::PublishEverything)
    }

    /// What a chat turn publishes: the dispatcher decides the session surface,
    /// then the shared tool loop re-routes the same text under that surface.
    /// Both halves must be exercised — the loop is where the old fallback
    /// re-widened a set the session had just held still.
    fn session_names(tools: &[Box<dyn Tool>], message: &str, surface: &SessionToolSurface) -> Vec<String> {
        policy_names(tools, message, &surface.tiering, surface.unrouted)
    }

    fn policy_names(
        tools: &[Box<dyn Tool>],
        message: &str,
        tiering: &crate::config::ToolTieringConfig,
        unrouted: UnroutedToolPolicy,
    ) -> Vec<String> {
        let mut names = resolve_tools_for_intent(
            tools,
            message,
            &tiering.always_include,
            &tiering.always_exclude,
            unrouted,
        )
        .into_iter()
        .map(|tool| tool.name().to_string())
        .collect::<Vec<_>>();
        names.sort();
        names
    }

    /// MUTATION GUARD: make `select_tools_for_intent` return
    /// `RoutingOutcome::Routed` for an empty category set, or make
    /// `resolve_tools_for_intent` ignore `PublishEverything`, and this goes red.
    #[test]
    fn an_unrouted_message_keeps_the_whole_registry() {
        let tools = probe_registry();
        let tiering = crate::config::ToolTieringConfig::default();
        assert_eq!(
            selected_names(&tools, "ay\u{fa}dame a revisar esto, por favor", &tiering),
            vec!["probe_core", "probe_devops", "probe_web"],
            "a request the English keyword table cannot read must not be trimmed"
        );
    }

    #[test]
    fn an_unrouted_message_still_obeys_always_exclude() {
        let tools = probe_registry();
        let tiering = crate::config::ToolTieringConfig {
            always_exclude: vec!["probe_web".to_string()],
            ..crate::config::ToolTieringConfig::default()
        };
        assert_eq!(
            selected_names(&tools, "hello there", &tiering),
            vec!["probe_core", "probe_devops"],
            "the fallback widens intent routing, never operator policy"
        );
    }

    #[test]
    fn a_routed_message_still_narrows_to_core_plus_its_category() {
        let tools = probe_registry();
        let tiering = crate::config::ToolTieringConfig::default();
        assert_eq!(
            selected_names(&tools, "commit this change", &tiering),
            vec!["probe_core", "probe_devops"],
            "naming a capability is evidence, so the rest of the catalog is dropped"
        );
    }

    /// MUTATION GUARD: make `sticky_surface` return `base.clone()` and the
    /// second assertion goes red.
    #[test]
    fn session_tool_exposure_only_ever_grows_until_reset() {
        let tools = probe_registry();
        let base = crate::config::ToolTieringConfig::default();
        let exposure = SessionToolExposure::new();

        let first = exposure.sticky_surface(&base, &tools, "commit this change");
        assert_eq!(
            session_names(&tools, "commit this change", &first),
            vec!["probe_core", "probe_devops"]
        );

        // A turn that names nothing this session has not already asked for must
        // publish the identical set, whatever the wording.
        let second = exposure.sticky_surface(&base, &tools, "please push the merge to the branch");
        assert_eq!(
            session_names(&tools, "please push the merge to the branch", &second),
            vec!["probe_core", "probe_devops"]
        );

        // A genuinely new intent widens the set exactly once, and it stays.
        let third = exposure.sticky_surface(&base, &tools, "search the changelog");
        assert_eq!(
            session_names(&tools, "search the changelog", &third),
            vec!["probe_core", "probe_devops", "probe_web"]
        );
        let fourth = exposure.sticky_surface(&base, &tools, "commit this change");
        assert_eq!(
            session_names(&tools, "commit this change", &fourth),
            vec!["probe_core", "probe_devops", "probe_web"],
            "a routed set may not shrink inside one session"
        );

        exposure.reset();
        assert!(exposure.snapshot().is_empty());
        let after_reset = exposure.sticky_surface(&base, &tools, "commit this change");
        assert_eq!(
            session_names(&tools, "commit this change", &after_reset),
            vec!["probe_core", "probe_devops"],
            "a session boundary starts the union over"
        );
    }

    /// A turn the keyword table cannot read is not evidence of a new capability,
    /// so a session that already holds a set re-publishes exactly that set. The
    /// old behaviour published the whole registry and the union absorbed it,
    /// which pinned every later turn of the session at full width.
    ///
    /// MUTATION GUARD: absorb the unrouted fallback unconditionally in
    /// `sticky_surface`, or return `PublishEverything` from it, and the third
    /// and fourth assertions go red.
    #[test]
    fn an_unrouted_turn_neither_widens_nor_shrinks_an_established_session() {
        let tools = probe_registry();
        let base = crate::config::ToolTieringConfig::default();
        let exposure = SessionToolExposure::new();
        // Spanish for "thanks, that is all" — an ordinary closing line that
        // activates no English category.
        let unreadable = "gracias, eso es todo";

        let routed = exposure.sticky_surface(&base, &tools, "commit this change");
        let established = session_names(&tools, "commit this change", &routed);
        assert_eq!(established, vec!["probe_core", "probe_devops"]);

        // Fixture self-check: the same text through a stateless entry point
        // really does fall back to the whole registry, so the assertion below
        // is about the session rule and not about the message being routable.
        assert_eq!(
            selected_names(&tools, unreadable, &base),
            vec!["probe_core", "probe_devops", "probe_web"],
            "the fixture message must be genuinely unrouted"
        );

        let quiet = exposure.sticky_surface(&base, &tools, unreadable);
        assert_eq!(
            session_names(&tools, unreadable, &quiet),
            established,
            "an unrouted turn must publish the session set unchanged"
        );
        assert_eq!(
            exposure.snapshot(),
            vec!["probe_core".to_string(), "probe_devops".to_string()],
            "an unrouted turn must not absorb the whole registry into the union"
        );

        // And the session is still able to grow afterwards.
        let later = exposure.sticky_surface(&base, &tools, "search the changelog");
        assert_eq!(
            session_names(&tools, "search the changelog", &later),
            vec!["probe_core", "probe_devops", "probe_web"]
        );
    }

    /// The protection R1 added for an unreadable *first* turn stays: a session
    /// with nothing absorbed yet has no earlier decision to keep, so it gets the
    /// whole registry and keeps it.
    ///
    /// MUTATION GUARD: drop the `names.is_empty()` branch in `sticky_surface`
    /// and the first assertion goes red.
    #[test]
    fn a_session_opening_on_an_unrouted_turn_still_gets_the_whole_registry() {
        let tools = probe_registry();
        let base = crate::config::ToolTieringConfig::default();
        let exposure = SessionToolExposure::new();
        let unreadable = "gracias, eso es todo";

        let opening = exposure.sticky_surface(&base, &tools, unreadable);
        assert_eq!(
            session_names(&tools, unreadable, &opening),
            vec!["probe_core", "probe_devops", "probe_web"],
            "a first turn with no earlier decision may not be trimmed"
        );

        let next = exposure.sticky_surface(&base, &tools, "commit this change");
        assert_eq!(
            session_names(&tools, "commit this change", &next),
            vec!["probe_core", "probe_devops", "probe_web"],
            "the union only ever grows"
        );
    }

    #[test]
    fn session_tool_exposure_never_resurrects_an_excluded_tool() {
        let tools = probe_registry();
        let base = crate::config::ToolTieringConfig {
            always_exclude: vec!["probe_web".to_string()],
            ..crate::config::ToolTieringConfig::default()
        };
        let exposure = SessionToolExposure::new();
        let sticky = exposure.sticky_surface(&base, &tools, "search the changelog");
        assert_eq!(
            session_names(&tools, "search the changelog", &sticky),
            vec!["probe_core"],
            "the excluded web probe is the only tool that category would have added"
        );
        assert!(!exposure.snapshot().iter().any(|name| name == "probe_web"));
    }

    #[test]
    fn core_dependency_availability_honors_exclusion_and_model_allowlist() {
        let mut config = crate::config::ToolTieringConfig::default();
        assert!(core_dependency_is_available("skill_read", "model", &config));

        config.always_exclude.push("skill_read".to_string());
        assert!(!core_dependency_is_available("skill_read", "model", &config));

        config.always_exclude.clear();
        config
            .model_allowlists
            .insert("small-model".to_string(), vec!["shell".to_string()]);
        assert!(!core_dependency_is_available("skill_read", "small-model", &config));
        assert!(core_dependency_is_available("skill_read", "other-model", &config));
    }

    /// A request the English keyword table cannot read, but which is
    /// unmistakably a request: a full clause with a verb and an object.
    const SUBSTANTIVE_PARAPHRASE: &str =
        "please take another careful look at what we have so far and tell me what you would do next";

    /// The same shape written in a script that does not separate words with
    /// spaces. It asks the agent to look something up online and summarize it,
    /// and it names no English capability keyword.
    const SUBSTANTIVE_UNSUPPORTED_LANGUAGE: &str = "ช่วยค้นหาบันทึกการเผยแพร่ล่าสุดบนอินเทอร์เน็ตแล้วสรุปให้หน่อย";

    /// A closing line in the same unspaced script ("that is enough").
    const UNSPACED_CLOSING: &str = "พอแล้ว";

    fn assert_fixture_is_unrouted(message: &str) {
        assert!(
            CLASSIFIER.classify(message).is_empty(),
            "fixture must be genuinely unrouted, otherwise the assertion is vacuous: {message}"
        );
    }

    /// R06: an established narrow session must still be able to reach a
    /// capability it never named in English. A substantive turn the keyword
    /// table cannot read republishes the whole allowed catalog **for that turn
    /// only**; the session union is left exactly as it was, so a discovery turn
    /// cannot pin the session at full width.
    ///
    /// MUTATION GUARD: make `sticky_surface` always return
    /// `KeepPinnedExposure`, or make `unrouted_turn_requests_discovery` answer
    /// `false` for everything, and the discovery assertion goes red; absorb the
    /// discovery surface into `names` and the snapshot assertion goes red.
    #[test]
    fn a_substantive_unrouted_turn_reopens_the_catalog_for_one_turn() {
        let tools = probe_registry();
        let base = crate::config::ToolTieringConfig::default();

        for request in [SUBSTANTIVE_PARAPHRASE, SUBSTANTIVE_UNSUPPORTED_LANGUAGE] {
            assert_fixture_is_unrouted(request);
            let exposure = SessionToolExposure::new();

            let routed = exposure.sticky_surface(&base, &tools, "commit this change");
            assert_eq!(
                session_names(&tools, "commit this change", &routed),
                vec!["probe_core", "probe_devops"],
                "the session must start narrow, or the discovery assertion is vacuous"
            );

            let discovery = exposure.sticky_surface(&base, &tools, request);
            assert_eq!(
                session_names(&tools, request, &discovery),
                vec!["probe_core", "probe_devops", "probe_web"],
                "a substantive unrouted request must reach a capability the session never named"
            );
            assert_eq!(
                exposure.snapshot(),
                vec!["probe_core".to_string(), "probe_devops".to_string()],
                "discovery lasts one turn and must not be absorbed into the session union"
            );

            let quiet = exposure.sticky_surface(&base, &tools, "thanks, that is all");
            assert_eq!(
                session_names(&tools, "thanks, that is all", &quiet),
                vec!["probe_core", "probe_devops"],
                "the turn after a discovery turn returns to the session set"
            );
        }
    }

    /// The other half of the contract: a greeting, a closing or a one-word
    /// acknowledgement is not a capability request in any language, so it must
    /// leave an established session exactly as narrow as it was.
    #[test]
    fn a_harmless_unrouted_follow_up_never_reopens_the_catalog() {
        let tools = probe_registry();
        let base = crate::config::ToolTieringConfig::default();
        let exposure = SessionToolExposure::new();

        let routed = exposure.sticky_surface(&base, &tools, "commit this change");
        let established = session_names(&tools, "commit this change", &routed);
        assert_eq!(established, vec!["probe_core", "probe_devops"]);

        for follow_up in ["gracias, eso es todo", "hola", "ok", UNSPACED_CLOSING] {
            assert_fixture_is_unrouted(follow_up);
            let surface = exposure.sticky_surface(&base, &tools, follow_up);
            assert_eq!(
                session_names(&tools, follow_up, &surface),
                established,
                "`{follow_up}` names nothing new and must publish the session set unchanged"
            );
            assert_eq!(
                exposure.snapshot(),
                vec!["probe_core".to_string(), "probe_devops".to_string()],
                "`{follow_up}` must not absorb anything into the session union"
            );
        }
    }

    /// Discovery widens what routing may hide, never what operator policy
    /// forbids: an excluded tool stays invisible on the discovery turn too.
    ///
    /// The registry carries **two** Extended tools on purpose. The assertion
    /// has to separate two outcomes that a three-tool registry renders
    /// identical: discovery running and withholding the excluded tool, and
    /// discovery never running at all. `probe_media` is published only if
    /// discovery ran, `probe_web` only if the exclusion was ignored, so the two
    /// halves of the contract are pinned by two assertions that fail for
    /// different reasons.
    ///
    /// MUTATION GUARD: resolve the `PublishEverything` branch of
    /// `resolve_tools_for_intent` with `full_tool_surface(all_tools, &[])` —
    /// the exclusion bypassed in the discovery branch — and `probe_web` appears;
    /// make `unrouted_turn_requests_discovery` answer `false` and `probe_media`
    /// disappears.
    #[test]
    fn a_discovery_turn_never_exposes_an_excluded_tool() {
        let tools = discovery_probe_registry();
        let base = crate::config::ToolTieringConfig {
            always_exclude: vec!["probe_web".to_string()],
            ..crate::config::ToolTieringConfig::default()
        };
        let exposure = SessionToolExposure::new();

        let routed = exposure.sticky_surface(&base, &tools, "commit this change");
        assert_eq!(
            session_names(&tools, "commit this change", &routed),
            vec!["probe_core", "probe_devops"],
            "the session must start narrower than the allowed catalog, or the discovery assertion is vacuous"
        );

        assert_fixture_is_unrouted(SUBSTANTIVE_PARAPHRASE);
        let discovery = exposure.sticky_surface(&base, &tools, SUBSTANTIVE_PARAPHRASE);
        assert_eq!(
            session_names(&tools, SUBSTANTIVE_PARAPHRASE, &discovery),
            vec!["probe_core", "probe_devops", "probe_media"],
            "a discovery turn publishes everything operator policy allows — `probe_media` proves it ran, and \
             `probe_web` is missing because always_exclude outranks it"
        );
        assert!(
            !exposure.snapshot().iter().any(|name| name == "probe_web"),
            "an excluded tool must never be absorbed into the session union"
        );
        assert!(
            !exposure.snapshot().iter().any(|name| name == "probe_media"),
            "discovery lasts one turn and must not be absorbed into the session union"
        );
    }

    /// The discovery signal itself, at the level it is decided: structure, not
    /// vocabulary. Nothing here may be satisfied by adding words to the
    /// keyword table.
    #[test]
    fn the_discovery_signal_separates_requests_from_conversation() {
        for quiet in [
            "gracias, eso es todo",
            "thanks, that is all",
            "ok",
            "hola",
            "sounds good, thank you very much",
            UNSPACED_CLOSING,
        ] {
            assert!(
                !unrouted_turn_requests_discovery(quiet),
                "`{quiet}` is conversation and must not reopen the catalog"
            );
        }

        for request in [
            SUBSTANTIVE_PARAPHRASE,
            SUBSTANTIVE_UNSUPPORTED_LANGUAGE,
            // Short, but each names a concrete object the turn must act on.
            "open https://example.com/report",
            "look at src/tools/intent.rs",
            "run `cargo check` for me",
            "check /etc/hosts",
        ] {
            assert!(
                unrouted_turn_requests_discovery(request),
                "`{request}` is a request and must reopen the catalog"
            );
        }
    }
}
