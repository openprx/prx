//! Owned asynchronous runtime-statistics transactions; no ordinary pool lease.
use super::PostgresMemory;
use crate::memory::traits::{RouterLearningEvidence, RouterLearningRead, RouterLearningSnapshot};
use anyhow::{Context, Result};
use std::time::Instant;

/// Learning state follows the actual configured event relation, just as Router
/// history does. Old schema-wide tables are intentionally not read or imported:
/// their rows do not identify which configured memory backend produced them.
fn namespace(schema_ident: &str, qualified_message_events_table: &str) -> Result<(String, String)> {
    let encoded = super::router_history::namespace_digest(qualified_message_events_table)?;
    let state = format!("rls_v1_{encoded}");
    let receipts = format!("rlr_v1_{encoded}");
    super::validate_identifier(&state, "Router learning state table")?;
    super::validate_identifier(&receipts, "Router learning receipt table")?;
    Ok((
        format!("{schema_ident}.{}", super::quote_identifier(&state)),
        format!("{schema_ident}.{}", super::quote_identifier(&receipts)),
    ))
}

fn terminal_lock(state: &str, workspace: &str, owner: &str, terminal: &str) -> String {
    format!("{state}:router-terminal:{workspace}:{owner}:{terminal}")
}
fn domain_lock(state: &str, domain: &str) -> String {
    format!("{state}:router-domain:{domain}")
}
fn schema_lock(state: &str) -> String {
    format!("{state}:router-learning-schema-v1")
}

