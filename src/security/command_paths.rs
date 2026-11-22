//! Shell argument and substitution checks for confined command execution.
use std::path::Path;

use super::policy::{QuoteState, SecurityPolicy, fold_shell_line_continuations, split_unquoted_segments};

fn is_env_assignment(word: &str) -> bool {
    word.contains('=') && word.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
}

pub(super) enum CommandPathViolation {
    Path(String, crate::security::PathRefusalReason),
    Dynamic(String),
    ActiveSubstitution,
}

impl CommandPathViolation {
    /// One typed refusal for the orchestration validator and the shell backend.
    pub(super) fn into_refusal(self) -> crate::security::PathRefusal {
        use crate::security::{PathRefusal, PathRefusalReason};
        match self {
            Self::Path(path, reason) => PathRefusal::shell(reason, &path),
            Self::Dynamic(token) => PathRefusal::shell(PathRefusalReason::DynamicShellArgument, &token),
            Self::ActiveSubstitution => PathRefusal::shell(PathRefusalReason::ActiveShellSubstitution, ""),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellLexTokenKind {
    Word,
    Redirection,
    CommandSeparator,
}

struct ShellLexToken {
    text: String,
    kind: ShellLexTokenKind,
    dynamic: bool,
    active_substitution: bool,
}

const MAX_NESTED_SHELL_VALIDATION_DEPTH: usize = 4;

pub(super) fn forbidden_path_argument(
    policy: &SecurityPolicy,
    root: &Path,
    command: &str,
) -> Option<CommandPathViolation> {
    forbidden_path_argument_inner(policy, root, command, 0, true)
}

pub(super) fn forbidden_path_argument_inner(
    policy: &SecurityPolicy,
    root: &Path,
    command: &str,
    depth: usize,
    enforce_dynamic: bool,
) -> Option<CommandPathViolation> {
    let folded_command = fold_shell_line_continuations(command);
    for segment in split_unquoted_segments(&folded_command) {
        let tokens = shell_words_and_redirections(&segment);
        let mut index = 0;
        while index < tokens.len() {
            while tokens
                .get(index)
                .is_some_and(|token| token.kind == ShellLexTokenKind::CommandSeparator)
            {
                index += 1;
            }
            while tokens.get(index).is_some_and(|token| {
                token.kind == ShellLexTokenKind::Word && !token.active_substitution && is_env_assignment(&token.text)
            }) {
                index += 1;
            }
            let Some(executable) = tokens.get(index) else {
                break;
            };
            if executable.kind == ShellLexTokenKind::CommandSeparator {
                continue;
            }
            if enforce_dynamic && executable.active_substitution {
                return Some(CommandPathViolation::ActiveSubstitution);
            }
            if enforce_dynamic && executable.dynamic {
                return Some(CommandPathViolation::Dynamic(executable.text.clone()));
            }
            let benign_dynamic_arguments = allows_benign_dynamic_arguments(&executable.text);
            index += 1;
            let arguments_start = index;
            let mut expects_redirection_operand = false;

            while let Some(token) = tokens.get(index) {
                if token.kind == ShellLexTokenKind::CommandSeparator {
                    break;
                }
                if enforce_dynamic && token.active_substitution {
                    return Some(CommandPathViolation::ActiveSubstitution);
                }
                if token.kind == ShellLexTokenKind::Redirection {
                    expects_redirection_operand = true;
                    index += 1;
                    continue;
                }
                if enforce_dynamic
                    && token.dynamic
                    && (expects_redirection_operand || token.text.contains('/') || !benign_dynamic_arguments)
                {
                    return Some(CommandPathViolation::Dynamic(token.text.clone()));
                }
                expects_redirection_operand = false;
                let candidate = token.text.as_str();
                if candidate.is_empty() || candidate.starts_with('-') || candidate.contains("://") {
                    index += 1;
                    continue;
                }
                let looks_like_path = candidate.starts_with('/')
                    || candidate.starts_with("./")
                    || candidate.starts_with("../")
                    || candidate.starts_with("~/")
                    || (candidate.contains('/') && !candidate.chars().any(char::is_whitespace));
                if looks_like_path {
                    if let Err(reason) = policy.check_path_at(root, candidate) {
                        return Some(CommandPathViolation::Path(candidate.to_string(), reason));
                    }
                }
                index += 1;
            }
            if let Some(violation) = nested_wrapper_violation(
                policy,
                root,
                &executable.text,
                tokens.get(arguments_start..index).unwrap_or_default(),
                depth,
                enforce_dynamic,
            ) {
                return Some(violation);
            }
            if tokens
                .get(index)
                .is_some_and(|token| token.kind == ShellLexTokenKind::CommandSeparator)
            {
                index += 1;
            }
        }
    }
    None
}

fn shell_command_basename(executable: &str) -> &str {
    executable.rsplit(['/', '\\']).next().unwrap_or(executable)
}

fn allows_benign_dynamic_arguments(executable: &str) -> bool {
    matches!(shell_command_basename(executable), "echo" | "printf" | "sleep")
}

fn nested_wrapper_violation(
    policy: &SecurityPolicy,
    root: &Path,
    executable: &str,
    arguments: &[ShellLexToken],
    depth: usize,
    enforce_dynamic: bool,
) -> Option<CommandPathViolation> {
    let words = arguments
        .iter()
        .filter(|token| token.kind == ShellLexTokenKind::Word)
        .map(|token| token.text.as_str())
        .collect::<Vec<_>>();
    let base = shell_command_basename(executable);
    let payload = match base {
        "eval" => Some(words.join(" ")),
        "command" => {
            let start = words
                .iter()
                .position(|word| !word.starts_with('-'))
                .unwrap_or(words.len());
            Some(words.get(start..).unwrap_or_default().join(" "))
        }
        "env" => {
            let start = words
                .iter()
                .position(|word| !word.starts_with('-') && !is_env_assignment(word))
                .unwrap_or(words.len());
            Some(words.get(start..).unwrap_or_default().join(" "))
        }
        "sh" | "bash" | "dash" | "zsh" | "ksh" | "fish" => words
            .iter()
            .position(|word| *word == "-c")
            .and_then(|flag| words.get(flag + 1))
            .map(|payload| (*payload).to_string()),
        "python" | "python3" | "perl" | "ruby" | "node" | "php" => {
            let payload = words
                .iter()
                .position(|word| matches!(*word, "-c" | "-e"))
                .and_then(|flag| words.get(flag + 1))?;
            return code_payload_violation(policy, root, payload, enforce_dynamic);
        }
        _ => None,
    }?;
    if payload.is_empty() {
        return None;
    }
    if depth >= MAX_NESTED_SHELL_VALIDATION_DEPTH {
        return enforce_dynamic.then_some(CommandPathViolation::Dynamic(payload));
    }
    forbidden_path_argument_inner(policy, root, &payload, depth + 1, enforce_dynamic)
}

fn code_payload_violation(
    policy: &SecurityPolicy,
    root: &Path,
    payload: &str,
    enforce_dynamic: bool,
) -> Option<CommandPathViolation> {
    if enforce_dynamic && payload.contains('$') {
        return Some(CommandPathViolation::Dynamic(payload.to_string()));
    }
    for candidate in payload
        .split(|character: char| !(character.is_alphanumeric() || matches!(character, '/' | '.' | '_' | '-' | '~')))
    {
        let looks_like_path = candidate.starts_with('/')
            || candidate.starts_with("./")
            || candidate.starts_with("../")
            || candidate.starts_with("~/");
        if looks_like_path {
            if let Err(reason) = policy.check_path_at(root, candidate) {
                return Some(CommandPathViolation::Path(candidate.to_string(), reason));
            }
        }
    }
    None
}

/// Tokenize one already-separated shell segment into quote-aware words and
/// unquoted redirection operators. Redirections are emitted as their own token
/// even when attached to a command/operand (`cat</etc/passwd`), while `<`/`>`
/// inside quotes remain literal word content.
fn shell_words_and_redirections(segment: &str) -> Vec<ShellLexToken> {
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut word_dynamic = false;
    let mut word_active_substitution = false;
    let mut quote = QuoteState::None;
    let mut chars = segment.chars().peekable();

    let flush_word =
        |tokens: &mut Vec<ShellLexToken>, word: &mut String, dynamic: &mut bool, active_substitution: &mut bool| {
            if !word.is_empty() {
                tokens.push(ShellLexToken {
                    text: std::mem::take(word),
                    kind: ShellLexTokenKind::Word,
                    dynamic: *dynamic,
                    active_substitution: *active_substitution,
                });
                *dynamic = false;
                *active_substitution = false;
            }
        };

    while let Some(ch) = chars.next() {
        match quote {
            QuoteState::Single => {
                if ch == '\'' {
                    quote = QuoteState::None;
                } else {
                    word.push(ch);
                }
            }
            QuoteState::Double => match ch {
                '"' => quote = QuoteState::None,
                '$' => {
                    word_dynamic = true;
                    word_active_substitution |= chars.peek().is_some_and(|next| *next == '(');
                    word.push(ch);
                }
                '`' => {
                    word_active_substitution = true;
                    word.push(ch);
                }
                '\\' => {
                    if let Some(escaped) = chars.next() {
                        word.push(escaped);
                    }
                }
                _ => word.push(ch),
            },
            QuoteState::None => match ch {
                '\'' => quote = QuoteState::Single,
                '"' => quote = QuoteState::Double,
                '\\' => {
                    if let Some(escaped) = chars.next() {
                        word.push(escaped);
                    }
                }
                '$' => {
                    word_dynamic = true;
                    word_active_substitution |= chars.peek().is_some_and(|next| *next == '(');
                    word.push(ch);
                }
                '`' => {
                    word_active_substitution = true;
                    word.push(ch);
                }
                '<' | '>' => {
                    flush_word(&mut tokens, &mut word, &mut word_dynamic, &mut word_active_substitution);
                    tokens.push(ShellLexToken {
                        text: ch.to_string(),
                        kind: ShellLexTokenKind::Redirection,
                        dynamic: false,
                        active_substitution: chars.peek().is_some_and(|next| *next == '('),
                    });
                }
                '&' => {
                    flush_word(&mut tokens, &mut word, &mut word_dynamic, &mut word_active_substitution);
                    tokens.push(ShellLexToken {
                        text: ch.to_string(),
                        kind: ShellLexTokenKind::CommandSeparator,
                        dynamic: false,
                        active_substitution: false,
                    });
                }
                _ if ch.is_whitespace() => {
                    flush_word(&mut tokens, &mut word, &mut word_dynamic, &mut word_active_substitution)
                }
                _ => word.push(ch),
            },
        }
    }
    flush_word(&mut tokens, &mut word, &mut word_dynamic, &mut word_active_substitution);
    tokens
}

pub(super) fn contains_active_shell_substitution(command: &str) -> bool {
    shell_words_and_redirections(command)
        .iter()
        .any(|token| token.active_substitution)
}
