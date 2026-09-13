//! Intent-based tool filtering for intelligent context management.
//!
//! Uses lightweight keyword matching (<1ms) to classify user messages into
//! semantic categories, then filters the tool registry so only relevant tools
//! are sent to the LLM — reducing context noise and improving response quality.
//!
//! The keyword table is English-only by project policy. Narrowing therefore
//! only ever happens on evidence: a message that activates no category at all
//! (a non-English request, or an English one that names no capability) keeps
//! the whole registry instead of collapsing to the [`ToolTier::Core`] floor.
//! Trimming a catalog on the strength of a keyword table that cannot read the
//! request would silently amputate capabilities the user asked for, which is a
//! far worse failure than paying for a few extra tool schemas.

use std::collections::HashSet;
use std::sync::LazyLock;

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
/// instruction such as "do not use browser or MCP" or "不要使用浏览器/MCP".
///
/// Capability routing happens before the model sees the tool catalog, so a
/// plain substring match on a forbidden capability is particularly harmful:
/// the request "do not use MCP" used to cold-start every configured MCP server.
/// Keep this deliberately narrow and clause-local. Positive mentions in a
/// later sentence or after an explicit contrast ("but use MCP" / "但使用 MCP")
/// still activate the category.
fn keyword_is_in_negative_instruction(message: &str, keyword_index: usize) -> bool {
    const CLAUSE_BOUNDARIES: &[char] = &['.', '!', '?', ';', '\n', '\r', '。', '！', '？', '；'];
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
        "禁止使用",
        "禁止调用",
        "不要使用",
        "不要调用",
        "不使用",
        "不调用",
        "不得使用",
        "不得调用",
        "无需使用",
        "无需调用",
        "别用",
        "别调用",
    ];
    const POSITIVE_PIVOTS: &[&str] = &[" but ", " instead ", " however ", "但", "但是", "而是", "改用"];

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

/// Filter tools based on user intent. Core tools always included.
/// Standard tools included if any of their categories match (or if no categories are set).
/// Extended tools only included on explicit category match.
///
/// When the classifier activates **no** category the turn is *unrouted*: the
/// keyword table produced no evidence about this request, so the full registry
/// is published instead of the Core floor. `always_exclude` (and therefore the
/// channel surface folded into it) still applies — the fallback widens what
/// intent routing may hide, never what operator policy forbids.
///
/// The `always_include` / `always_exclude` lists (tool names) are applied after
/// tier-based filtering to allow user overrides.
pub fn select_tools_for_intent<'a>(
    all_tools: &'a [Box<dyn Tool>],
    user_message: &str,
    always_include: &[String],
    always_exclude: &[String],
) -> Vec<&'a dyn Tool> {
    let activated = CLASSIFIER.classify(user_message);
    let unrouted = activated.is_empty();

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

            // No category activated: the classifier has nothing to say about
            // this request, so it does not get to remove anything from it.
            if unrouted {
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
        tracing::debug!(?active, ?rejected, unrouted, "capability routing decision");
    }

    selected
}

/// Return whether a core dependency may be exposed under the turn's explicit
/// exclusion and model-allowlist policy. This is used by prompt compilers to
/// avoid advertising capabilities whose required tool cannot be selected.
pub fn core_dependency_is_available(name: &str, model: &str, config: &crate::config::ToolTieringConfig) -> bool {
    !config.always_exclude.iter().any(|excluded| excluded == name)
        && model_allows_tool_name(model, name, &config.model_allowlists)
}

/// Apply an optional exact-model allowlist after ordinary intent tiering.
///
/// Keeping this as a second-stage intersection preserves the global
/// `always_exclude` boundary and prevents a model allowlist from enabling a
/// capability that intent tiering or operator policy already removed.
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