impl PostgresMemory {
    pub(super) async fn router_learning_transaction(
        &self,
        evidence: &RouterLearningEvidence,
        update: Option<(i64, RouterLearningSnapshot)>,
        deadline: Instant,
    ) -> Result<(RouterLearningRead, bool)> {
        evidence.validate()?;
        let fingerprint = evidence.fingerprint()?;
        let domain = evidence.domain();
        let encoded = update.as_ref().map(|(_, next)| next.encode()).transpose()?;
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .context("Router learning deadline expired")?;
        let mut config: tokio_postgres::Config = self
            .client
            .db_url
            .parse()
            .context("Router PostgreSQL configuration invalid")?;
        config.connect_timeout(remaining);
        let timeout_ms = remaining.as_millis().max(1).min(i32::MAX as u128);
        config.options(&format!(
            "{} -c statement_timeout={timeout_ms} -c lock_timeout={timeout_ms}",
            config.get_options().unwrap_or_default()
        ));
        let operation = async {
            let (mut client, connection) = config.connect(tokio_postgres::NoTls).await?;
            tokio::pin!(connection);
            let transaction = async {
                let tx: tokio_postgres::Transaction<'_> = client.transaction().await?;
                let (state, receipts) = namespace(&self.schema_ident, &self.qualified_message_events_table)?;
                let event = &evidence.terminal;
                tx.query_one(
                    "SELECT set_config('prx.rls_bypass','off',true),set_config('prx.current_workspace',$1,true),
                    set_config('prx.current_owner',$2,true),set_config('prx.owner_isolated','on',true)",
                    &[&event.workspace_id, &event.owner_id],
                )
                .await?;
                let sql = format!("SELECT EXISTS(SELECT 1 FROM {events} t JOIN {events} s ON s.event_id=$1
                    WHERE t.event_id=$2 AND t.workspace_id=$3 AND t.owner_id=$4 AND t.session_key=$5 AND t.run_id=$6
                    AND t.event_type='turn.finalized' AND t.role='event' AND t.idempotency_key=$7
                    AND t.raw_payload_json=$8 AND t.content=$9 AND t.source=$10
                    AND t.channel IS NOT DISTINCT FROM $11::TEXT AND t.sender IS NOT DISTINCT FROM $12::TEXT
                    AND s.workspace_id=t.workspace_id AND s.owner_id=t.owner_id AND s.session_key=t.session_key
                    AND s.run_id=t.run_id AND s.source=t.source AND s.channel IS NOT DISTINCT FROM t.channel
                    AND s.sender IS NOT DISTINCT FROM t.sender AND s.role='user' AND s.event_type='message.created'
                    AND s.idempotency_key=('agent_api:' || t.run_id || ':input')
                AND s.visibility='session' AND s.id<t.id AND s.config_generation_id IS NOT DISTINCT FROM t.config_generation_id
                    AND s.config_source_revision IS NOT DISTINCT FROM t.config_source_revision AND t.id=$13 AND t.content_hash IS NOT DISTINCT FROM $14
                    AND t.visibility='session' AND t.recipient=t.session_key AND t.causation_event_id=$1
                    AND t.parent_session_key IS NULL AND t.parent_run_id IS NULL
                    AND t.agent_id IS NULL AND t.persona_id IS NULL AND t.subject_ref_json IS NULL
                    AND t.goal_id IS NULL AND t.correlation_id IS NULL AND t.attempt_id IS NULL AND t.lease_epoch IS NULL
                    AND t.config_generation_id=$15 AND t.config_source_revision=$16
                    AND t.source_ref_json=$17
                    AND s.parent_session_key IS NULL AND s.parent_run_id IS NULL
                    AND s.agent_id IS NULL AND s.persona_id IS NULL AND s.subject_ref_json IS NULL
                    AND s.goal_id IS NULL AND s.correlation_id IS NULL AND s.attempt_id IS NULL AND s.lease_epoch IS NULL
                    AND s.causation_event_id IS NULL AND s.recipient=t.recipient AND s.raw_payload_json IS NOT DISTINCT FROM $20::TEXT
                    AND s.content=$18 AND s.content_hash=$19 AND s.source_ref_json=$17
 FOR SHARE OF t,s)", events=self.qualified_message_events_table);
                let valid: bool = tx
                    .query_one(
                        &sql,
                        &[
                            &evidence.source_event_id,
                            &event.event_id,
                            &event.workspace_id,
                            &event.owner_id,
                            &event.session_key,
                            &evidence.terminal_id(),
                            &event.idempotency_key,
                            &event.raw_payload_json,
                            &event.content,
                            &event.source.as_str(),
                            &event.channel,
                            &event.sender,
                            &event.id,
                            &event.content_hash,
                            &evidence.generation,
                            &evidence.revision,
                            &serde_json::to_string(&event.source)?,
                            &evidence.source_input,
                            &Self::content_hash(&evidence.source_input),
                            &evidence.source_raw_payload,
                        ],
                    )
                    .await?
                    .get(0);
                anyhow::ensure!(valid, "Router learning source or terminal authority mismatch");
                let missing: bool = tx
                    .query_one(
                        "SELECT to_regclass($1) IS NULL OR to_regclass($2) IS NULL",
                        &[&state, &receipts],
                    )
                    .await?
                    .get(0);
                if missing {
                    // Serialize first creation across every domain in this backend; the
                    // transaction lock outlives DDL and releases only once it is visible.
                    let schema_lock = schema_lock(&state);
                    tx.query_one("SELECT pg_advisory_xact_lock(hashtextextended($1,0))", &[&schema_lock])
                        .await?;
                    tx.batch_execute(&format!("CREATE TABLE IF NOT EXISTS {state} (
                    domain TEXT PRIMARY KEY, version BIGINT NOT NULL, snapshot TEXT NOT NULL CHECK(octet_length(snapshot)<=262144));
                    CREATE TABLE IF NOT EXISTS {receipts} (workspace TEXT NOT NULL, owner TEXT NOT NULL,
                    terminal TEXT NOT NULL, domain TEXT NOT NULL, fingerprint TEXT NOT NULL,
                    PRIMARY KEY(workspace,owner,terminal));")).await?;
                }
                // A transaction advisory lock serializes the absent-row case too. The terminal
                // lock also prevents cross-domain replay races; digest collisions only serialize.
                for key in [
                    terminal_lock(
                        &state,
                        &event.workspace_id,
                        event.owner_id.as_deref().unwrap_or_default(),
                        evidence.terminal_id(),
                    ),
                    domain_lock(&state, &domain),
                ] {
                    tx.query_one("SELECT pg_advisory_xact_lock(hashtextextended($1,0))", &[&key])
                        .await?;
                }
                let replay = tx.query_opt(&format!("SELECT CASE WHEN octet_length(domain)=64 THEN domain ELSE '' END,CASE WHEN octet_length(fingerprint)=64 THEN fingerprint ELSE '' END FROM {receipts} WHERE workspace=$1 AND owner=$2 AND terminal=$3"),
                    &[&event.workspace_id,&event.owner_id,&evidence.terminal_id()]).await?;
                if let Some(row) = &replay {
                    anyhow::ensure!(
                        row.get::<_, String>(0) == domain && row.get::<_, String>(1) == fingerprint,
                        "Router learning terminal replay conflicts"
                    );
                }
                let stored = tx
                    .query_opt(
                        &format!(
                            "SELECT version,CASE WHEN octet_length(snapshot)<=262144 THEN snapshot END
                    FROM {state} WHERE domain=$1 FOR UPDATE"
                        ),
                        &[&domain],
                    )
                    .await?;
                let snapshot = stored
                    .map(|row| -> Result<_> {
                        let value: Option<String> = row.get(1);
                        let snapshot =
                            RouterLearningSnapshot::decode(&value.context("Router snapshot exceeds byte bound")?)?;
                        anyhow::ensure!(
                            snapshot.version == row.get::<_, i64>(0),
                            "Router snapshot version mismatch"
                        );
                        Ok(snapshot)
                    })
                    .transpose()?;
                anyhow::ensure!(Instant::now() < deadline, "Router learning deadline expired");
                if replay.is_some() {
                    anyhow::ensure!(snapshot.is_some(), "Router replay snapshot missing");
                    tx.commit().await?;
                    return Ok((
                        RouterLearningRead {
                            snapshot,
                            replayed: true,
                        },
                        false,
                    ));
                }
                if let Some((expected, next)) = update {
                    let version = snapshot.as_ref().map_or(0, |state| state.version);
                    if version != expected {
                        return Ok((
                            RouterLearningRead {
                                snapshot,
                                replayed: false,
                            },
                            true,
                        ));
                    }
                    anyhow::ensure!(
                        expected.checked_add(1) == Some(next.version),
                        "Router next version invalid"
                    );
                    tx.execute(
                        &format!(
                            "INSERT INTO {state}(domain,version,snapshot) VALUES($1,$2,$3)
                        ON CONFLICT(domain) DO UPDATE SET version=excluded.version,snapshot=excluded.snapshot"
                        ),
                        &[&domain, &next.version, &encoded.context("Router update missing")?],
                    )
                    .await?;
                    tx.execute(
                        &format!(
                            "INSERT INTO {receipts}(workspace,owner,terminal,domain,fingerprint) VALUES($1,$2,$3,$4,$5)"
                        ),
                        &[
                            &event.workspace_id,
                            &event.owner_id,
                            &evidence.terminal_id(),
                            &domain,
                            &fingerprint,
                        ],
                    )
                    .await?;
                    anyhow::ensure!(Instant::now() < deadline, "Router learning deadline expired");
                    tx.commit().await?;
                    Ok((
                        RouterLearningRead {
                            snapshot: Some(next),
                            replayed: false,
                        },
                        false,
                    ))
                } else {
                    tx.commit().await?;
                    Ok((
                        RouterLearningRead {
                            snapshot,
                            replayed: false,
                        },
                        false,
                    ))
                }
            };
            tokio::select! {
                result = transaction => result,
                result = &mut connection => { result?; anyhow::bail!("Router PostgreSQL protocol closed") }
            }
        };
        tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), operation)
            .await
            .context("Router learning deadline expired")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(schema: &str, table: &str) -> (String, String) {
        let schema = super::super::quote_identifier(schema);
        let events = format!("{schema}.{}", super::super::quote_identifier(table));
        namespace(&schema, &events).expect("configured private namespace")
    }

