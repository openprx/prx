//! Owner and visibility guards, access audit log and principal resolution.
use super::pool::PostgresConnectionPool;
use super::{PG_DIALECT, PostgresMemory};
use crate::memory::principal::{ChatType, MemoryWriteContext, Principal, Role, Visibility};
use crate::memory::traits::MemoryPrincipal;
use anyhow::{Context, Result};
use chrono::Utc;
use postgres::GenericClient;
use std::sync::Arc;
use uuid::Uuid;

impl PostgresMemory {
    /// Bind trusted isolation context to the current transaction only. Values
    /// come from resolved runtime principals, never prompt/tool arguments. A
    /// non-system operation without both identifiers fails before SQL executes.
    ///
    /// `owner_isolated` is the caller's `require_owner_match` bit: when set,
    /// the vector RLS policies drop their shared `global`/`workspace` read arm
    /// for this transaction (see [`Self::vector_rls_policy_sql`]).
    pub(super) fn apply_rls_context_raw<C: GenericClient>(
        client: &mut C,
        system: bool,
        workspace_id: Option<&str>,
        owner_id: Option<&str>,
        owner_isolated: bool,
    ) -> Result<()> {
        if system {
            client.execute("SELECT set_config('prx.rls_bypass', 'on', true)", &[])?;
            return Ok(());
        }

        let workspace = workspace_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .context("PostgreSQL vector isolation requires a trusted workspace context")?;
        let owner = owner_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .context("PostgreSQL vector isolation requires a trusted owner context")?;
        client.execute("SELECT set_config('prx.rls_bypass', 'off', true)", &[])?;
        client.execute("SELECT set_config('prx.current_workspace', $1, true)", &[&workspace])?;
        // The owner and its isolation bit are bound in one statement: the
        // policy reads them together, and it saves a round trip.
        let isolation = if owner_isolated { "on" } else { "off" };
        client.execute(
            "SELECT set_config('prx.current_owner', $1, true), set_config('prx.owner_isolated', $2, true)",
            &[&owner, &isolation],
        )?;
        Ok(())
    }

    pub(super) fn document_owner_for_principal(principal: &MemoryPrincipal) -> Option<String> {
        // C03 / V02-R4: an owner-isolated principal's channel/sender pair is the
        // caller-chosen external subject alone, so deriving the document owner
        // from it would let two issuers sharing a subject read each other's
        // private documents. Use the authenticated owner id instead; when it is
        // absent the principal admits no owner-scoped document at all.
        if principal.require_owner_match {
            return crate::memory::owner_predicate::has_owner_id(principal)
                .then(|| principal.owner_id.clone())
                .flatten();
        }
        let channel = principal.channel.as_deref()?.trim();
        let sender = principal.sender.as_deref()?.trim();
        if channel.is_empty() || sender.is_empty() {
            return None;
        }
        Some(
            super::super::principal::OwnerPrincipal::new(
                principal.workspace_id.clone(),
                channel,
                sender,
                principal.session_key.clone().unwrap_or_default(),
                vec![Role::Anonymous],
            )
            .owner_id,
        )
    }

    /// The guard appended to the *visibility* disjunct of every document chunk
    /// search statement (V02-R8). Twin of `SqliteMemory::document_visibility_guard`.
    ///
    /// Those statements admit a chunk when the owning document is `global` or
    /// `workspace` **or** when the chunk's owner matches the caller. The first
    /// disjunct short-circuits the second, and a document's visibility defaults
    /// to `workspace`, so an owner-isolated caller was handed a snippet of every
    /// other owner's documents — including the conversation transcript the
    /// OS-paging evictor ingests verbatim.
    ///
    /// `document_search` renders chunk content to its caller, so it has to agree
    /// with `document_get_chunk`'s `chunk_visible_to_principal`, which is
    /// already the intended semantic: an owner-isolated principal reads a chunk
    /// only when the chunk's `owner_id` equals its authenticated owner id. A
    /// `global` or `workspace` document owned by someone else and an
    /// unattributed chunk are both hidden, and a caller with no owner id reaches
    /// nothing. The owner disjunct beside this guard already expresses exactly
    /// that test, so the whole correction is to stop the visibility disjunct
    /// short-circuiting it.
    ///
    /// The fragment comes from
    /// [`crate::memory::owner_predicate::owner_match_suffix`] — the single
    /// owner-isolation seam — asked for an arm that carries no owner of its own
    /// (`has_owner = false`), which is precisely what this arm is: a document's
    /// visibility proves nothing about who owns the chunk. It therefore renders
    /// the empty string for a non-isolated principal, leaving the statements and
    /// their bind lists byte-identical, and `" AND FALSE"` for an isolated one.
    /// It binds no parameter in either case, so the placeholder accounting of
    /// these statements is unchanged.
    ///
    /// This is a statement-level filter. The row-level security policy on the
    /// chunk table is a second, independent layer and is untouched.
    pub(super) fn document_visibility_guard(principal: &MemoryPrincipal) -> String {
        // Unused: this arm never binds, because it never has an owner to bind.
        const UNBOUND_INDEX: usize = 1;
        crate::memory::owner_predicate::owner_match_suffix(
            PG_DIALECT,
            UNBOUND_INDEX,
            principal.require_owner_match,
            false,
        )
        .sql
    }

