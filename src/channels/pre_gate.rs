//! Smart group-reply pre-gate: a cheap, two-tier triage that runs *before* the
//! full agent loop for smart-mode group messages that do NOT explicitly mention
//! the bot. Its sole purpose is to cut token cost by keeping obvious group noise
//! out of the (expensive) agent loop, while never silently dropping a message
//! that should have been answered.
//!
//! # Tiers
//!
//! - **Tier 1 — 0-token heuristic** ([`classify_heuristic`]): partitions a
//!   message into three buckets:
//!     - [`Heuristic::EnterLoop`] — clearly relevant (a question, a fuzzy
//!       mention of the bot's name, a direct address, or a continuation of a
//!       thread the bot recently participated in). Enters the loop immediately.
//!     - [`Heuristic::Skip`] — obvious noise (pure emoji/stickers, ultra-short
//!       social reactions like "lol"). Stays silent without calling any model.
//!     - [`Heuristic::Uncertain`] — everything else; escalates to Tier 2.
//!
//! - **Tier 2 — cheap-tier classifier** ([`classify_with_model`]): only invoked
//!   for the `Uncertain` bucket. One short prompt → `{respond: bool}`. This is
//!   the token-saving sweet spot: a single cheap call instead of a full loop.
//!
//! # Invariants
//!
//! - The pre-gate is **only** reached for smart-mode group messages with NO
//!   explicit @-mention. The caller guarantees @-mentions / DMs / non-smart
//!   modes never enter here (see `channels/mod.rs`).
//! - **Fail-open / err-toward-entering**: when the heuristic is uncertain and
//!   the classifier is disabled, errors, times out, or returns an unparseable
//!   answer, the decision is *always* [`PreGateDecision::EnterLoop`]. A pre-gate
//!   fault must never cause the bot to miss a message it should have answered.

use std::time::Duration;

use crate::config::SmartGroupConfig;
use crate::providers::Provider;

/// Tier-1 (0-token) heuristic classification of a group message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Heuristic {
    /// Clearly relevant — enter the agent loop without consulting any model.
    EnterLoop,
    /// Obvious noise — stay silent without consulting any model.
    Skip,
    /// Indeterminate — escalate to the Tier-2 cheap-model classifier.
    Uncertain,
}

/// Final pre-gate decision for a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreGateDecision {
    /// Proceed into the full agent loop.
    EnterLoop,
    /// Stay silent: do not enter the loop, do not send, do not write history.
    Skip,
}

/// Which path produced the [`PreGateDecision`], for metrics / observability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreGatePath {
    /// Pre-gate disabled by config — message entered the loop unconditionally.
    Disabled,
    /// Tier-1 heuristic decided `EnterLoop` (no model called).
    HeuristicEnter,
    /// Tier-1 heuristic decided `Skip` (no model called).
    HeuristicSkip,
    /// Tier-2 classifier decided to enter the loop.
    ClassifierEnter,
    /// Tier-2 classifier decided to skip.
    ClassifierSkip,
    /// Tier-2 path failed (disabled/error/timeout/unparseable) and failed open.
    ClassifierFailOpen,
}

impl PreGatePath {
    /// Stable, snake_case label for metrics emission.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::HeuristicEnter => "heuristic_enter",
            Self::HeuristicSkip => "heuristic_skip",
            Self::ClassifierEnter => "classifier_enter",
            Self::ClassifierSkip => "classifier_skip",
            Self::ClassifierFailOpen => "classifier_fail_open",
        }
    }
}

/// Outcome of a pre-gate evaluation: the decision plus the path that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreGateOutcome {
    pub decision: PreGateDecision,
    pub path: PreGatePath,
}

impl PreGateOutcome {
    /// Construct an "enter the loop" outcome with the given attribution path.
    #[must_use]
    pub const fn enter(path: PreGatePath) -> Self {
        Self {
            decision: PreGateDecision::EnterLoop,
            path,
        }
    }

    /// Construct a "stay silent" outcome with the given attribution path.
    #[must_use]
    pub const fn skip(path: PreGatePath) -> Self {
        Self {
            decision: PreGateDecision::Skip,
            path,
        }
    }

    /// Whether this outcome means the agent loop should run.
    #[must_use]
    pub const fn should_enter_loop(self) -> bool {
        matches!(self.decision, PreGateDecision::EnterLoop)
    }
}

