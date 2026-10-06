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

/// The shared-tier arm of the session-context event read (OWNER-VIS OV-C).
///
/// That read is already confined to one session key, but an owner-isolated
/// caller's key (`mcp:{subject}`) is the caller-chosen subject alone, which
/// two issuers can share — so for such a reader `workspace` rows are
/// owner-qualified with the same `owner_fragment` as the other arms. For
/// every other reader the fragment is empty and the arm is spelled exactly as
/// before, leaving the statement byte-identical. `global` stays shared: it is
/// the explicit cross-workspace tier.
#[must_use]
pub fn session_context_shared_tiers(owner_fragment: &OwnerMatchPredicate) -> String {
    if owner_fragment.sql.is_empty() {
        return "visibility IN ('global', 'workspace')".to_string();
    }
    format!(
        "(visibility = 'global' OR (visibility = 'workspace'{}))",
        owner_fragment.sql
    )
}

/// Why an owner-isolated `memory_store` write was refused (V02-R9).
///
/// `key` is globally unique on both adapters and every scoped writer upserts
/// with `ON CONFLICT(key) DO UPDATE ... owner_id = excluded.owner_id`, so
/// without a guard an owner-isolated caller that names another owner's key
/// overwrites that owner's content and takes the row over. The refusal names
/// no owner. [`Self::KeyNotAvailable`] does reveal that the key exists under
/// someone else — an existence oracle on key names, not on content — which is
/// the accepted cost of keeping keys global (per-owner key namespacing would
/// change what every by-key read returns).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OwnerScopedStoreRefusal {
    /// The key belongs to another owner, or to no owner at all.
    #[error("key '{key}' is not available to this caller")]
    KeyNotAvailable { key: String },
    /// The caller is owner-isolated but carries no usable owner id.
    #[error("an owner-isolated write requires an authenticated owner id")]
    MissingOwner,
    /// The backend stores no owner and cannot confine a write to one.
    #[error("memory backend '{backend}' cannot confine a write to an owner")]
    UnscopedBackend { backend: String },
}

/// The owner an owner-isolated write is confined to, or the refusal that
/// replaces the write.
///
/// `Ok(None)` for a context that is not owner-isolated: its write keeps the
/// historical owner attribution and the historical statement, byte for byte.
/// `Ok(Some(owner))` is the trimmed, authenticated owner id from the
/// service-authored scope, which becomes the stored `owner_id` of the row.
/// A blank or absent owner id refuses the write before any SQL runs, so an
/// isolated caller can never write an unattributed row.
///
/// # Errors
///
/// [`OwnerScopedStoreRefusal::MissingOwner`] for an isolated context with no
/// usable owner id.
pub fn isolated_store_owner(
    context: Option<&super::principal::MemoryWriteContext>,
) -> Result<Option<String>, OwnerScopedStoreRefusal> {
    let Some(context) = context.filter(|context| context.require_owner_match) else {
        return Ok(None);
    };
    context
        .owner_id
        .as_deref()
        .map(str::trim)
        .filter(|owner| !owner.is_empty())
        .map(|owner| Some(owner.to_string()))
        .ok_or(OwnerScopedStoreRefusal::MissingOwner)
}

