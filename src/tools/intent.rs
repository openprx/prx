//! Intent-based tool filtering for intelligent context management.
//!
//! Uses lightweight keyword matching (<1ms) to classify user messages into
//! semantic categories, then filters the tool registry so only relevant tools
//! are sent to the LLM — reducing context noise and improving response quality.

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
            ("\u{641c}\u{7d22}", ToolCategory::WebBrowsing),
            ("search", ToolCategory::WebBrowsing),
            ("google", ToolCategory::WebBrowsing),
            ("\u{7f51}\u{9875}", ToolCategory::WebBrowsing),
            ("url", ToolCategory::WebBrowsing),
            ("fetch", ToolCategory::WebBrowsing),
            ("browse", ToolCategory::WebBrowsing),
            ("website", ToolCategory::WebBrowsing),
            ("http", ToolCategory::WebBrowsing),
            // Scheduling
            ("\u{5b9a}\u{65f6}", ToolCategory::Scheduling),
            ("cron", ToolCategory::Scheduling),
            ("schedule", ToolCategory::Scheduling),
            ("\u{5b9a}\u{671f}", ToolCategory::Scheduling),
            ("timer", ToolCategory::Scheduling),
            ("heartbeat", ToolCategory::Scheduling),
            // Communication
            ("\u{53d1}\u{9001}", ToolCategory::Communication),
            ("\u{901a}\u{77e5}", ToolCategory::Communication),
            ("push", ToolCategory::Communication),
            ("notify", ToolCategory::Communication),
            ("message", ToolCategory::Communication),
            ("tts", ToolCategory::Communication),
            ("\u{8bed}\u{97f3}", ToolCategory::Communication),
            ("telegram", ToolCategory::Communication),
            ("discord", ToolCategory::Communication),
            ("slack", ToolCategory::Communication),
            // Memory
            ("\u{8bb0}\u{4f4f}", ToolCategory::Memory),
            ("remember", ToolCategory::Memory),
            ("forget", ToolCategory::Memory),
            ("\u{5fd8}\u{8bb0}", ToolCategory::Memory),
            ("memory", ToolCategory::Memory),
            ("\u{8bb0}\u{5fc6}", ToolCategory::Memory),
            ("\u{5b58}\u{50a8}", ToolCategory::Memory),
            // DevOps
            ("git", ToolCategory::DevOps),
            ("commit", ToolCategory::DevOps),
            ("deploy", ToolCategory::DevOps),
            ("api", ToolCategory::DevOps),
            ("branch", ToolCategory::DevOps),
            ("merge", ToolCategory::DevOps),
            // Media
            ("\u{56fe}\u{7247}", ToolCategory::Media),
            ("image", ToolCategory::Media),
            ("screenshot", ToolCategory::Media),
            ("\u{622a}\u{56fe}", ToolCategory::Media),
            ("canvas", ToolCategory::Media),
            ("photo", ToolCategory::Media),
            // System / Automation
            ("session", ToolCategory::System),
            ("config", ToolCategory::System),
            ("proxy", ToolCategory::System),
            ("node", ToolCategory::System),
            ("gateway", ToolCategory::System),
            ("mcp", ToolCategory::Automation),
            ("composio", ToolCategory::Automation),
            ("delegate", ToolCategory::Automation),
            ("agent", ToolCategory::Automation),
            ("\u{5b50}\u{4ee3}\u{7406}", ToolCategory::Automation),
            ("spawn", ToolCategory::Automation),
            // FileSystem (extra activation)
            ("\u{6587}\u{4ef6}", ToolCategory::FileSystem),
            ("file", ToolCategory::FileSystem),
            ("\u{76ee}\u{5f55}", ToolCategory::FileSystem),
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
            if lower.contains(pattern.as_str()) {
                cats.insert(*category);
            }
        }
        cats
    }
}

static CLASSIFIER: LazyLock<IntentClassifier> = LazyLock::new(IntentClassifier::new);

/// Filter tools based on user intent. Core tools always included.
/// Standard tools included if any of their categories match (or if no categories are set).
/// Extended tools only included on explicit category match.
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

    all_tools
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
        .collect()
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

    #[test]
    fn classify_simple_greeting() {
        let cats = CLASSIFIER
            .classify("\u{4f60}\u{597d}\u{ff0c}\u{4eca}\u{5929}\u{5929}\u{6c14}\u{600e}\u{4e48}\u{6837}\u{ff1f}");
        assert!(cats.is_empty(), "Simple greeting should not activate any category");
    }

    #[test]
    fn classify_web_search() {
        let cats = CLASSIFIER.classify("\u{5e2e}\u{6211}\u{641c}\u{7d22}\u{4e00}\u{4e0b} Rust \u{6559}\u{7a0b}");
        assert!(cats.contains(&ToolCategory::WebBrowsing));
    }

    #[test]
    fn classify_scheduling() {
        let cats = CLASSIFIER.classify("\u{8bbe}\u{7f6e}\u{4e00}\u{4e2a} cron \u{5b9a}\u{65f6}\u{4efb}\u{52a1}");
        assert!(cats.contains(&ToolCategory::Scheduling));
    }

    #[test]
    fn classify_memory() {
        let cats = CLASSIFIER.classify("\u{8bb0}\u{4f4f}\u{6211}\u{559c}\u{6b22}\u{7528} Rust");
        assert!(cats.contains(&ToolCategory::Memory));
    }

    #[test]
    fn classify_devops() {
        let cats = CLASSIFIER.classify("commit \u{8fd9}\u{4e9b}\u{4fee}\u{6539}\u{5230} git");
        assert!(cats.contains(&ToolCategory::DevOps));
    }

    #[test]
    fn classify_mixed() {
        let cats = CLASSIFIER.classify(
            "\u{641c}\u{7d22}\u{6587}\u{6863}\u{7136}\u{540e}\u{8bb0}\u{4f4f}\u{5173}\u{952e}\u{4fe1}\u{606f}",
        );
        assert!(cats.contains(&ToolCategory::WebBrowsing));
        assert!(cats.contains(&ToolCategory::Memory));
    }
}
