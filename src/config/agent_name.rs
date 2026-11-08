//! The wire and config grammar of an agent name: `^[a-z][a-z0-9_-]{0,63}$`.
//!
//! The match is exact. There is no trimming, case folding or Unicode
//! normalisation, so one spelling names one agent at every ingress (config
//! load, request body field, decoded path segment).

use serde::{Deserialize, Serialize};

/// The reserved name of the PRX main agent. It is a valid grammar value, but
/// it is never a named agent: `"main"` selects Main and `[agents.main]` is
/// kept for delegate/spawn only.
pub const MAIN_AGENT_NAME: &str = "main";

/// Longest accepted name, in bytes (the grammar is ASCII-only).
pub const AGENT_NAME_MAX_LEN: usize = 64;

/// Why a name was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AgentNameError {
    /// The first byte is missing or is not `a-z`.
    #[error("agent name must start with a lowercase ASCII letter")]
    InvalidStart,
    /// A later byte is not `a-z`, `0-9`, `_` or `-`.
    #[error("agent name may contain only lowercase ASCII letters, digits, '_' and '-'")]
    InvalidCharacter,
    /// The name is longer than [`AGENT_NAME_MAX_LEN`] bytes.
    #[error("agent name is longer than {AGENT_NAME_MAX_LEN} bytes")]
    TooLong,
}

/// A name that satisfies the grammar.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AgentName(String);

impl AgentName {
    /// Parse `raw` with no normalisation.
    ///
    /// # Errors
    ///
    /// The first grammar violation found: start, then characters, then length.
    pub fn parse(raw: &str) -> Result<Self, AgentNameError> {
        let mut bytes = raw.bytes();
        if !bytes.next().is_some_and(|first| first.is_ascii_lowercase()) {
            return Err(AgentNameError::InvalidStart);
        }
        if !bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-') {
            return Err(AgentNameError::InvalidCharacter);
        }
        if raw.len() > AGENT_NAME_MAX_LEN {
            return Err(AgentNameError::TooLong);
        }
        Ok(Self(raw.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is the reserved main-agent name.
    #[must_use]
    pub fn is_main(&self) -> bool {
        self.0 == MAIN_AGENT_NAME
    }
}

impl TryFrom<String> for AgentName {
    type Error = AgentNameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<AgentName> for String {
    fn from(value: AgentName) -> Self {
        value.0
    }
}

/// Whether a configured `[agents.<name>]` key can be targeted as a named
/// agent: it satisfies the grammar and is not the reserved `main`.
#[must_use]
pub fn is_targetable(raw: &str) -> bool {
    AgentName::parse(raw).is_ok_and(|name| !name.is_main())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grammar_accepts_the_documented_shape() {
        for name in ["a", "main", "research-bot_2", &"a".repeat(AGENT_NAME_MAX_LEN)] {
            assert!(AgentName::parse(name).is_ok_and(|parsed| parsed.as_str() == name));
        }
    }

    #[test]
    fn main_is_valid_grammar_but_not_targetable() {
        assert!(AgentName::parse("main").is_ok_and(|name| name.is_main()));
        assert!(!is_targetable("main"));
        assert!(is_targetable("mainframe"));
    }
}