/// Return whether a named prompt/catalog entry is visible to this model.
/// Models without a configured allowlist retain the ordinary tool surface.
pub fn model_allows_tool_name<S: std::hash::BuildHasher>(
    model: &str,
    tool_name: &str,
    model_allowlists: &std::collections::HashMap<String, Vec<String>, S>,
) -> bool {
    model_allowlists
        .get(model)
        .is_none_or(|allowlist| allowlist.iter().any(|allowed| allowed == tool_name))
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

    /// Absorb this turn's routing decision and return the tiering policy that
    /// exposes the session's cumulative set.
    ///
    /// The union is handed back through the existing `always_include` knob, so
    /// the tool loop keeps running exactly one selector. `always_exclude` still
    /// outranks `always_include` inside [`select_tools_for_intent`], so a name
    /// operator policy forbids can never be resurrected by stickiness.
    #[must_use]
    pub fn sticky_tiering(
        &self,
        base: &crate::config::ToolTieringConfig,
        all_tools: &[Box<dyn Tool>],
        routing_input: &str,
    ) -> crate::config::ToolTieringConfig {
        let routed = select_tools_for_intent(all_tools, routing_input, &base.always_include, &base.always_exclude);
        let mut names = self.names.lock();
        for tool in routed {
            if !names.contains(tool.name()) {
                names.insert(tool.name().to_string());
            }
        }
        let mut sticky = base.clone();
        for name in names.iter() {
            if !sticky.always_include.iter().any(|existing| existing == name) {
                sticky.always_include.push(name.clone());
            }
        }
        sticky
    }
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
            CLASSIFIER
                .classify("\u{4f60}\u{597d}\u{ff0c}\u{4eca}\u{5929}\u{5929}\u{6c14}\u{600e}\u{4e48}\u{6837}\u{ff1f}")
                .is_empty(),
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

    /// The negative-instruction guard also has to hold when the sentence around
    /// the English capability names is not English: those markers are the only
    /// thing standing between "do not touch MCP" and a cold MCP start.
    #[test]
    fn non_english_negative_capability_instructions_do_not_activate_categories() {
        let cats = CLASSIFIER.classify("\u{8bfb}\u{53d6} PDF skill\u{ff1b}\u{4e0d}\u{8981}\u{8c03}\u{7528} MCP\u{3001}plugin \u{6216} WASM\u{3002}");
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

        let selected = select_tools_for_intent(&tools, "search the release notes", &[], &[]);
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
        let selected = select_tools_for_intent(&tools, "run shell", &[], &[]);
        let allowlists = std::collections::HashMap::from([("small-model".to_string(), vec!["shell".to_string()])]);

        let filtered = apply_model_tool_allowlist(selected, "small-model", &allowlists);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name(), "shell");

        let unconfigured = select_tools_for_intent(&tools, "run shell", &[], &[]);
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

    fn selected_names(
        tools: &[Box<dyn Tool>],
        message: &str,
        tiering: &crate::config::ToolTieringConfig,
    ) -> Vec<String> {
        let mut names = select_tools_for_intent(tools, message, &tiering.always_include, &tiering.always_exclude)
            .into_iter()
            .map(|tool| tool.name().to_string())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    /// MUTATION GUARD: delete the `unrouted` short-circuit in
    /// `select_tools_for_intent` and this goes red.
    #[test]
    fn an_unrouted_message_keeps_the_whole_registry() {
        let tools = probe_registry();
        let tiering = crate::config::ToolTieringConfig::default();
        assert_eq!(
            selected_names(
                &tools,
                "\u{5e2e}\u{6211}\u{770b}\u{4e00}\u{4e0b}\u{8fd9}\u{4e2a}\u{600e}\u{4e48}\u{529e}",
                &tiering
            ),
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

    /// MUTATION GUARD: make `sticky_tiering` return `base.clone()` and the
    /// second assertion goes red.
    #[test]
    fn session_tool_exposure_only_ever_grows_until_reset() {
        let tools = probe_registry();
        let base = crate::config::ToolTieringConfig::default();
        let exposure = SessionToolExposure::new();

        let first = exposure.sticky_tiering(&base, &tools, "commit this change");
        assert_eq!(
            selected_names(&tools, "commit this change", &first),
            vec!["probe_core", "probe_devops"]
        );

        // A turn that names nothing this session has not already asked for must
        // publish the identical set, whatever the wording.
        let second = exposure.sticky_tiering(&base, &tools, "please push the merge to the branch");
        assert_eq!(
            selected_names(&tools, "please push the merge to the branch", &second),
            vec!["probe_core", "probe_devops"]
        );

        // A genuinely new intent widens the set exactly once, and it stays.
        let third = exposure.sticky_tiering(&base, &tools, "search the changelog");
        assert_eq!(
            selected_names(&tools, "search the changelog", &third),
            vec!["probe_core", "probe_devops", "probe_web"]
        );
        let fourth = exposure.sticky_tiering(&base, &tools, "commit this change");
        assert_eq!(
            selected_names(&tools, "commit this change", &fourth),
            vec!["probe_core", "probe_devops", "probe_web"],
            "a routed set may not shrink inside one session"
        );

        exposure.reset();
        assert!(exposure.snapshot().is_empty());
        let after_reset = exposure.sticky_tiering(&base, &tools, "commit this change");
        assert_eq!(
            selected_names(&tools, "commit this change", &after_reset),
            vec!["probe_core", "probe_devops"],
            "a session boundary starts the union over"
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
        let sticky = exposure.sticky_tiering(&base, &tools, "search the changelog");
        assert_eq!(
            selected_names(&tools, "search the changelog", &sticky),
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
}
