//! Parameterized owner-isolation predicate fragments (C03 / V02-R4).
//!
//! The `session` and `private` visibility arms of every principal-scoped read
//! admit rows by `session_key` and by `sender`/`agent_id`/`persona_id`. For an
//! externally authenticated MCP caller both of those are derived from the
//! external *subject alone* (`mcp:{subject}`, sender = subject), so two
//! identities that present the same subject under different issuers would share
//! each other's session and private history. Only `owner_id`
//! (`owner:{workspace}:agent:{issuer}:{subject}`) separates them.
//!
//! This module builds the one SQL suffix that closes that gap, so every call
//! site in both adapters shares a single implementation and a single mutation
//! seam. It emits **nothing at all** when the principal does not require owner
//! isolation, which keeps the historical statements byte-identical for channel,
//! chat, cron, xin and sub-agent principals.
//!
//! Iron rule 9 (SQL injection): only placeholder tokens (`?N` / `$N`) are ever
//! interpolated here — the owner id itself is always bound as a parameter by the
//! caller.

use super::session_predicate::PlaceholderDialect;

/// A built owner-isolation suffix plus whether the caller must bind the owner
/// id as a trailing parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerMatchPredicate {
    /// SQL fragment appended *inside* the `session` / `private` visibility arm.
    /// Either empty (no isolation requested), `" AND FALSE"` (isolation
    /// requested with no owner id: fail closed) or `" AND owner_id = <token>"`.
    pub sql: String,
    /// Whether the caller must append the principal's `owner_id` as a bound
    /// parameter at the index this fragment was built with.
    pub bound_owner: bool,
}

/// Build the owner-isolation suffix for one `session` / `private` arm.
///
/// `index` is the 1-based placeholder position the caller will bind the owner
/// id to; it is only consumed when `bound_owner` is `true`.
///
/// Semantics:
/// - `require_owner_match == false` → empty fragment, nothing bound. The
///   statement is byte-identical to the pre-C03 one.
/// - `require_owner_match == true` and `has_owner == false` → `" AND FALSE"`:
///   an owner-isolated principal with no owner id admits no `session` or
///   `private` row at all. Never falls back to sender-only matching.
/// - `require_owner_match == true` and `has_owner == true` →
///   `" AND owner_id = <token>"`. A stored NULL `owner_id` compares to NULL,
///   which is not true, so unattributed legacy rows are excluded without being
///   touched.
#[must_use]
pub fn owner_match_suffix(
    dialect: PlaceholderDialect,
    index: usize,
    require_owner_match: bool,
    has_owner: bool,
) -> OwnerMatchPredicate {
    if !require_owner_match {
        return OwnerMatchPredicate {
            sql: String::new(),
            bound_owner: false,
        };
    }
    if !has_owner {
        return OwnerMatchPredicate {
            sql: " AND FALSE".to_string(),
            bound_owner: false,
        };
    }
    OwnerMatchPredicate {
        sql: format!(" AND owner_id = {}", dialect.token(index)),
        bound_owner: true,
    }
}

/// Whether `principal` carries a usable (non-blank) owner id.
#[must_use]
pub fn has_owner_id(principal: &super::traits::MemoryPrincipal) -> bool {
    principal
        .owner_id
        .as_deref()
        .map(str::trim)
        .is_some_and(|owner| !owner.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryPrincipal;

    #[test]
    fn no_isolation_emits_nothing_and_binds_nothing() {
        let predicate = owner_match_suffix(PlaceholderDialect::Sqlite, 9, false, true);
        assert_eq!(predicate.sql, "");
        assert!(!predicate.bound_owner);
    }

    #[test]
    fn isolation_without_owner_id_fails_closed() {
        let predicate = owner_match_suffix(PlaceholderDialect::Postgres, 9, true, false);
        assert_eq!(predicate.sql, " AND FALSE");
        assert!(
            !predicate.bound_owner,
            "nothing is bound, so the arm can never match by sender alone"
        );
    }

    #[test]
    fn isolation_with_owner_id_binds_the_owner_parameter() {
        let sqlite = owner_match_suffix(PlaceholderDialect::Sqlite, 9, true, true);
        assert_eq!(sqlite.sql, " AND owner_id = ?9");
        assert!(sqlite.bound_owner);
        let postgres = owner_match_suffix(PlaceholderDialect::Postgres, 12, true, true);
        assert_eq!(postgres.sql, " AND owner_id = $12");
        assert!(postgres.bound_owner);
    }

    #[test]
    fn blank_owner_id_does_not_count_as_an_owner() {
        let mut principal = MemoryPrincipal {
            workspace_id: "ws".to_string(),
            owner_id: Some("   ".to_string()),
            ..MemoryPrincipal::default()
        };
        assert!(!has_owner_id(&principal));
        principal.owner_id = None;
        assert!(!has_owner_id(&principal));
        principal.owner_id = Some("owner:ws:agent:issuer:subject".to_string());
        assert!(has_owner_id(&principal));
    }
}