/// Common interrogative words that strongly signal the message wants an answer.
const QUESTION_WORDS: &[&str] = &[
    "who",
    "what",
    "when",
    "where",
    "why",
    "how",
    "which",
    "can ",
    "could ",
    "should ",
    "would ",
    "is ",
    "are ",
    "does ",
    "do ",
    "did ",
    "will ",
    "谁",
    "什么",
    "为什么",
    "怎么",
    "怎样",
    "哪",
    "吗",
    "呢",
    "如何",
    "是否",
];

/// Tiny set of obvious social-noise tokens. Kept intentionally short: the goal
/// is to catch *clear* throwaway reactions, not to be a sentiment classifier.
/// Anything not matched here (and not clearly relevant) becomes `Uncertain`,
/// which errs toward entering the loop via the classifier / fail-open.
const NOISE_TOKENS: &[&str] = &[
    "lol",
    "lmao",
    "rofl",
    "haha",
    "hahaha",
    "hehe",
    "ok",
    "okay",
    "k",
    "kk",
    "yes",
    "no",
    "yep",
    "nope",
    "yeah",
    "nah",
    "thx",
    "thanks",
    "ty",
    "np",
    "gg",
    "wow",
    "nice",
    "cool",
    "+1",
    "👍",
    "哈哈",
    "哈哈哈",
    "嗯",
    "好",
    "好的",
    "收到",
    "谢谢",
    "牛",
    "赞",
    "可以",
    "行",
];

/// Maximum character length for a message to be eligible for the "ultra-short
/// social reaction" noise bucket. Above this, even a low-signal message is
/// treated as `Uncertain` (err toward entering).
const SHORT_NOISE_MAX_CHARS: usize = 12;

/// Returns true if `text` is composed solely of emoji / symbol / punctuation /
/// whitespace characters (no letters or digits in any script) — i.e. a pure
/// emoji or sticker-style reaction.
fn is_pure_symbolic(text: &str) -> bool {
    let mut saw_char = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            continue;
        }
        saw_char = true;
        if ch.is_alphanumeric() {
            return false;
        }
    }
    saw_char
}

/// Tier-1 heuristic classification (0 tokens).
///
/// `text` is the already-metadata-stripped user content. `bot_names` are the
/// bot's configured display names (lowercased comparison is done internally).
/// `bot_recently_active` indicates the bot participated in this group's recent
/// turns (topic-continuation signal), which biases toward entering the loop.
#[must_use]
pub fn classify_heuristic(text: &str, bot_names: &[String], bot_recently_active: bool) -> Heuristic {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        // Nothing to say to — treat as noise (the loop would have nothing to act
        // on, and an empty body is never a question).
        return Heuristic::Skip;
    }

    let lower = trimmed.to_lowercase();

    // ── Clearly relevant signals (enter the loop) ───────────────────────────
    // 1. Fuzzy bot-name mention anywhere in the text. (Explicit @-mentions never
    //    reach the pre-gate; this catches name-without-@ "hey assistant ...".)
    for name in bot_names {
        let name = name.trim().to_lowercase();
        if name.len() >= 2 && lower.contains(&name) {
            return Heuristic::EnterLoop;
        }
    }

    // 2. Explicit question mark (ASCII or full-width).
    if trimmed.contains('?') || trimmed.contains('？') {
        return Heuristic::EnterLoop;
    }

    // 3. Interrogative lead-in words.
    let probe = format!("{lower} ");
    for q in QUESTION_WORDS {
        if probe.starts_with(q) || probe.contains(&format!(" {q}")) {
            return Heuristic::EnterLoop;
        }
    }

    // ── Obvious noise (skip without any model call) ─────────────────────────
    // 4. Pure emoji / sticker / punctuation.
    if is_pure_symbolic(trimmed) {
        return Heuristic::Skip;
    }

    // 5. Ultra-short social reactions, but ONLY when the bot is not in an active
    //    back-and-forth in this group. If the bot was just talking, a terse
    //    "ok"/"yes" may be a reply to it → let the loop (and stay_silent) judge.
    if !bot_recently_active && trimmed.chars().count() <= SHORT_NOISE_MAX_CHARS {
        let normalized: String = lower
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '!' && *c != '.')
            .collect();
        if NOISE_TOKENS.iter().any(|t| normalized == *t) {
            return Heuristic::Skip;
        }
    }

    // 6. Topic continuation: the bot recently participated → bias to entering.
    if bot_recently_active {
        return Heuristic::EnterLoop;
    }

    // ── Everything else is genuinely uncertain → Tier 2. ────────────────────
    Heuristic::Uncertain
}

