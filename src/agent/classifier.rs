use crate::config::schema::{
    QueryClassificationConfig, TaskRoutingConfig, TaskRoutingIntentConfig,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskIntent {
    Simple,
    Delegate,
    Stream,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifyResult {
    pub intent: TaskIntent,
    pub model_hint: Option<String>,
    pub reason: String,
}

impl From<TaskRoutingIntentConfig> for TaskIntent {
    fn from(value: TaskRoutingIntentConfig) -> Self {
        match value {
            TaskRoutingIntentConfig::Simple => TaskIntent::Simple,
            TaskRoutingIntentConfig::Delegate => TaskIntent::Delegate,
            TaskRoutingIntentConfig::Stream => TaskIntent::Stream,
        }
    }
}

/// Classify a user message against the configured rules and return the
/// matching hint string, if any.
///
/// Returns `None` when classification is disabled, no rules are configured,
/// or no rule matches the message.
pub fn classify(config: &QueryClassificationConfig, message: &str) -> Option<String> {
    if !config.enabled || config.rules.is_empty() {
        return None;
    }

    let lower = message.to_lowercase();
    let len = message.len();

    let mut rules: Vec<_> = config.rules.iter().collect();
    rules.sort_by(|a, b| b.priority.cmp(&a.priority));

    for rule in rules {
        // Length constraints
        if let Some(min) = rule.min_length {
            if len < min {
                continue;
            }
        }
        if let Some(max) = rule.max_length {
            if len > max {
                continue;
            }
        }

        // Check keywords (case-insensitive) and patterns (case-sensitive)
        let keyword_hit = rule
            .keywords
            .iter()
            .any(|kw: &String| lower.contains(&kw.to_lowercase()));
        let pattern_hit = rule
            .patterns
            .iter()
            .any(|pat: &String| message.contains(pat.as_str()));

        if keyword_hit || pattern_hit {
            return Some(rule.hint.clone());
        }
    }

    None
}

pub fn classify_intent(config: &TaskRoutingConfig, message: &str) -> ClassifyResult {
    if !config.enabled {
        return ClassifyResult {
            intent: TaskIntent::Stream,
            model_hint: None,
            reason: "task_routing disabled".to_string(),
        };
    }

    let lower = message.to_lowercase();
    let mut rules: Vec<_> = config.rules.iter().collect();
    rules.sort_by(|a, b| b.priority.cmp(&a.priority));

    for rule in rules {
        let keyword_hit = rule
            .keywords
            .iter()
            .map(|keyword| keyword.trim())
            .filter(|keyword| !keyword.is_empty())
            .any(|keyword| lower.contains(&keyword.to_lowercase()));

        if !keyword_hit {
            continue;
        }

        let intent = TaskIntent::from(rule.intent);
        let model_hint = match intent {
            TaskIntent::Delegate => rule
                .sub_agent_model
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .or_else(|| {
                    rule.model_hint
                        .as_deref()
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(str::to_string)
                }),
            TaskIntent::Simple | TaskIntent::Stream => rule
                .model_hint
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
        };

        return ClassifyResult {
            intent,
            model_hint,
            reason: format!("matched keywords {:?} (priority {})", rule.keywords, rule.priority),
        };
    }

    ClassifyResult {
        intent: TaskIntent::from(config.default_intent),
        model_hint: None,
        reason: "no task_routing rule matched; using default intent".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{
        ClassificationRule, QueryClassificationConfig, TaskRoutingConfig, TaskRoutingIntentConfig,
        TaskRoutingRule,
    };

    fn make_config(enabled: bool, rules: Vec<ClassificationRule>) -> QueryClassificationConfig {
        QueryClassificationConfig { enabled, rules }
    }

    #[test]
    fn disabled_returns_none() {
        let config = make_config(
            false,
            vec![ClassificationRule {
                hint: "fast".into(),
                keywords: vec!["hello".into()],
                ..Default::default()
            }],
        );
        assert_eq!(classify(&config, "hello"), None);
    }

    #[test]
    fn empty_rules_returns_none() {
        let config = make_config(true, vec![]);
        assert_eq!(classify(&config, "hello"), None);
    }

    #[test]
    fn keyword_match_case_insensitive() {
        let config = make_config(
            true,
            vec![ClassificationRule {
                hint: "fast".into(),
                keywords: vec!["hello".into()],
                ..Default::default()
            }],
        );
        assert_eq!(classify(&config, "HELLO world"), Some("fast".into()));
    }

    #[test]
    fn pattern_match_case_sensitive() {
        let config = make_config(
            true,
            vec![ClassificationRule {
                hint: "code".into(),
                patterns: vec!["fn ".into()],
                ..Default::default()
            }],
        );
        assert_eq!(classify(&config, "fn main()"), Some("code".into()));
        assert_eq!(classify(&config, "FN MAIN()"), None);
    }

    #[test]
    fn length_constraints() {
        let config = make_config(
            true,
            vec![ClassificationRule {
                hint: "fast".into(),
                keywords: vec!["hi".into()],
                max_length: Some(10),
                ..Default::default()
            }],
        );
        assert_eq!(classify(&config, "hi"), Some("fast".into()));
        assert_eq!(
            classify(&config, "hi there, how are you doing today?"),
            None
        );

        let config2 = make_config(
            true,
            vec![ClassificationRule {
                hint: "reasoning".into(),
                keywords: vec!["explain".into()],
                min_length: Some(20),
                ..Default::default()
            }],
        );
        assert_eq!(classify(&config2, "explain"), None);
        assert_eq!(
            classify(&config2, "explain how this works in detail"),
            Some("reasoning".into())
        );
    }

    #[test]
    fn priority_ordering() {
        let config = make_config(
            true,
            vec![
                ClassificationRule {
                    hint: "fast".into(),
                    keywords: vec!["code".into()],
                    priority: 1,
                    ..Default::default()
                },
                ClassificationRule {
                    hint: "code".into(),
                    keywords: vec!["code".into()],
                    priority: 10,
                    ..Default::default()
                },
            ],
        );
        assert_eq!(classify(&config, "write some code"), Some("code".into()));
    }

    #[test]
    fn no_match_returns_none() {
        let config = make_config(
            true,
            vec![ClassificationRule {
                hint: "fast".into(),
                keywords: vec!["hello".into()],
                ..Default::default()
            }],
        );
        assert_eq!(classify(&config, "something completely different"), None);
    }

    #[test]
    fn task_routing_uses_priority_and_delegate_model() {
        let config = TaskRoutingConfig {
            enabled: true,
            default_intent: TaskRoutingIntentConfig::Simple,
            rules: vec![
                TaskRoutingRule {
                    keywords: vec!["分析".into()],
                    intent: TaskRoutingIntentConfig::Stream,
                    model_hint: Some("fast".into()),
                    sub_agent_model: None,
                    priority: 1,
                },
                TaskRoutingRule {
                    keywords: vec!["分析".into(), "修复".into()],
                    intent: TaskRoutingIntentConfig::Delegate,
                    model_hint: None,
                    sub_agent_model: Some("claude-opus-4-6".into()),
                    priority: 10,
                },
            ],
        };

        let result = classify_intent(&config, "请分析并修复这个 bug");
        assert_eq!(result.intent, TaskIntent::Delegate);
        assert_eq!(result.model_hint.as_deref(), Some("claude-opus-4-6"));
    }

    #[test]
    fn task_routing_falls_back_to_default_intent() {
        let config = TaskRoutingConfig {
            enabled: true,
            default_intent: TaskRoutingIntentConfig::Simple,
            rules: vec![],
        };

        let result = classify_intent(&config, "今天天气如何");
        assert_eq!(result.intent, TaskIntent::Simple);
        assert!(result.model_hint.is_none());
    }
}
