//! Parameterized `session_key` read-merge predicate fragments (D4).
//!
//! When a durable `session_key` is migrated to a canonical format, recall must
//! read both the new canonical history and the pre-cutover legacy history as a
//! union (read-merge, never move). This module builds the parameterized SQL
//! fragment for that union from a list of candidate keys
//! (`MemoryPrincipal::session_key_candidates`), so every call site shares one
//! deduplicating, single-key-degrading implementation.
//!
//! Iron rule 9 (SQL injection): only placeholder tokens (`?N` / `$N`) are ever
//! interpolated into SQL here — key *values* are always bound as parameters by
//! the caller. No value is ever formatted into the statement string.

/// SQL placeholder dialect for a backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceholderDialect {
    /// SQLite positional placeholders: `?1`, `?2`, ...
    Sqlite,
    /// Postgres positional placeholders: `$1`, `$2`, ...
    Postgres,
}

impl PlaceholderDialect {
    fn token(self, index: usize) -> String {
        match self {
            Self::Sqlite => format!("?{index}"),
            Self::Postgres => format!("${index}"),
        }
    }
}

/// A built `session_key` predicate fragment plus the placeholder indices it
/// consumed, so the caller can bind the candidate key values in matching order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionKeyPredicate {
    /// SQL boolean fragment, already wrapped in parentheses where needed.
    pub sql: String,
    /// Number of bound key parameters the caller must append (0, 1, or N).
    pub bound_keys: usize,
}

/// Build the `session`-visibility OR-predicate fragment used inside the larger
/// visibility filter (the `OR (visibility = 'session' AND ...)` arm).
///
/// `indices` are the explicit 1-based placeholder positions for each candidate
/// key, in candidate order. The first index is normally an *existing* bound
/// parameter (the canonical key already in the query), and any further indices
/// are *new trailing* placeholders the caller binds the legacy key(s) to.
///
/// Semantics by index count:
/// - 0 indices → `FALSE` (matches nothing; mirrors the legacy
///   `session_key IS NOT NULL AND session_key = <null>` which is never true).
/// - 1 index → `(<token> IS NOT NULL AND session_key = <token>)`
///   — **byte-identical** to the historical single-key fragment.
/// - N indices → `(session_key IN (<token>, <token>, ...))`.
#[must_use]
pub fn session_visibility_or_fragment(dialect: PlaceholderDialect, indices: &[usize]) -> SessionKeyPredicate {
    match indices {
        [] => SessionKeyPredicate {
            sql: "FALSE".to_string(),
            bound_keys: 0,
        },
        [only] => {
            let token = dialect.token(*only);
            SessionKeyPredicate {
                sql: format!("({token} IS NOT NULL AND session_key = {token})"),
                bound_keys: 1,
            }
        }
        many => {
            let tokens = many
                .iter()
                .map(|index| dialect.token(*index))
                .collect::<Vec<_>>()
                .join(", ");
            SessionKeyPredicate {
                sql: format!("(session_key IN ({tokens}))"),
                bound_keys: many.len(),
            }
        }
    }
}

/// Build a top-level `session_key` equality/membership fragment (the
/// `AND session_key = ?N` / `WHERE session_key = ?N` style hard filter).
///
/// `indices` semantics match [`session_visibility_or_fragment`].
///
/// Semantics by index count:
/// - 0 indices → `FALSE`. Callers guard against this case earlier (they
///   early-return an empty result when no `session_key` is present), but the
///   fragment stays well-formed regardless.
/// - 1 index → `session_key = <token>` — **byte-identical** to the legacy
///   single-key filter.
/// - N indices → `session_key IN (<token>, ...)`.
#[must_use]
pub fn session_key_match_fragment(dialect: PlaceholderDialect, indices: &[usize]) -> SessionKeyPredicate {
    match indices {
        [] => SessionKeyPredicate {
            sql: "FALSE".to_string(),
            bound_keys: 0,
        },
        [only] => {
            let token = dialect.token(*only);
            SessionKeyPredicate {
                sql: format!("session_key = {token}"),
                bound_keys: 1,
            }
        }
        many => {
            let tokens = many
                .iter()
                .map(|index| dialect.token(*index))
                .collect::<Vec<_>>()
                .join(", ");
            SessionKeyPredicate {
                sql: format!("session_key IN ({tokens})"),
                bound_keys: many.len(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn or_fragment_single_key_is_legacy_byte_identical_sqlite() {
        let p = session_visibility_or_fragment(PlaceholderDialect::Sqlite, &[5]);
        assert_eq!(p.sql, "(?5 IS NOT NULL AND session_key = ?5)");
        assert_eq!(p.bound_keys, 1);
    }

    #[test]
    fn or_fragment_single_key_is_legacy_byte_identical_postgres() {
        let p = session_visibility_or_fragment(PlaceholderDialect::Postgres, &[4]);
        assert_eq!(p.sql, "($4 IS NOT NULL AND session_key = $4)");
        assert_eq!(p.bound_keys, 1);
    }

    #[test]
    fn or_fragment_two_keys_uses_in_list_with_explicit_indices() {
        // First index is the existing canonical placeholder, second is a new
        // trailing placeholder bound to the legacy key.
        let s = session_visibility_or_fragment(PlaceholderDialect::Sqlite, &[5, 9]);
        assert_eq!(s.sql, "(session_key IN (?5, ?9))");
        assert_eq!(s.bound_keys, 2);
        let p = session_visibility_or_fragment(PlaceholderDialect::Postgres, &[4, 9]);
        assert_eq!(p.sql, "(session_key IN ($4, $9))");
        assert_eq!(p.bound_keys, 2);
    }

    #[test]
    fn or_fragment_zero_keys_is_false() {
        let p = session_visibility_or_fragment(PlaceholderDialect::Sqlite, &[]);
        assert_eq!(p.sql, "FALSE");
        assert_eq!(p.bound_keys, 0);
    }

    #[test]
    fn match_fragment_single_key_is_legacy_byte_identical() {
        let s = session_key_match_fragment(PlaceholderDialect::Sqlite, &[3]);
        assert_eq!(s.sql, "session_key = ?3");
        assert_eq!(s.bound_keys, 1);
        let p = session_key_match_fragment(PlaceholderDialect::Postgres, &[3]);
        assert_eq!(p.sql, "session_key = $3");
        assert_eq!(p.bound_keys, 1);
    }

    #[test]
    fn match_fragment_two_keys_uses_in_list() {
        let s = session_key_match_fragment(PlaceholderDialect::Sqlite, &[3, 9]);
        assert_eq!(s.sql, "session_key IN (?3, ?9)");
        assert_eq!(s.bound_keys, 2);
    }

    #[test]
    fn match_fragment_three_keys_uses_in_list() {
        let p = session_key_match_fragment(PlaceholderDialect::Postgres, &[2, 3, 4]);
        assert_eq!(p.sql, "session_key IN ($2, $3, $4)");
        assert_eq!(p.bound_keys, 3);
    }
}
