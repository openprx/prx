//! Owned asynchronous PostgreSQL sessions for bounded ingress recovery.
//! Pool checkout is intentionally excluded: the ordinary pool has no deadline.
use super::PostgresMemory;
use crate::memory::ingress_recovery::{
    AtomicIngressMutation, IngressCapsule, IngressEventLookup, IngressLifetime, IngressRecoveryCandidate,
    IngressRecoveryScan, IngressRecoveryScope, IngressRecoveryState, MAX_CAPSULE_BYTES, MAX_ID_BYTES,
    MAX_RECOVERY_EVENT_BYTES, MAX_RECOVERY_PAGE, input_within_bounds, remaining, serialized_within,
};
use crate::memory::{MessageEvent, MessageEventInput};
use anyhow::Context;
use std::sync::Arc;
use std::time::Instant;
use tokio_postgres::{Client, NoTls};

struct Session {
    client: Client,
    driver: tokio::task::JoinHandle<()>,
    lost: tokio_util::sync::CancellationToken,
}
impl Drop for Session {
    fn drop(&mut self) {
        self.lost.cancel();
        self.driver.abort();
    }
}
struct SessionDeadline {
    timer: tokio::task::JoinHandle<()>,
    abort: tokio::task::AbortHandle,
    lost: tokio_util::sync::CancellationToken,
    complete: bool,
}
impl SessionDeadline {
    fn new(session: &Session, deadline: Instant) -> Self {
        let abort = session.driver.abort_handle();
        let lost = session.lost.clone();
        let timer_abort = abort.clone();
        let timer_lost = lost.clone();
        let timer = tokio::spawn(async move {
            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
            timer_lost.cancel();
            timer_abort.abort();
        });
        Self {
            timer,
            abort,
            lost,
            complete: false,
        }
    }
    fn complete(&mut self) {
        self.complete = true;
        self.timer.abort();
    }
}
impl Drop for SessionDeadline {
    fn drop(&mut self) {
        self.timer.abort();
        if !self.complete {
            self.lost.cancel();
            self.abort.abort();
        }
    }
}
async fn bounded<T>(
    deadline: Instant,
    future: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    remaining(deadline)?;
    tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), future)
        .await
        .context("PostgreSQL ingress deadline elapsed")?
}
async fn connect(url: &str, deadline: Instant) -> anyhow::Result<Session> {
    let mut config: tokio_postgres::Config = url.parse().context("invalid PostgreSQL ingress connection")?;
    config.connect_timeout(remaining(deadline)?);
    let (client, connection) = bounded(deadline, async { Ok(config.connect(NoTls).await?) }).await?;
    let lost = tokio_util::sync::CancellationToken::new();
    let signal = lost.clone();
    let driver = tokio::spawn(async move {
        let result = connection.await;
        signal.cancel();
        if result.is_err() {
            tracing::warn!("PostgreSQL ingress session lost");
        }
    });
    let session = Session { client, driver, lost };
    bounded(deadline, async {
        session
            .client
            .batch_execute(super::PostgresSessionContext::System.reset_statement())
            .await?;
        Ok(())
    })
    .await?;
    Ok(session)
}
fn lock_key(namespace: &str, workspace: &str, token: &str) -> i64 {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    for value in [namespace, workspace, token] {
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value.as_bytes());
    }
    let bytes = digest.finalize();
    let mut prefix = [0u8; 8];
    for (target, source) in prefix.iter_mut().zip(bytes) {
        *target = source;
    }
    i64::from_be_bytes(prefix)
}
struct Lease {
    token: String,
    backend_instance: uuid::Uuid,
    relation_namespace: String,
    database_namespace: String,
    workspace: String,
    table: String,
    session: tokio::sync::Mutex<Session>,
    lost: tokio_util::sync::CancellationToken,
    recovery: bool,
}
#[async_trait::async_trait]
impl IngressLifetime for Lease {
    fn token(&self) -> &str {
        &self.token
    }
    fn namespace(&self) -> &str {
        &self.database_namespace
    }
    fn loss_signal(&self) -> Option<tokio_util::sync::CancellationToken> {
        Some(self.lost.clone())
    }
    async fn validate(&self, deadline: Instant) -> anyhow::Result<()> {
        bounded(deadline, async {
            let session = self.session.lock().await;
            let mut deadline_guard = SessionDeadline::new(&session, deadline);
            let row = session
                .client
                .query_opt(
                    &format!(
                        "SELECT active FROM {} WHERE namespace=$1 AND workspace_id=$2 AND token=$3",
                        self.table
                    ),
                    &[&self.relation_namespace, &self.workspace, &self.token],
                )
                .await?
                .context("PostgreSQL ingress lifetime is missing")?;
            anyhow::ensure!(
                row.get::<_, bool>(0) != self.recovery,
                "PostgreSQL ingress lifetime was fenced"
            );
            deadline_guard.complete();
            Ok(())
        })
        .await
    }
}
impl PostgresMemory {
    fn ingress_instance(&self) -> uuid::Uuid {
        *self.ingress_owner_id.get_or_init(uuid::Uuid::new_v4)
    }
    fn ingress_recovery_table(&self) -> String {
        format!("{}.prx_ingress_recovery", self.schema_ident)
    }
    fn ingress_lifetime_table(&self) -> String {
        format!("{}.prx_ingress_lifetimes", self.schema_ident)
    }
    async fn ingress_session(&self, deadline: Instant) -> anyhow::Result<Session> {
        connect(&self.client.db_url, deadline).await
    }
    async fn prepare_ingress_schema(&self, session: &Session, deadline: Instant) -> anyhow::Result<()> {
        let table = self.ingress_recovery_table();
        let lifetimes = self.ingress_lifetime_table();
        let backends = format!("{}.prx_ingress_backends", self.schema_ident);
        bounded(deadline,async {
            let mut deadline_guard=SessionDeadline::new(session,deadline);
            let millis=remaining(deadline)?.as_millis().clamp(1,i32::MAX as u128).to_string();
            session.client.execute("SELECT set_config('statement_timeout',$1,false),set_config('lock_timeout',$1,false)",&[&millis]).await?;
            let exists:bool=session.client.query_one("SELECT to_regclass($1) IS NOT NULL AND to_regclass($2) IS NOT NULL AND to_regclass($3) IS NOT NULL",&[&table,&lifetimes,&backends]).await?.get(0);
            if !exists {
                session.client.batch_execute("BEGIN").await?;
                let initialization_key=lock_key(&self.schema_ident,"prx-ingress-schema","v1");
                session.client.query_one("SELECT pg_advisory_xact_lock($1)",&[&initialization_key]).await?;
                session.client.batch_execute(&format!("CREATE TABLE IF NOT EXISTS {backends}(namespace TEXT PRIMARY KEY,identity TEXT NOT NULL CHECK(octet_length(identity)=36),version INTEGER NOT NULL CHECK(version=1));
CREATE TABLE IF NOT EXISTS {lifetimes}(namespace TEXT NOT NULL,workspace_id TEXT NOT NULL,token TEXT NOT NULL,active BOOLEAN NOT NULL,PRIMARY KEY(namespace,workspace_id,token));
CREATE TABLE IF NOT EXISTS {table}(id BIGSERIAL PRIMARY KEY,namespace TEXT NOT NULL,workspace_id TEXT NOT NULL,owner_id TEXT NOT NULL,source TEXT NOT NULL,token TEXT NOT NULL,input_event_id TEXT NOT NULL,capsule TEXT NOT NULL CHECK(octet_length(capsule)<=16384),status TEXT NOT NULL CHECK(status IN ('pending','unconfirmed','reconciled')),UNIQUE(namespace,workspace_id,input_event_id));
CREATE INDEX IF NOT EXISTS prx_ingress_pending ON {table}(namespace,workspace_id,owner_id,source,status,token,id);
COMMIT;")).await?;
            }
            deadline_guard.complete();Ok(())
        }).await
    }
    pub(super) async fn new_ingress_lifetime(
        &self,
        workspace: &str,
        token: &str,
        deadline: Instant,
        recovery: bool,
    ) -> anyhow::Result<Option<Arc<dyn IngressLifetime>>> {
        anyhow::ensure!(
            !workspace.is_empty() && workspace.len() <= MAX_ID_BYTES && uuid::Uuid::parse_str(token).is_ok(),
            "invalid ingress lifetime scope"
        );
        let session = self.ingress_session(deadline).await?;
        self.prepare_ingress_schema(&session, deadline).await?;
        let table = self.ingress_lifetime_table();
        let backends = format!("{}.prx_ingress_backends", self.schema_ident);
        let database_namespace:String=bounded(deadline,async {
            session.client.execute(&format!("INSERT INTO {backends}(namespace,identity,version) VALUES($1,$2,1) ON CONFLICT DO NOTHING"),&[&self.qualified_message_events_table,&uuid::Uuid::new_v4().to_string()]).await?;
            let row=session.client.query_one(&format!("SELECT CASE WHEN octet_length(identity)<=36 THEN identity ELSE NULL END,version FROM {backends} WHERE namespace=$1"),&[&self.qualified_message_events_table]).await?;
            anyhow::ensure!(row.get::<_,i32>(1)==1,"unsupported PostgreSQL ingress schema");
            row.get::<_,Option<String>>(0).context("oversized PostgreSQL ingress backend identity")
        }).await?;
        anyhow::ensure!(
            uuid::Uuid::parse_str(&database_namespace).is_ok(),
            "invalid PostgreSQL ingress backend identity"
        );
        let key = lock_key(&self.qualified_message_events_table, workspace, token);
        let acquired = bounded(deadline, async {
            let sql = if recovery {
                "SELECT pg_try_advisory_lock($1)"
            } else {
                "SELECT pg_try_advisory_lock_shared($1)"
            };
            Ok(session.client.query_one(sql, &[&key]).await?.get::<_, bool>(0))
        })
        .await?;
        if !acquired {
            return Ok(None);
        }
        bounded(deadline, async {
            if recovery {
                let changed = session
                    .client
                    .execute(
                        &format!("UPDATE {table} SET active=false WHERE namespace=$1 AND workspace_id=$2 AND token=$3"),
                        &[&self.qualified_message_events_table, &workspace, &token],
                    )
                    .await?;
                anyhow::ensure!(changed == 1, "unknown PostgreSQL ingress lifetime");
            } else {
                session
                    .client
                    .execute(
                        &format!("INSERT INTO {table}(namespace,workspace_id,token,active) VALUES($1,$2,$3,true)"),
                        &[&self.qualified_message_events_table, &workspace, &token],
                    )
                    .await?;
            }
            Ok(())
        })
        .await?;
        let lease = Arc::new(Lease {
            token: token.to_owned(),
            backend_instance: self.ingress_instance(),
            relation_namespace: self.qualified_message_events_table.clone(),
            database_namespace,
            workspace: workspace.to_owned(),
            table,
            lost: session.lost.clone(),
            session: tokio::sync::Mutex::new(session),
            recovery,
        });
        let mut registry = leases().lock();
        registry.retain(|entry| entry.strong_count() > 0);
        anyhow::ensure!(registry.len() < 128, "PostgreSQL ingress lifetime capacity exhausted");
        registry.push(Arc::downgrade(&lease));
        Ok(Some(lease))
    }
    pub(super) async fn ingress_candidates(
        &self,
        scope: &IngressRecoveryScope,
        current: Arc<dyn IngressLifetime>,
        phase: IngressRecoveryScan,
        after_id: i64,
        limit: usize,
        deadline: Instant,
    ) -> anyhow::Result<Vec<IngressRecoveryCandidate>> {
        anyhow::ensure!(
            (1..=MAX_RECOVERY_PAGE).contains(&limit)
                && after_id >= 0
                && !scope.workspace_id.is_empty()
                && !scope.owner_id.is_empty()
                && scope.workspace_id.len() <= MAX_ID_BYTES
                && scope.owner_id.len() <= MAX_ID_BYTES
                && !matches!(scope.source, crate::memory::MessageEventSource::Other(_)),
            "invalid ingress recovery query"
        );
        let lease = reading_lease(
            &self.qualified_message_events_table,
            self.ingress_instance(),
            &scope.workspace_id,
        )?;
        let erased: Arc<dyn IngressLifetime> = lease.clone();
        anyhow::ensure!(
            Arc::ptr_eq(&erased, &current),
            "recovery scan lifetime is not the issued current owner"
        );
        let other_state = match phase {
            IngressRecoveryScan::PendingOnly => "pending",
            IngressRecoveryScan::IncludeUnconfirmed => "unconfirmed",
        };
        let session = bounded(deadline, async { Ok(lease.session.lock().await) }).await?;
        bounded(deadline,async {
            let mut deadline_guard=SessionDeadline::new(&session,deadline);
            let table=self.ingress_recovery_table();
            let projection="id, CASE WHEN octet_length(capsule)<=16384 THEN capsule ELSE NULL END AS capsule";
            let sql=format!("SELECT * FROM (
                SELECT {projection} FROM {table} WHERE namespace=$1 AND workspace_id=$2 AND owner_id=$3 AND source=$4 AND status IN ('pending',$8) AND token<$7 AND id>$5
                UNION ALL
                SELECT {projection} FROM {table} WHERE namespace=$1 AND workspace_id=$2 AND owner_id=$3 AND source=$4 AND status IN ('pending',$8) AND token>$7 AND id>$5
                ) candidates ORDER BY id LIMIT $6");
            let rows=session.client.query(&sql,&[&self.qualified_message_events_table,&scope.workspace_id,&scope.owner_id,&scope.source.as_str(),&after_id,&(limit as i64),&current.token(),&other_state]).await?;
            let result=rows.into_iter().map(|row| {
                let encoded:Option<String>=row.get(1);
                Ok(IngressRecoveryCandidate {cursor:row.get(0),capsule:IngressCapsule::decode(&encoded.context("oversized ingress capsule")?)?})
            }).collect();
            deadline_guard.complete();
            result
        }).await
    }
}

fn leases() -> &'static parking_lot::Mutex<Vec<std::sync::Weak<Lease>>> {
    static LEASES: std::sync::OnceLock<parking_lot::Mutex<Vec<std::sync::Weak<Lease>>>> = std::sync::OnceLock::new();
    LEASES.get_or_init(Default::default)
}
fn issued_lease(
    value: &Arc<dyn IngressLifetime>,
    namespace: &str,
    backend_instance: uuid::Uuid,
    capsule: &IngressCapsule,
    recovery: bool,
) -> anyhow::Result<Arc<Lease>> {
    let mut registry = leases().lock();
    registry.retain(|entry| entry.strong_count() > 0);
    registry
        .iter()
        .filter_map(std::sync::Weak::upgrade)
        .find(|lease| {
            let erased: Arc<dyn IngressLifetime> = lease.clone();
            Arc::ptr_eq(&erased, value)
                && lease.backend_instance == backend_instance
                && lease.relation_namespace == namespace
                && lease.workspace == capsule.scope.workspace_id
                && lease.database_namespace == capsule.backend_namespace
                && lease.token == capsule.lifetime
                && lease.recovery == recovery
        })
        .context("PostgreSQL ingress lease was not issued by this backend")
}

impl PostgresMemory {
    pub(super) async fn ingress_append_atomic(
        &self,
        input: MessageEventInput,
        mutation: AtomicIngressMutation,
    ) -> anyhow::Result<(MessageEvent, bool)> {
        let deadline = match &mutation {
            AtomicIngressMutation::Input { deadline, .. } => *deadline,
            AtomicIngressMutation::Recovery(commit) => commit.deadline,
        };
        self.append_bounded(input, Some(mutation), deadline).await
    }
    pub(super) async fn ingress_repair_append(
        &self,
        input: MessageEventInput,
        deadline: Instant,
    ) -> anyhow::Result<MessageEvent> {
        self.append_bounded(input, None, deadline).await.map(|(event, _)| event)
    }
    async fn append_bounded(
        &self,
        input: MessageEventInput,
        mutation: Option<AtomicIngressMutation>,
        deadline: Instant,
    ) -> anyhow::Result<(MessageEvent, bool)> {
        remaining(deadline)?;
        input_within_bounds(&input, MAX_RECOVERY_EVENT_BYTES)?;
        input.validate()?;
        let recovery = matches!(&mutation, Some(AtomicIngressMutation::Recovery(_)));
        let lease = if let Some(operation) = &mutation {
            let capsule = operation.capsule();
            capsule.validate_bounds()?;
            serialized_within(capsule, MAX_CAPSULE_BYTES)?;
            anyhow::ensure!(
                input.workspace_id == capsule.scope.workspace_id
                    && input.owner_id.as_deref() == Some(capsule.scope.owner_id.as_str())
                    && input.source == capsule.scope.source,
                "ingress transaction scope mismatch"
            );
            match operation {
                AtomicIngressMutation::Input { .. } => {
                    anyhow::ensure!(
                        input.role == "user"
                            && input.event_type == "message.created"
                            && input.event_id.as_deref() == Some(capsule.input_event_id.as_str())
                            && input.idempotency_key.as_deref() == Some(capsule.input_key.as_str())
                            && input.run_id.as_deref() == Some(capsule.run_id.as_str()),
                        "invalid atomic ingress input identity"
                    );
                    let wrapper: serde_json::Value = serde_json::from_str(
                        input
                            .raw_payload_json
                            .as_deref()
                            .context("ingress capsule wrapper missing")?,
                    )?;
                    anyhow::ensure!(
                        wrapper.get("prx_ingress_capsule") == Some(&serde_json::to_value(capsule)?)
                            && wrapper
                                .get("original_payload_json")
                                .is_some_and(|value| value.is_null() || value.is_string()),
                        "ingress capsule wrapper mismatch"
                    );
                }
                AtomicIngressMutation::Recovery(commit) => {
                    let (state, event_type) = match commit.state {
                        IngressRecoveryState::Unconfirmed => ("unconfirmed", "turn.ingress.interrupted"),
                        IngressRecoveryState::Reconciled => ("reconciled", "turn.ingress.reconciled"),
                        IngressRecoveryState::Pending => anyhow::bail!("recovery cannot restore pending authority"),
                    };
                    let key = format!("ingress:{}:{state}", capsule.input_event_id);
                    anyhow::ensure!(
                        input.idempotency_key.as_deref() == Some(key.as_str())
                            && input.run_id.as_deref() == Some(capsule.run_id.as_str())
                            && input.causation_event_id.as_deref() == Some(capsule.input_event_id.as_str())
                            && input.role == "event"
                            && input.event_type == event_type,
                        "invalid ingress recovery evidence identity"
                    );
                    let payload: serde_json::Value = serde_json::from_str(
                        input
                            .raw_payload_json
                            .as_deref()
                            .context("ingress recovery payload missing")?,
                    )?;
                    let binding = payload
                        .get("ingress_recovery")
                        .context("ingress recovery binding missing")?;
                    anyhow::ensure!(
                        binding.get("capsule") == Some(&serde_json::to_value(capsule)?)
                            && binding.get("state").and_then(serde_json::Value::as_str) == Some(state)
                            && binding
                                .get("evidence_authentication")
                                .and_then(serde_json::Value::as_str)
                                .is_some_and(
                                    |value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
                                ),
                        "invalid ingress recovery evidence binding"
                    );
                }
            }
            let value = match operation {
                AtomicIngressMutation::Input { lifetime, .. } => lifetime,
                AtomicIngressMutation::Recovery(commit) => &commit.claim.lifetime,
            };
            issued_lease(
                value,
                &self.qualified_message_events_table,
                self.ingress_instance(),
                capsule,
                recovery,
            )?
        } else {
            reading_lease(
                &self.qualified_message_events_table,
                self.ingress_instance(),
                &input.workspace_id,
            )?
        };
        lease.validate(deadline).await?;
        let expected_input = input.clone();
        let mut session = bounded(deadline, async { Ok(lease.session.lock().await) }).await?;
        let qualified_message_events_table = self.qualified_message_events_table.clone();
        let qualified_memory_events_table = self.qualified_memory_events_table.clone();
        let table = self.ingress_recovery_table();
        let lifetime_table = self.ingress_lifetime_table();
        bounded(deadline,async {
            let mut deadline_guard=SessionDeadline::new(&session,deadline);
            let now = chrono::Utc::now();
            let event_id = input.event_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let content_hash = Self::content_hash(&input.content);
            let visibility = input.visibility.as_str().to_string();
            let source = input.source.as_str().to_string();
            let source_ref_json = serde_json::to_string(&input.source)?;
            let subject_ref_json = input.subject.as_ref().map(serde_json::to_string).transpose()?;
            let config_generation_id = input.config_generation_id.map(i64::try_from).transpose()?;

            let insert_stmt = format!(
                "
                INSERT INTO {qualified_message_events_table} (
                    event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                    parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                    sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                    goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                    content, content_hash, raw_payload_json, visibility, created_at, updated_at
                )
                VALUES (
                    $1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
                    $11, $12, $13, $14, $15, $16, $17, $18, $19, $20,
                    $21, $22, $23, $24, $25, $26, $27, $28, $29, $30,
                    $31
                )
                ON CONFLICT DO NOTHING
                "
            );
            let select_stmt = format!(
                "
                SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                       parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                       sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                       goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                       content, content_hash, raw_payload_json, visibility, created_at, updated_at
                FROM {qualified_message_events_table}
                WHERE event_id = $1
                   OR ($2::TEXT IS NOT NULL AND workspace_id = $3 AND idempotency_key = $2)
                ORDER BY CASE WHEN event_id = $1 THEN 0 ELSE 1 END
                LIMIT 1
                "
            );
            let outbox_stmt = format!(
                "
                INSERT INTO {qualified_memory_events_table} (
                    event_id, workspace_id, owner_id, event_type, subject_table, subject_id, session_key,
                    agent_id, persona_id, visibility, payload_json, created_at
                )
                VALUES ($1, $2, $3, $4, 'message_events', $5, $6, $7, $8, $9, NULL, $10)
                ON CONFLICT (event_id) DO NOTHING
                "
            );


            let tx: tokio_postgres::Transaction<'_> = session.client.transaction().await?;
            // A normal writer retains a transaction lock even if its lifetime
            // session is lost, so a recoverer cannot fence a running commit.
            let key=lock_key(&self.qualified_message_events_table,&lease.workspace,&lease.token);
            if !recovery {
                let acquired:bool=tx.query_one("SELECT pg_try_advisory_xact_lock_shared($1)",&[&key]).await?.get(0);
                anyhow::ensure!(acquired,"ingress lifetime is being recovered");
            }
            let active:bool=tx.query_one(&format!("SELECT active FROM {lifetime_table} WHERE namespace=$1 AND workspace_id=$2 AND token=$3 FOR SHARE"),&[&self.qualified_message_events_table,&lease.workspace,&lease.token]).await?.get(0);
            anyhow::ensure!(active != recovery,"ingress writer was fenced");
                let inserted = tx.execute(
                    &insert_stmt,
                    &[
                        &event_id,
                        &input.idempotency_key,
                        &input.workspace_id,
                        &input.owner_id,
                        &source,
                        &input.channel,
                        &input.session_key,
                        &input.parent_session_key,
                        &input.run_id,
                        &input.parent_run_id,
                        &input.agent_id,
                        &input.persona_id,
                        &input.sender,
                        &input.recipient,
                        &input.role,
                        &input.event_type,
                        &source_ref_json,
                        &subject_ref_json,
                        &input.goal_id,
                        &input.causation_event_id,
                        &input.correlation_id,
                        &input.attempt_id,
                        &input.lease_epoch,
                        &config_generation_id,
                        &input.config_source_revision,
                        &input.content,
                        &content_hash,
                        &input.raw_payload_json,
                        &visibility,
                        &now,
                        &now,
                    ],
                ).await?;
                let stored_size:i32=tx.query_one(&format!("SELECT octet_length(e::text) FROM {qualified_message_events_table} e WHERE event_id=$1 OR ($2::TEXT IS NOT NULL AND workspace_id=$3 AND idempotency_key=$2) ORDER BY CASE WHEN event_id=$1 THEN 0 ELSE 1 END LIMIT 1"),&[&event_id,&input.idempotency_key,&input.workspace_id]).await?.get(0);
                anyhow::ensure!(stored_size>=0 && stored_size as usize<=MAX_RECOVERY_EVENT_BYTES,"ingress append receipt exceeds byte budget");
                let row = tx.query_one(&select_stmt, &[&event_id, &input.idempotency_key, &input.workspace_id]).await?;
                let event = Self::row_to_message_event(&row)?;
                if inserted > 0 {
                    tx.execute(
                        &outbox_stmt,
                        &[
                            &uuid::Uuid::new_v4().to_string(),
                            &event.workspace_id,
                            &event.owner_id,
                            &event.event_type,
                            &event.event_id,
                            &event.session_key,
                            &event.agent_id,
                            &event.persona_id,
                            &event.visibility.as_str(),
                            &chrono::Utc::now(),
                        ],
                    ).await?;
                }

            anyhow::ensure!(event.workspace_id==expected_input.workspace_id && event.owner_id==expected_input.owner_id
                && event.source==expected_input.source && event.idempotency_key==expected_input.idempotency_key,
                "ingress transaction returned a different owned event");
            if recovery {
                anyhow::ensure!(event.content==expected_input.content && event.raw_payload_json==expected_input.raw_payload_json
                    && event.run_id==expected_input.run_id && event.event_type==expected_input.event_type
                    && event.channel==expected_input.channel
                    && event.session_key==expected_input.session_key
                    && event.parent_session_key==expected_input.parent_session_key
                    && event.parent_run_id==expected_input.parent_run_id
                    && event.agent_id==expected_input.agent_id
                    && event.persona_id==expected_input.persona_id
                    && event.sender==expected_input.sender
                    && event.recipient==expected_input.recipient
                    && event.role==expected_input.role
                    && event.subject==expected_input.subject
                    && event.goal_id==expected_input.goal_id
                    && event.causation_event_id==expected_input.causation_event_id
                    && event.correlation_id==expected_input.correlation_id
                    && event.attempt_id==expected_input.attempt_id
                    && event.lease_epoch==expected_input.lease_epoch
                    && event.config_generation_id==expected_input.config_generation_id
                    && event.config_source_revision==expected_input.config_source_revision
                    && event.visibility==expected_input.visibility,
                    "ingress evidence idempotency conflict");
            }
            if let Some(operation)=&mutation {
            let capsule=operation.capsule();
            match operation {
                AtomicIngressMutation::Input { .. } => {
                    if inserted>0 {
                        anyhow::ensure!(event.event_id==capsule.input_event_id && event.idempotency_key.as_deref()==Some(capsule.input_key.as_str()) && event.run_id.as_deref()==Some(capsule.run_id.as_str()),"ingress input identity changed");
                        let encoded=serde_json::to_string(capsule)?;
                        anyhow::ensure!(encoded.len()<=MAX_CAPSULE_BYTES,"oversized ingress capsule");
                        tx.execute(&format!("INSERT INTO {table}(namespace,workspace_id,owner_id,source,token,input_event_id,capsule,status) VALUES($1,$2,$3,$4,$5,$6,$7,'pending')"),&[&self.qualified_message_events_table,&capsule.scope.workspace_id,&capsule.scope.owner_id,&capsule.scope.source.as_str(),&capsule.lifetime,&capsule.input_event_id,&encoded]).await?;
                    }
                }
                AtomicIngressMutation::Recovery(commit) => {
                    let source_exists:bool=tx.query_one(&format!("SELECT EXISTS(SELECT 1 FROM {qualified_message_events_table} WHERE event_id=$1 AND workspace_id=$2 AND owner_id=$3 AND source=$4 AND idempotency_key=$5 AND run_id=$6 AND role='user' AND event_type='message.created')"),&[&capsule.input_event_id,&capsule.scope.workspace_id,&capsule.scope.owner_id,&capsule.scope.source.as_str(),&capsule.input_key,&capsule.run_id]).await?.get(0);
                    anyhow::ensure!(source_exists,"ingress recovery input no longer matches its owned capsule");
                    let row=tx.query_one(&format!("SELECT CASE WHEN octet_length(capsule)<=16384 THEN capsule ELSE NULL END,status FROM {table} WHERE namespace=$1 AND workspace_id=$2 AND owner_id=$3 AND input_event_id=$4 FOR UPDATE"),&[&self.qualified_message_events_table,&capsule.scope.workspace_id,&capsule.scope.owner_id,&capsule.input_event_id]).await?;
                    anyhow::ensure!(row.get::<_,Option<String>>(0).context("oversized stored ingress capsule")?==serde_json::to_string(capsule)?,"ingress recovery capsule changed");
                    let old:String=row.get(1);
                    let state=match commit.state {IngressRecoveryState::Unconfirmed=>"unconfirmed",IngressRecoveryState::Reconciled=>"reconciled",IngressRecoveryState::Pending=>anyhow::bail!("cannot reset ingress recovery")};
                    anyhow::ensure!(old!="reconciled" || state=="reconciled","cannot downgrade ingress recovery");
                    tx.execute(&format!("UPDATE {table} SET status=$1 WHERE namespace=$2 AND workspace_id=$3 AND input_event_id=$4"),&[&state,&self.qualified_message_events_table,&capsule.scope.workspace_id,&capsule.input_event_id]).await?;
                }
            }
            }
            remaining(deadline)?;
            tx.commit().await?;
            // Lease and the owned driver remain alive through commit, including
            // cancellation. Any late ordinary terminal is evidence only and
            // has no access to this projection transition.
            let _retained=&lease;
            deadline_guard.complete();
            Ok((event,inserted>0))
        }).await
    }
}
impl PostgresMemory {
    pub(super) async fn ingress_read(
        &self,
        scope: &IngressRecoveryScope,
        lookup: &IngressEventLookup,
        deadline: Instant,
    ) -> anyhow::Result<Option<MessageEvent>> {
        let (column, value, source_bound) = match lookup {
            IngressEventLookup::EventId(value) => ("event_id", value, true),
            IngressEventLookup::IdempotencyKey(value) => ("idempotency_key", value, true),
            IngressEventLookup::OwnedEventId(value) => ("event_id", value, false),
            IngressEventLookup::OwnedIdempotencyKey(value) => ("idempotency_key", value, false),
        };
        let source_filter = source_bound.then(|| scope.source.as_str());
        anyhow::ensure!(
            !value.is_empty()
                && value.len() <= MAX_ID_BYTES
                && !scope.workspace_id.is_empty()
                && !scope.owner_id.is_empty()
                && scope.workspace_id.len() <= MAX_ID_BYTES
                && scope.owner_id.len() <= MAX_ID_BYTES
                && !scope.source.as_str().is_empty()
                && scope.source.as_str().len() <= MAX_ID_BYTES,
            "invalid ingress read scope"
        );
        let lease = reading_lease(
            &self.qualified_message_events_table,
            self.ingress_instance(),
            &scope.workspace_id,
        )?;
        let session = bounded(deadline, async { Ok(lease.session.lock().await) }).await?;
        bounded(deadline, async {
            let mut deadline_guard = SessionDeadline::new(&session, deadline);
            let table = &self.qualified_message_events_table;
            let predicate =
                format!("workspace_id=$1 AND owner_id=$2 AND {column}=$3 AND ($4::TEXT IS NULL OR source=$4)");
            let Some(size) = session
                .client
                .query_opt(
                    &format!("SELECT octet_length(e::text) FROM {table} e WHERE {predicate}"),
                    &[&scope.workspace_id, &scope.owner_id, &value, &source_filter],
                )
                .await?
            else {
                deadline_guard.complete();
                return Ok(None);
            };
            anyhow::ensure!(
                size.get::<_, i32>(0) >= 0 && size.get::<_, i32>(0) as usize <= MAX_RECOVERY_EVENT_BYTES,
                "ingress recovery event exceeds byte budget"
            );
            let row = session
                .client
                .query_opt(
                    &format!("SELECT {EVENT_COLUMNS} FROM {table} e WHERE {predicate} AND octet_length(e::text)<=$5"),
                    &[
                        &scope.workspace_id,
                        &scope.owner_id,
                        &value,
                        &source_filter,
                        &(MAX_RECOVERY_EVENT_BYTES as i32),
                    ],
                )
                .await?
                .context("ingress event changed during bounded read")?;
            let event = Self::row_to_message_event(&row)?;
            deadline_guard.complete();
            Ok(Some(event))
        })
        .await
    }
}

fn reading_lease(namespace: &str, backend_instance: uuid::Uuid, workspace: &str) -> anyhow::Result<Arc<Lease>> {
    let mut registry = leases().lock();
    registry.retain(|entry| entry.strong_count() > 0);
    registry
        .iter()
        .filter_map(std::sync::Weak::upgrade)
        .find(|lease| {
            lease.relation_namespace == namespace
                && lease.backend_instance == backend_instance
                && lease.workspace == workspace
                && !lease.recovery
                && !lease.lost.is_cancelled()
        })
        .context("PostgreSQL ingress read requires a live backend-issued owner")
}

impl PostgresMemory {
    pub(super) async fn ingress_repair_console(&self, event: &MessageEvent, deadline: Instant) -> anyhow::Result<()> {
        event.validate_console_projection()?;
        serialized_within(event, MAX_RECOVERY_EVENT_BYTES)?;
        let lease = reading_lease(
            &self.qualified_message_events_table,
            self.ingress_instance(),
            &event.workspace_id,
        )?;
        lease.validate(deadline).await?;
        let mut session = bounded(deadline, async { Ok(lease.session.lock().await) }).await?;
        bounded(deadline,async {
            let mut deadline_guard=SessionDeadline::new(&session,deadline);
            let events_table=&self.qualified_message_events_table;
            let sessions_table=self.qualified_sessions_table();
            let turns_table=self.qualified_conversation_turns_table();
            let tx: tokio_postgres::Transaction<'_> = session.client.transaction().await?;
            let key=lock_key(&self.qualified_message_events_table,&lease.workspace,&lease.token);
            let acquired:bool=tx.query_one("SELECT pg_try_advisory_xact_lock_shared($1)",&[&key]).await?.get(0);
            anyhow::ensure!(acquired,"ingress lifetime is being recovered");
            let active:bool=tx.query_one(&format!("SELECT active FROM {} WHERE namespace=$1 AND workspace_id=$2 AND token=$3 FOR SHARE",self.ingress_lifetime_table()),&[&self.qualified_message_events_table,&lease.workspace,&lease.token]).await?.get(0);
            anyhow::ensure!(active,"console recovery owner was fenced");
            let size:i32=tx.query_one(&format!("SELECT octet_length(e::text) FROM {events_table} e WHERE event_id=$1 AND workspace_id=$2 AND owner_id=$3 FOR UPDATE"),&[&event.event_id,&event.workspace_id,&event.owner_id]).await?.get(0);
            anyhow::ensure!(size>=0 && size as usize<=MAX_RECOVERY_EVENT_BYTES,"console recovery event exceeds byte budget");
                // The event row lock serializes competing retries of this exact
                // projection across backend instances, before checking history.
                let row = tx.query_one(&format!(
                    "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                            parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                            sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                            goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                            content, content_hash, raw_payload_json, visibility, created_at, updated_at
                     FROM {events_table} WHERE event_id = $1 AND workspace_id = $2 AND owner_id = $3 FOR UPDATE"),
                    &[&event.event_id, &event.workspace_id, &event.owner_id]).await?;
                let durable = Self::row_to_message_event(&row)?;
                anyhow::ensure!(serde_json::to_value(&durable)? == serde_json::to_value(&event)?,
                    "console projection differs from its durable event");
                let existing = tx.query(&format!("SELECT CASE WHEN octet_length(session_key)<=4096 THEN session_key ELSE NULL END, CASE WHEN owner_id IS NULL OR octet_length(owner_id)<=4096 THEN owner_id ELSE NULL END, CASE WHEN octet_length(role)<=4096 THEN role ELSE NULL END, CASE WHEN octet_length(content)<=1048576 THEN content ELSE NULL END FROM {turns_table}
                    WHERE message_event_id = $1 OR message_id = $1 LIMIT 2"), &[&event.event_id]).await?;
                if let Some(row) = existing.first() {
                    anyhow::ensure!(existing.len() == 1
                        && row.get::<_, Option<String>>(0) == event.session_key
                        && row.get::<_, Option<String>>(1) == event.owner_id
                        && row.get::<_, Option<String>>(2).as_deref() == Some(event.role.as_str()) && row.get::<_, Option<String>>(3).as_deref() == Some(event.content.as_str()),
                        "console message projection conflicts with its durable event");
                    tx.commit().await?;deadline_guard.complete();return Ok(());
                }
                let preview: String = event.content.chars().take(200).collect();
                tx.execute(&format!("INSERT INTO {sessions_table} AS current_session
                    (session_key, channel, sender, owner_id, created_at, updated_at, message_count, last_message_preview)
                    VALUES ($1, $2, $3, $4, $5, $5, 1, $6)
                    ON CONFLICT(session_key) DO UPDATE SET
                        message_count = COALESCE(current_session.message_count, 0) + 1,
                        channel = CASE WHEN EXCLUDED.updated_at >= current_session.updated_at THEN EXCLUDED.channel ELSE current_session.channel END,
                        sender = CASE WHEN EXCLUDED.updated_at >= current_session.updated_at THEN EXCLUDED.sender ELSE current_session.sender END,
                        owner_id = CASE WHEN EXCLUDED.updated_at >= current_session.updated_at THEN EXCLUDED.owner_id ELSE current_session.owner_id END,
                        last_message_preview = CASE WHEN EXCLUDED.updated_at >= current_session.updated_at THEN EXCLUDED.last_message_preview ELSE current_session.last_message_preview END,
                        updated_at = GREATEST(current_session.updated_at, EXCLUDED.updated_at)"),
                    &[&event.session_key, &event.channel, &event.sender, &event.owner_id, &event.created_at, &preview]).await?;
                tx.execute(&format!("INSERT INTO {turns_table}
                    (session_key, owner_id, role, content, timestamp, message_id, message_event_id, agent_id, persona_id, visibility)
                    VALUES ($1, $2, $3, $4, $5, $6, $6, $7, $8, 'session')"),
                    &[&event.session_key, &event.owner_id, &event.role, &event.content, &event.created_at,
                        &event.event_id, &event.agent_id, &event.persona_id]).await?;

            remaining(deadline)?;
            tx.commit().await?;
            deadline_guard.complete();
            Ok(())
        }).await
    }
}

const EVENT_COLUMNS: &str = "id,event_id,idempotency_key,workspace_id,owner_id,source,channel,session_key,parent_session_key,run_id,parent_run_id,agent_id,persona_id,sender,recipient,role,event_type,source_ref_json,subject_ref_json,goal_id,causation_event_id,correlation_id,attempt_id,lease_epoch,config_generation_id,config_source_revision,content,content_hash,raw_payload_json,visibility,created_at,updated_at";