    /// The owner id an owner-isolated delete is confined to, with blanks
    /// treated as absent — the same rule
    /// [`Self::isolated_read_without_owner`] applies to the keyed reads, so the
    /// short-circuit and this anchor can never disagree about what "no owner"
    /// means. A non-isolated context never yields one.
    pub(super) fn forget_owner_anchor(context: &MemoryWriteContext) -> Option<String> {
        if !context.require_owner_match {
            return None;
        }
        context
            .owner_id
            .as_deref()
            .map(str::trim)
            .filter(|owner| !owner.is_empty())
            .map(str::to_string)
    }

    /// The guard appended to `forget_with_context`'s visibility scope
    /// (R-B1d D-1).
    ///
    /// Delete-by-key was confined to the shared visibility predicate alone.
    /// That predicate owner-qualifies every **non-public** arm for an isolated
    /// caller, but `visibility = 'public'` stays shared by design — correct for
    /// a read, wrong for a destructive operation, because it let an
    /// owner-isolated caller remove a public row belonging to someone else.
    /// This guard is therefore appended at the top level of the `WHERE` rather
    /// than inside one arm: an owner-isolated caller deletes only rows whose
    /// `owner_id` equals its authenticated owner id, whatever their visibility.
    ///
    /// The fragment comes from
    /// [`crate::memory::owner_predicate::owner_match_suffix`] — the single
    /// owner-isolation seam — so it renders the empty string for a
    /// non-isolated caller (statement and bind list byte-identical) and
    /// `" AND owner_id = $N"` for an isolated one. The `" AND FALSE"` case
    /// cannot be reached here: an isolated context with no usable owner id has
    /// already returned above, before any statement or RLS call. `index` is the
    /// caller's next free placeholder, so the one-marker-one-parameter
    /// accounting is preserved.
    ///
    /// This is a statement-level filter; the row-level security policy on the
    /// memory table is a second, independent layer and is untouched.
    pub(super) fn forget_owner_guard(
        context: &MemoryWriteContext,
        index: usize,
    ) -> crate::memory::owner_predicate::OwnerMatchPredicate {
        crate::memory::owner_predicate::owner_match_suffix(
            PG_DIALECT,
            index,
            context.require_owner_match,
            Self::forget_owner_anchor(context).is_some(),
        )
    }

    pub(super) fn is_system_principal(principal: &MemoryPrincipal) -> bool {
        principal
            .agent_id
            .as_deref()
            .is_some_and(super::super::principal::is_system_principal)
            || principal
                .persona_id
                .as_deref()
                .is_some_and(super::super::principal::is_system_principal)
    }

    /// D4 read-merge: distinct legacy `session_key` candidate(s) bound to *new
    /// trailing* `$N` placeholders. See the SQLite twin
    /// `SqliteMemory::legacy_session_key_params` for the shared contract.
    pub(super) fn legacy_session_key_params(principal: &MemoryPrincipal) -> Vec<String> {
        let mut candidates = principal.session_key_candidates();
        if candidates.is_empty() {
            candidates
        } else {
            candidates.split_off(1)
        }
    }

    /// D4 read-merge: placeholder indices for the canonical key plus trailing
    /// legacy key(s). Mirrors `SqliteMemory::session_indices`.
    /// C03 owner isolation: 1-based placeholder index for the owner parameter.
    ///
    /// The owner id always binds *after* the D4 legacy session keys, so the
    /// index is the first free trailing slot once those are accounted for.
    pub(super) const fn owner_index(legacy_start: usize, legacy_key_count: usize) -> usize {
        legacy_start + legacy_key_count
    }