/// System prompt for the Tier-2 cheap classifier. Deliberately tiny.
///
/// The prompt explicitly requests `YES`/`NO` only to reduce the chance that a
/// model replying in the user's language (e.g. Chinese) produces a verbose
/// answer. The parser in [`parse_classifier_answer`] also recognises Chinese
/// decisive tokens as a robust fallback.
const CLASSIFIER_SYSTEM_PROMPT: &str = "You are a fast relevance gate for a group-chat assistant. \
Given recent group messages and the latest message, decide whether the assistant should reply to the \
latest message. Reply ONLY with a single word: YES if the assistant should reply, NO if it should stay \
silent. Do NOT reply in any language other than this single word. \
The assistant is one participant among humans; it should reply when addressed, asked a question, \
or able to genuinely help, and stay silent for casual chatter between others. When unsure, answer YES.";

/// Build the user prompt for the classifier from recent context + latest text.
fn build_classifier_prompt(recent_context: &[String], latest: &str, bot_names: &[String]) -> String {
    let mut prompt = String::new();
    if let Some(primary) = bot_names.first() {
        prompt.push_str("Assistant name: ");
        prompt.push_str(primary.trim());
        prompt.push('\n');
    }
    if !recent_context.is_empty() {
        prompt.push_str("Recent messages:\n");
        for line in recent_context {
            prompt.push_str("- ");
            prompt.push_str(line.trim());
            prompt.push('\n');
        }
    }
    prompt.push_str("Latest message: ");
    prompt.push_str(latest.trim());
    prompt.push_str("\n\nShould the assistant reply? Answer YES or NO.");
    prompt
}

/// Parse the classifier's free-form answer into a respond/silent decision.
///
/// Returns `Some(true)` for an affirmative, `Some(false)` for a negative, and
/// `None` if the answer is unparseable (caller fails open).
///
/// Recognises both English (`YES`/`NO`) and Chinese decisive tokens so that
/// models that reply in Chinese (e.g. Kimi) are handled without falling through
/// to the fail-open path:
/// - Affirmative: `是`、`回应`、`回复`
/// - Negative:    `否`、`不`、`沉默`、`不回`
fn parse_classifier_answer(answer: &str) -> Option<bool> {
    let trimmed = answer.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_lowercase();

    // ── Chinese decisive tokens (checked before ASCII splitting, which would
    //    fragment multi-byte CJK characters into empty or garbled tokens). ───
    //
    // Negative tokens are checked FIRST, and longer/more-specific patterns
    // before shorter ones, so that `不回复` / `不需要回复` are correctly
    // identified as negative even though they contain the substring `回复`.
    //
    // Negative: 沉默 / 不回 (covers 不回复/不回答) / 否 / 不
    // Affirmative: 回应 / 回复 (longer first) / 是
    for zh_no in &["沉默", "不回", "否", "不"] {
        if lower.contains(zh_no) {
            return Some(false);
        }
    }
    for zh_yes in &["回应", "回复", "是"] {
        if lower.contains(zh_yes) {
            return Some(true);
        }
    }

    // ── ASCII / Latin tokens: split on non-alphanumeric, take first match. ──
    for token in lower.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()) {
        match token {
            "yes" | "y" | "true" | "reply" | "respond" => return Some(true),
            "no" | "n" | "false" | "silent" | "skip" | "ignore" => return Some(false),
            _ => {}
        }
    }
    None
}