/// The guard appended to the `ON CONFLICT(key) DO UPDATE SET ...` list of a
/// scoped `memories` upsert (V02-R9).
///
/// Empty for a write that is not owner-isolated, so the historical statement
/// is byte-identical. For an owner-isolated write it confines the update to a
/// row that already belongs to the writer: `excluded.owner_id` is the
/// writer's authenticated owner id ([`isolated_store_owner`]), and a stored
/// `NULL` owner compares to `NULL`, so an unattributed row is not claimable
/// either. When the guard suppresses the update the statement changes zero
/// rows, which the adapter reports as
/// [`OwnerScopedStoreRefusal::KeyNotAvailable`].
///
/// `table` is the column qualifier of the conflicting row: `memories` on
/// SQLite, the schema-qualified table on PostgreSQL. It is never caller input
/// and binds no parameter, so the placeholder accounting is unchanged.
#[must_use]
pub fn store_conflict_guard(dialect: PlaceholderDialect, table: &str, owner_isolated: bool) -> String {
    if !owner_isolated {
        return String::new();
    }
    let excluded = match dialect {
        PlaceholderDialect::Sqlite => "excluded",
        PlaceholderDialect::Postgres => "EXCLUDED",
    };
    format!("\n                     WHERE {table}.owner_id = {excluded}.owner_id")
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

    fn store_context(owner: Option<&str>, isolated: bool) -> crate::memory::principal::MemoryWriteContext {
        crate::memory::principal::MemoryWriteContext {
            owner_id: owner.map(str::to_string),
            require_owner_match: isolated,
            ..crate::memory::principal::MemoryWriteContext::default()
        }
    }

    /// MUTATION GUARD: render the guard for a non-isolated write and the first
    /// assertion goes red; never render it and the isolated rows do.
    #[test]
    fn store_conflict_guard_is_empty_unless_the_write_is_owner_isolated() {
        for dialect in [PlaceholderDialect::Sqlite, PlaceholderDialect::Postgres] {
            assert_eq!(
                store_conflict_guard(dialect, "memories", false),
                "",
                "a non-isolated upsert must stay byte-identical"
            );
        }
        let sqlite = store_conflict_guard(PlaceholderDialect::Sqlite, "memories", true);
        assert_eq!(sqlite.trim(), "WHERE memories.owner_id = excluded.owner_id");
        let postgres = store_conflict_guard(PlaceholderDialect::Postgres, "\"s\".\"memories\"", true);
        assert_eq!(postgres.trim(), "WHERE \"s\".\"memories\".owner_id = EXCLUDED.owner_id");
        for guard in [sqlite, postgres] {
            assert!(
                !guard.contains('?') && !guard.contains('$'),
                "the guard binds nothing: {guard}"
            );
        }
    }

    #[test]
    fn isolated_store_owner_confines_only_isolated_writes_and_refuses_a_blank_owner() {
        assert_eq!(isolated_store_owner(None), Ok(None));
        assert_eq!(
            isolated_store_owner(Some(&store_context(Some("owner-a"), false))),
            Ok(None),
            "a non-isolated write keeps its historical attribution"
        );
        assert_eq!(
            isolated_store_owner(Some(&store_context(Some("  owner-a "), true))),
            Ok(Some("owner-a".to_string()))
        );
        for blank in [None, Some(""), Some("   ")] {
            assert_eq!(
                isolated_store_owner(Some(&store_context(blank, true))),
                Err(OwnerScopedStoreRefusal::MissingOwner),
                "owner {blank:?}"
            );
        }
    }

    /// MUTATION GUARD: render the qualified arm for a non-isolated reader and
    /// the first assertion goes red.
    #[test]
    fn session_context_shared_tiers_are_byte_identical_unless_owner_isolated() {
        for dialect in [PlaceholderDialect::Sqlite, PlaceholderDialect::Postgres] {
            let open = owner_match_suffix(dialect, 9, false, true);
            assert_eq!(
                session_context_shared_tiers(&open),
                "visibility IN ('global', 'workspace')"
            );
        }
        let isolated = owner_match_suffix(PlaceholderDialect::Postgres, 9, true, true);
        assert_eq!(
            session_context_shared_tiers(&isolated),
            "(visibility = 'global' OR (visibility = 'workspace' AND owner_id = $9))"
        );
        let ownerless = owner_match_suffix(PlaceholderDialect::Sqlite, 9, true, false);
        assert_eq!(
            session_context_shared_tiers(&ownerless),
            "(visibility = 'global' OR (visibility = 'workspace' AND FALSE))"
        );
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