    #[test]
    fn learning_tables_and_locks_isolate_actual_configured_event_relations() {
        let (first, first_receipts) = names("prx_scope", "memory_a_message_events");
        let (second, second_receipts) = names("prx_scope", "memory_b_message_events");
        let (other_schema, _) = names("prx_other", "memory_a_message_events");
        assert_ne!(first, second);
        assert_ne!(first_receipts, second_receipts);
        assert_ne!(first, first_receipts);
        assert_ne!(first, other_schema);
        let long_prefix = "a".repeat(62);
        assert_ne!(
            names("prx_scope", &format!("{long_prefix}a")),
            names("prx_scope", &format!("{long_prefix}b")),
            "long relation names differing at the final byte remain distinct"
        );
        assert_eq!(
            names("prx_scope", "memory_a_message_events"),
            (first.clone(), first_receipts)
        );
        assert_ne!(schema_lock(&first), schema_lock(&second));
        assert_ne!(domain_lock(&first, "same-domain"), domain_lock(&second, "same-domain"));
        assert_ne!(
            terminal_lock(&first, "same-workspace", "same-owner", "same-terminal"),
            terminal_lock(&second, "same-workspace", "same-owner", "same-terminal")
        );
        assert_ne!(
            terminal_lock(&first, "same-workspace", "same-owner", "same-terminal"),
            terminal_lock(&first, "same-workspace", "same-owner", "another-terminal")
        );
        assert_ne!(domain_lock(&first, "domain-a"), domain_lock(&first, "domain-b"));
        assert!(!first.contains("router_learning_state"));
    }

    #[test]
    fn private_learning_names_keep_full_digest_without_postgres_identifier_truncation() {
        let events = format!(
            "{}.{}",
            super::super::quote_identifier("prx_scope"),
            super::super::quote_identifier("memory_a_message_events")
        );
        let digest = super::super::router_history::namespace_digest(&events).unwrap();
        assert_eq!(digest.len(), 52, "full SHA256 base32, not a truncated identifier hash");
        let (state, receipts) = names("prx_scope", "memory_a_message_events");
        for (qualified, prefix) in [(state, "rls_v1_"), (receipts, "rlr_v1_")] {
            let identifier = format!("{prefix}{digest}");
            assert!(identifier.len() <= 63);
            super::super::validate_identifier(&identifier, "fixture identifier").unwrap();
            assert_eq!(
                qualified,
                format!("\"prx_scope\".{}", super::super::quote_identifier(&identifier))
            );
        }
    }
}