/// Tier-2: ask the cheap classifier whether to respond. Returns the outcome
/// (always failing open to `EnterLoop` on any error/timeout/unparseable answer).
///
/// `provider`/`model` are the resolved classifier provider+model (the caller
/// resolves these from [`SmartGroupConfig`], falling back to the channel route).
pub async fn classify_with_model(
    provider: &dyn Provider,
    model: &str,
    cfg: &SmartGroupConfig,
    recent_context: &[String],
    latest: &str,
    bot_names: &[String],
) -> PreGateOutcome {
    let prompt = build_classifier_prompt(recent_context, latest, bot_names);
    let call = provider.chat_with_system(
        Some(CLASSIFIER_SYSTEM_PROMPT),
        &prompt,
        model,
        cfg.classifier_temperature,
    );
    let timeout = Duration::from_secs(cfg.classifier_timeout_secs.max(1));

    match tokio::time::timeout(timeout, call).await {
        Ok(Ok(answer)) => match parse_classifier_answer(&answer) {
            Some(true) => PreGateOutcome::enter(PreGatePath::ClassifierEnter),
            Some(false) => PreGateOutcome::skip(PreGatePath::ClassifierSkip),
            None => {
                tracing::debug!(answer = %answer, "pre-gate classifier returned unparseable answer; failing open");
                PreGateOutcome::enter(PreGatePath::ClassifierFailOpen)
            }
        },
        Ok(Err(err)) => {
            tracing::warn!("pre-gate classifier call failed; failing open (enter loop): {err}");
            PreGateOutcome::enter(PreGatePath::ClassifierFailOpen)
        }
        Err(_elapsed) => {
            tracing::warn!(
                timeout_secs = cfg.classifier_timeout_secs,
                "pre-gate classifier timed out; failing open (enter loop)"
            );
            PreGateOutcome::enter(PreGatePath::ClassifierFailOpen)
        }
    }
}