    pub(super) fn session_indices(
        canonical_index: usize,
        legacy_start: usize,
        principal: &MemoryPrincipal,
        legacy_keys: &[String],
    ) -> Vec<usize> {
        if principal
            .session_key
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
        {
            return Vec::new();
        }
        let mut indices = Vec::with_capacity(1 + legacy_keys.len());
        indices.push(canonical_index);
        for offset in 0..legacy_keys.len() {
            indices.push(legacy_start + offset);
        }
        indices
    }

    /// Whether an owner-isolated read must answer empty without touching the
    /// database (C03 / V02-R5, follow-up F1).
    ///
    /// Under isolation the RLS owner follows `context.owner_id` alone, and a
    /// blank or absent owner id leaves it `None`. `apply_rls_context_raw`
    /// rejects a non-system read that carries no owner, so without this
    /// decision the read would return an *error* where the SQLite adapter
    /// answers with an empty result — the same fail-closed intent, two
    /// different contracts for the caller. The blank rule is the one
    /// [`crate::memory::owner_predicate::owner_match_suffix`] applies to the
    /// SQL arm, so both ends agree on what "no owner" means.
    ///
    /// A non-isolated context is never short-circuited: its read path and its
    /// RLS owner are byte-identical to the historical ones.
    #[must_use]
    pub(super) fn isolated_read_without_owner(context: &MemoryWriteContext) -> bool {
        context.require_owner_match && context.owner_id.as_deref().map(str::trim).is_none_or(str::is_empty)
    }

    pub(super) fn principal_from_context(context: &MemoryWriteContext) -> Principal {
        let current_channel = context.channel.clone().unwrap_or_default();
        let current_chat_id = context.chat_id.clone().unwrap_or_default();
        let current_chat_type = context
            .chat_type
            .as_deref()
            .map(ChatType::from_str)
            .unwrap_or(ChatType::Dm);
        let user_id = context.sender_id.clone().unwrap_or_else(|| {
            let channel = context.channel.as_deref().unwrap_or("unknown");
            let raw_sender = context.raw_sender.as_deref().unwrap_or("unknown");
            format!("anonymous:{channel}:{raw_sender}")
        });
        let role = if context.sender_id.is_some() {
            Role::Member
        } else {
            Role::Anonymous
        };

        Principal {
            user_id,
            role,
            projects: Vec::new(),
            visibility_ceiling: Visibility::Private,
            blocked_patterns: Vec::new(),
            current_channel,
            current_chat_id,
            current_chat_type,
            // FIX-P1-06: carry the raw sender anchor so build_sql_scope can match
            // self-authored rows for anonymous principals.
            raw_sender: context.raw_sender.clone().unwrap_or_default(),
            acl_enforced: true,
            owner_id: None,
            require_owner_match: false,
        }
        .with_owner_scope(context)
    }

    /// FIX-P0-22: append an access-audit row (parity with SQLite `log_access`).
    /// System/owner principals are not audited. Best-effort: a logging failure
    /// must not abort the caller's operation, so errors are traced and dropped.
    pub(super) fn log_access_best_effort_blocking(
        client: &PostgresConnectionPool,
        audit_table: &str,
        principal: &Principal,
        action: &str,
        query: Option<&str>,
        memory_id: Option<&str>,
        policy_rule: Option<&str>,
        result: &str,
    ) {
        if principal.role == Role::Owner {
            return;
        }
        let sql = format!(
            "INSERT INTO {audit_table} \
             (id, timestamp, requester, action, query, memory_id, policy_rule, result) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)"
        );
        let id = Uuid::new_v4().to_string();
        let timestamp = Utc::now().to_rfc3339();
        let outcome = client.with_client(|client| {
            client.execute(
                &sql,
                &[
                    &id,
                    &timestamp,
                    &principal.user_id,
                    &action,
                    &query,
                    &memory_id,
                    &policy_rule,
                    &result,
                ],
            )?;
            Ok(())
        });
        if let Err(error) = outcome {
            tracing::debug!(error = %error, "Postgres access-audit log write failed (best-effort)");
        }
    }

