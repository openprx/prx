use crate::agent::classifier::TaskIntent;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouterIntent {
    Conversation,
    Code,
    Analysis,
    Summary,
    LongDoc,
    Tool,
    Unknown,
}

pub fn infer_router_intent(task_intent: TaskIntent, message: &str) -> RouterIntent {
    if matches!(task_intent, TaskIntent::Delegate) {
        return RouterIntent::Tool;
    }

    if message.chars().count() > 2_000 {
        return RouterIntent::LongDoc;
    }

    let lower = message.to_lowercase();

    if [
        "code", "debug", "compile", "build", "function", "class", "fn ", "impl", "cargo", "npm",
    ]
    .iter()
    .any(|keyword| lower.contains(keyword))
    {
        return RouterIntent::Code;
    }

    if [
        "summary",
        "summarize",
        "summarise",
        "abstract",
        "translate",
        "translation",
    ]
    .iter()
    .any(|keyword| lower.contains(keyword))
    {
        return RouterIntent::Summary;
    }

    if [
        "analyze",
        "analyse",
        "analysis",
        "evaluate",
        "assess",
        "compare",
        "comparison",
    ]
    .iter()
    .any(|keyword| lower.contains(keyword))
    {
        return RouterIntent::Analysis;
    }

    RouterIntent::Conversation
}

impl RouterIntent {
    pub const fn category_name(&self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::Code => "code",
            Self::Analysis => "analysis",
            Self::Summary => "summary",
            Self::LongDoc => "long_doc",
            Self::Tool => "tool",
            Self::Unknown => "unknown",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infer_code_intent_from_keywords() {
        assert_eq!(
            infer_router_intent(TaskIntent::Stream, "please debug this cargo build error"),
            RouterIntent::Code
        );
    }

    #[test]
    fn infer_summary_and_analysis_intents_from_english_keywords() {
        assert_eq!(
            infer_router_intent(TaskIntent::Stream, "write a summary of this thread"),
            RouterIntent::Summary
        );
        assert_eq!(
            infer_router_intent(TaskIntent::Stream, "translate the release notes"),
            RouterIntent::Summary
        );
        assert_eq!(
            infer_router_intent(TaskIntent::Stream, "compare the two rollout plans"),
            RouterIntent::Analysis
        );
        assert_eq!(
            infer_router_intent(TaskIntent::Stream, "evaluate the migration risk"),
            RouterIntent::Analysis
        );
    }

    /// A request the English table cannot read stays on the neutral default
    /// rather than being forced into a specialised bucket.
    #[test]
    fn requests_without_english_keywords_fall_back_to_conversation() {
        assert_eq!(
            infer_router_intent(TaskIntent::Stream, "Ol\u{e1}, tudo bem?"),
            RouterIntent::Conversation
        );
    }

    #[test]
    fn infer_long_doc_from_length() {
        let message = "a".repeat(2_100);
        assert_eq!(infer_router_intent(TaskIntent::Stream, &message), RouterIntent::LongDoc);
    }
}
