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
        "代码", "code", "debug", "编译", "函数", "class", "fn ", "impl", "cargo", "npm",
    ]
    .iter()
    .any(|keyword| lower.contains(keyword))
    {
        return RouterIntent::Code;
    }

    if ["总结", "摘要", "翻译", "summary", "translate"]
        .iter()
        .any(|keyword| lower.contains(keyword))
    {
        return RouterIntent::Summary;
    }

    if ["分析", "analyze", "评估", "对比", "compare"]
        .iter()
        .any(|keyword| lower.contains(keyword))
    {
        return RouterIntent::Analysis;
    }

    RouterIntent::Conversation
}

impl RouterIntent {
    pub fn category_name(&self) -> &'static str {
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
            infer_router_intent(TaskIntent::Stream, "请 debug 这段 cargo build 错误"),
            RouterIntent::Code
        );
    }

    #[test]
    fn infer_long_doc_from_length() {
        let message = "a".repeat(2_100);
        assert_eq!(infer_router_intent(TaskIntent::Stream, &message), RouterIntent::LongDoc);
    }
}