/// Resolve the heuristic bucket into a decision *without* a classifier call.
/// Used when the classifier is disabled (uncertain → fail-open enter) and as the
/// terminal step for decisive heuristic buckets.
#[must_use]
pub const fn heuristic_only_outcome(h: Heuristic, pre_gate_enabled: bool) -> PreGateOutcome {
    if !pre_gate_enabled {
        return PreGateOutcome::enter(PreGatePath::Disabled);
    }
    match h {
        Heuristic::EnterLoop => PreGateOutcome::enter(PreGatePath::HeuristicEnter),
        Heuristic::Skip => PreGateOutcome::skip(PreGatePath::HeuristicSkip),
        // Uncertain + no classifier → err toward entering (fail-open).
        Heuristic::Uncertain => PreGateOutcome::enter(PreGatePath::ClassifierFailOpen),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        vec!["prx".to_string(), "assistant".to_string()]
    }

    // ── Tier-1 three-way classification ─────────────────────────────────────

    #[test]
    fn heuristic_question_enters_loop() {
        assert_eq!(
            classify_heuristic("what time is the meeting", &names(), false),
            Heuristic::EnterLoop
        );
        assert_eq!(classify_heuristic("really?", &names(), false), Heuristic::EnterLoop);
        assert_eq!(classify_heuristic("现在几点？", &names(), false), Heuristic::EnterLoop);
    }

    #[test]
    fn heuristic_fuzzy_bot_name_enters_loop() {
        assert_eq!(
            classify_heuristic("hey prx can you look at this", &names(), false),
            Heuristic::EnterLoop
        );
        // Name match wins even without a question mark.
        assert_eq!(
            classify_heuristic("assistant pull the latest", &names(), false),
            Heuristic::EnterLoop
        );
    }

    #[test]
    fn heuristic_topic_continuation_enters_loop() {
        // Plain statement, but bot was recently active → continuation → enter.
        assert_eq!(
            classify_heuristic("that makes sense to me", &names(), true),
            Heuristic::EnterLoop
        );
    }

    #[test]
    fn heuristic_pure_emoji_skips() {
        assert_eq!(classify_heuristic("😂😂😂", &names(), false), Heuristic::Skip);
        assert_eq!(classify_heuristic("👍", &names(), false), Heuristic::Skip);
        assert_eq!(classify_heuristic("!!!", &names(), false), Heuristic::Skip);
    }

    #[test]
    fn heuristic_short_social_reaction_skips() {
        assert_eq!(classify_heuristic("lol", &names(), false), Heuristic::Skip);
        assert_eq!(classify_heuristic("haha!", &names(), false), Heuristic::Skip);
        assert_eq!(classify_heuristic("哈哈", &names(), false), Heuristic::Skip);
        assert_eq!(classify_heuristic("thanks", &names(), false), Heuristic::Skip);
    }

    #[test]
    fn heuristic_short_reaction_when_bot_active_is_not_skipped() {
        // "ok" could be a reply to the bot if it just spoke → not noise-skipped.
        assert_ne!(classify_heuristic("ok", &names(), true), Heuristic::Skip);
    }

    #[test]
    fn heuristic_ambiguous_statement_is_uncertain() {
        assert_eq!(
            classify_heuristic("the deploy went out an hour ago to prod servers", &names(), false),
            Heuristic::Uncertain
        );
    }

    #[test]
    fn heuristic_empty_skips() {
        assert_eq!(classify_heuristic("   ", &names(), false), Heuristic::Skip);
    }

    // ── Answer parsing ──────────────────────────────────────────────────────

    #[test]
    fn parse_answer_variants() {
        assert_eq!(parse_classifier_answer("YES"), Some(true));
        assert_eq!(parse_classifier_answer("  yes, definitely "), Some(true));
        assert_eq!(parse_classifier_answer("No."), Some(false));
        assert_eq!(parse_classifier_answer("silent"), Some(false));
        assert_eq!(parse_classifier_answer("reply"), Some(true));
        assert_eq!(parse_classifier_answer(""), None);
        assert_eq!(parse_classifier_answer("maybe perhaps"), None);
    }

    #[test]
    fn parse_answer_chinese_affirmative() {
        // Bare affirmative tokens.
        assert_eq!(parse_classifier_answer("是"), Some(true));
        assert_eq!(parse_classifier_answer("回应"), Some(true));
        assert_eq!(parse_classifier_answer("回复"), Some(true));
        // Embedded in a phrase (kimi-style verbose reply).
        assert_eq!(parse_classifier_answer("应该回应这条消息"), Some(true));
        assert_eq!(parse_classifier_answer("助手需要回复用户"), Some(true));
    }

    #[test]
    fn parse_answer_chinese_negative() {
        // Bare negative tokens.
        assert_eq!(parse_classifier_answer("否"), Some(false));
        assert_eq!(parse_classifier_answer("不"), Some(false));
        assert_eq!(parse_classifier_answer("沉默"), Some(false));
        assert_eq!(parse_classifier_answer("不回"), Some(false));
        // Embedded in a phrase.
        assert_eq!(parse_classifier_answer("助手应该沉默"), Some(false));
        assert_eq!(parse_classifier_answer("不需要回复"), Some(false));
    }

    #[test]
    fn parse_answer_chinese_unparseable_returns_none() {
        // A Chinese reply that contains none of the decisive tokens.
        assert_eq!(parse_classifier_answer("这个问题比较复杂"), None);
        assert_eq!(parse_classifier_answer("也许可以考虑一下"), None);
    }

    #[test]
    fn parse_answer_chinese_affirmative_wins_over_spurious_negative_substring() {
        // "回应" contains no negative token; must parse as affirmative.
        assert_eq!(parse_classifier_answer("回应"), Some(true));
    }

    // ── heuristic_only_outcome (classifier-disabled path) ───────────────────

    #[test]
    fn heuristic_only_disabled_always_enters() {
        let o = heuristic_only_outcome(Heuristic::Skip, false);
        assert!(o.should_enter_loop());
        assert_eq!(o.path, PreGatePath::Disabled);
    }

    #[test]
    fn heuristic_only_uncertain_fails_open() {
        let o = heuristic_only_outcome(Heuristic::Uncertain, true);
        assert!(o.should_enter_loop());
        assert_eq!(o.path, PreGatePath::ClassifierFailOpen);
    }

    #[test]
    fn heuristic_only_skip_skips() {
        let o = heuristic_only_outcome(Heuristic::Skip, true);
        assert!(!o.should_enter_loop());
        assert_eq!(o.path, PreGatePath::HeuristicSkip);
    }

    // ── Classifier fail-open behavior (no real provider needed) ─────────────

    use crate::providers::traits::ProviderCapabilities;
    use async_trait::async_trait;

    struct FailingProvider;
    #[async_trait]
    impl Provider for FailingProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            anyhow::bail!("simulated classifier failure")
        }
    }

    struct YesProvider;
    #[async_trait]
    impl Provider for YesProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok("NO".to_string())
        }
    }

    #[tokio::test]
    async fn classifier_failure_fails_open_to_enter_loop() {
        let cfg = SmartGroupConfig::default();
        let outcome = classify_with_model(&FailingProvider, "m", &cfg, &[], "ambiguous statement", &names()).await;
        assert!(outcome.should_enter_loop(), "classifier failure must fail open");
        assert_eq!(outcome.path, PreGatePath::ClassifierFailOpen);
    }

    #[tokio::test]
    async fn classifier_no_answer_skips() {
        let cfg = SmartGroupConfig::default();
        let outcome = classify_with_model(&YesProvider, "m", &cfg, &[], "casual banter", &names()).await;
        assert!(!outcome.should_enter_loop(), "explicit NO should skip");
        assert_eq!(outcome.path, PreGatePath::ClassifierSkip);
    }
}