    pub(super) async fn log_access_best_effort(
        client: Arc<PostgresConnectionPool>,
        audit_table: String,
        principal: Principal,
        action: &'static str,
        query: Option<String>,
        memory_id: Option<String>,
        policy_rule: Option<&'static str>,
        result: &'static str,
    ) {
        let outcome = tokio::task::spawn_blocking(move || {
            Self::log_access_best_effort_blocking(
                &client,
                &audit_table,
                &principal,
                action,
                query.as_deref(),
                memory_id.as_deref(),
                policy_rule,
                result,
            );
        })
        .await;
        if let Err(error) = outcome {
            tracing::debug!(error = %error, "Postgres access-audit task failed (best-effort)");
        }
    }

    /// FIX-P0-21 (#1 F1): resolve a full ACL [`Principal`] from a write context
    /// by querying `identity_bindings` + `user_policies` (parity with the SQLite
    /// `resolve_principal`). Falls back to an anonymous principal when no binding
    /// or policy is found, or when the context lacks a channel/raw_sender.
    ///
    /// This is the async PrincipalResolver: it runs the lookups on a blocking
    /// task so the synchronous `postgres` client can be used without blocking the
    /// async runtime.
    pub(super) async fn resolve_principal_from_context(&self, context: &MemoryWriteContext) -> Principal {
        let current_channel = context.channel.clone().unwrap_or_default();
        let current_chat_id = context.chat_id.clone().unwrap_or_default();
        let current_chat_type = context
            .chat_type
            .as_deref()
            .map(ChatType::from_str)
            .unwrap_or(ChatType::Dm);

        // FIX-P1-06: raw sender anchor shared by the fallback and resolved paths.
        let raw_sender_anchor = context.raw_sender.clone().unwrap_or_default();
        let fallback = |user_id: String| {
            Principal {
                user_id,
                role: Role::Anonymous,
                projects: Vec::new(),
                visibility_ceiling: Visibility::Private,
                blocked_patterns: Vec::new(),
                current_channel: current_channel.clone(),
                current_chat_id: current_chat_id.clone(),
                current_chat_type: current_chat_type.clone(),
                raw_sender: raw_sender_anchor.clone(),
                acl_enforced: true,
                owner_id: None,
                require_owner_match: false,
            }
            .with_owner_scope(context)
        };

        let (Some(channel), Some(raw_sender)) = (context.channel.clone(), context.raw_sender.clone()) else {
            let channel = context.channel.as_deref().unwrap_or("unknown");
            let raw_sender = context.raw_sender.as_deref().unwrap_or("unknown");
            return fallback(format!("anonymous:{channel}:{raw_sender}"));
        };

        let client = self.client.clone();
        let bindings_table = self.qualified_identity_bindings_table();
        let policies_table = self.qualified_user_policies_table();
        let lookup_channel = channel.clone();
        let lookup_raw_sender = raw_sender.clone();

        let resolved = tokio::task::spawn_blocking(
            move || -> Result<Option<(String, Option<(String, String, String, String)>)>> {
                client.with_client(|client| {
                    let binding_sql = format!(
                        "SELECT user_id FROM {bindings_table} WHERE channel = $1 AND channel_account = $2 LIMIT 1"
                    );
                    let Some(binding_row) = client.query_opt(&binding_sql, &[&lookup_channel, &lookup_raw_sender])?
                    else {
                        return Ok(None);
                    };
                    let user_id: String = binding_row.get(0);

                    let policy_sql = format!(
                        "SELECT role, projects, visibility_ceiling, blocked_patterns \
                     FROM {policies_table} WHERE user_id = $1 LIMIT 1"
                    );
                    let policy = client.query_opt(&policy_sql, &[&user_id])?.map(|row| {
                        (
                            row.get::<_, String>(0),
                            row.get::<_, String>(1),
                            row.get::<_, String>(2),
                            row.get::<_, String>(3),
                        )
                    });
                    Ok(Some((user_id, policy)))
                })
            },
        )
        .await;

        let lookup = match resolved {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => {
                tracing::debug!(error = %error, "Postgres principal resolution query failed; using anonymous");
                None
            }
            Err(join_error) => {
                tracing::debug!(error = %join_error, "Postgres principal resolution task panicked; using anonymous");
                None
            }
        };

        let Some((user_id, policy)) = lookup else {
            return fallback(format!("anonymous:{channel}:{raw_sender}"));
        };

        super::super::principal::principal_from_policy(
            user_id,
            policy,
            current_channel,
            current_chat_id,
            current_chat_type,
            raw_sender_anchor,
        )
        .with_owner_scope(context)
    }
}
