//! Atomic recovery terminal commit: events, attestation, and cost outbox.

use super::{SqliteMemory, ingress_recovery, recovery_authority, recovery_keys::KeyDirectory, recovery_outbox};
use crate::memory::ingress_recovery::{AtomicIngressMutation, remaining};
use crate::memory::recovery_authority::{RecoveryTerminalCommit, RecoveryTerminalConflict, RecoveryTerminalOutcome};
use crate::memory::traits::{MessageEvent, MessageEventInput};
use anyhow::Context;
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use std::cell::RefCell;
use std::time::Instant;

type AfterPrefix = Box<dyn FnOnce(&[MessageEvent], &mut MessageEventInput) -> anyhow::Result<()> + Send>;

tokio::task_local! {
    static AFTER_PREFIX: RefCell<Option<AfterPrefix>>;
}

#[cfg(test)]
thread_local! {
    static CRASH_BEFORE_COMMIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Install a hook that runs after prefix events are durable and before the final row is inserted.
pub(crate) async fn with_after_prefix_async<F, R>(hook: AfterPrefix, work: F) -> R
where
    F: std::future::Future<Output = R>,
{
    AFTER_PREFIX.scope(RefCell::new(Some(hook)), work).await
}

/// Scope the decorator's hook to this task, including across backend awaits.
pub(crate) async fn map_after_prefix<F, R>(map: impl FnOnce(Option<AfterPrefix>) -> AfterPrefix, work: F) -> R
where
    F: std::future::Future<Output = R>,
{
    let hook = AFTER_PREFIX.try_with(|slot| slot.borrow_mut().take()).ok().flatten();
    with_after_prefix_async(map(hook), work).await
}

#[cfg(test)]
pub(crate) fn crash_before_commit(crash: bool) {
    CRASH_BEFORE_COMMIT.with(|cell| cell.set(crash));
}

impl SqliteMemory {
    pub(crate) async fn commit_recovery_terminal_impl(
        &self,
        commit: RecoveryTerminalCommit,
        deadline: Instant,
    ) -> anyhow::Result<RecoveryTerminalOutcome> {
        remaining(deadline)?;
        let pool = self.pool.clone();
        let db_path = self.db_path.clone();
        let hook = AFTER_PREFIX.try_with(|slot| slot.borrow_mut().take()).ok().flatten();
        #[cfg(test)]
        let crash = CRASH_BEFORE_COMMIT.with(|cell| {
            let crash = cell.get();
            cell.set(false);
            crash
        });
        crate::runtime::blocking::spawn_blocking(move || {
            #[cfg(test)]
            if crash {
                CRASH_BEFORE_COMMIT.with(|cell| cell.set(true));
            }
            commit_blocking(&pool, &db_path, commit, deadline, hook)
        })
        .await?
    }
}

fn commit_blocking(
    pool: &super::erasure_pool::MemoryPool,
    db_path: &std::path::Path,
    mut commit: RecoveryTerminalCommit,
    deadline: Instant,
    hook: Option<AfterPrefix>,
) -> anyhow::Result<RecoveryTerminalOutcome> {
    remaining(deadline)?;
    anyhow::ensure!(!commit.events.is_empty(), "recovery terminal commit has no events");
    let mapped = (|| {
        let write = pool.write_until(deadline)?;
        let _sync = recovery_authority::FullSync::new(&write)?;
        let tx = rusqlite::Transaction::new_unchecked(&write, TransactionBehavior::Immediate)?;
        recovery_authority::schema(&tx)?;
        remaining(deadline)?;

        let directory = KeyDirectory::open(db_path.parent().context("missing memory directory")?, false)?;
        let mut stored = Vec::new();
        let mut inserted_any = false;
        let mut existing_any = false;

        if let Some(ingress) = commit.ingress.take() {
            let input = ingress.evidence.clone();
            let mutation = AtomicIngressMutation::Recovery(ingress);
            ingress_recovery::before_append(&tx, &mutation, &input)?;
            let (event, inserted) = SqliteMemory::append_event_row(&tx, input, true)?;
            ingress_recovery::after_append(&tx, &mutation, &event, inserted)?;
            inserted_any |= inserted;
            existing_any |= !inserted;
            stored.push(event);
        }

        let final_index = commit.events.len().saturating_sub(1);
        let prefix: Vec<MessageEventInput> = commit.events.drain(..final_index).collect();
        let mut final_input = commit
            .events
            .pop()
            .context("recovery terminal commit has no final event")?;

        for input in prefix {
            let (event, inserted) = insert_terminal_event(&tx, input)?;
            inserted_any |= inserted;
            existing_any |= !inserted;
            stored.push(event);
        }

        if let Some(hook) = hook {
            hook(&stored, &mut final_input)?;
        }

        let (final_event, inserted) = insert_terminal_event(&tx, final_input)?;
        inserted_any |= inserted;
        existing_any |= !inserted;
        stored.push(final_event);

        if inserted_any && existing_any {
            return Err(RecoveryTerminalConflict.into());
        }

        let terminal_id = stored
            .last()
            .context("recovery terminal commit lost its final event")?
            .event_id
            .clone();
        let mut publication = commit.publication;
        // Preserve the reception already authenticated for this run. publish_in
        // verifies the previous receipt and the input bytes before settling it.
        if publication.input.is_none() {
            let input_id: Option<Option<String>> = tx
                .query_row(
                    "SELECT input_event_id FROM recovery_attestations
                 WHERE workspace_id=?1 AND owner_id=?2 AND source=?3 AND run_id=?4",
                    params![
                        publication.chain.workspace_id,
                        publication.chain.owner_id,
                        publication.chain.source,
                        publication.chain.run_id
                    ],
                    |row| row.get(0),
                )
                .optional()?;
            publication.input = input_id
                .flatten()
                .map(|id| recovery_authority::event(&tx, &publication.chain, &id))
                .transpose()?;
        }
        publication.terminal = Some(recovery_authority::event(&tx, &publication.chain, &terminal_id)?);
        let receipt = match recovery_authority::publish_in(&tx, &directory, &publication, true) {
            Ok(receipt) => receipt,
            Err(error) if error.to_string().contains("conflicting recovery") => {
                return Err(RecoveryTerminalConflict.into());
            }
            Err(error) => return Err(error),
        };

        let settlement_id = settlement_id_from_facts(&commit.outbox.facts)?;
        recovery_outbox::insert_in(
            &tx,
            &directory,
            &publication.chain.workspace_id,
            publication.chain.owner_id.as_str(),
            &settlement_id,
            &receipt.key_id,
            commit.outbox.pricing,
            &commit.outbox.facts,
        )?;

        remaining(deadline)?;
        #[cfg(test)]
        if CRASH_BEFORE_COMMIT.with(std::cell::Cell::get) {
            CRASH_BEFORE_COMMIT.with(|cell| cell.set(false));
            anyhow::bail!("test: crash before recovery terminal commit");
        }
        tx.commit()?;
        remaining(deadline)?;

        let outcome = if existing_any {
            RecoveryTerminalOutcome::Replayed {
                receipt,
                events: stored,
            }
        } else {
            RecoveryTerminalOutcome::Committed {
                receipt,
                events: stored,
            }
        };
        Ok(outcome)
    })();
    match mapped {
        Err(error) if error.is::<RecoveryTerminalConflict>() => Ok(RecoveryTerminalOutcome::Conflict),
        other => other,
    }
}

fn insert_terminal_event(
    tx: &rusqlite::Transaction<'_>,
    input: MessageEventInput,
) -> anyhow::Result<(MessageEvent, bool)> {
    let expected = input.clone();
    let (event, inserted) = SqliteMemory::append_event_row(tx, input, false)?;
    if !inserted && !events_match(&expected, &event) {
        return Err(RecoveryTerminalConflict.into());
    }
    Ok((event, inserted))
}

fn events_match(input: &MessageEventInput, stored: &MessageEvent) -> bool {
    let event_id_ok = input.event_id.as_ref().is_none_or(|id| id == &stored.event_id);
    event_id_ok
        && input.idempotency_key == stored.idempotency_key
        && input.workspace_id == stored.workspace_id
        && input.owner_id == stored.owner_id
        && input.source == stored.source
        && input.role == stored.role
        && input.event_type == stored.event_type
        && input.content == stored.content
        && input.raw_payload_json == stored.raw_payload_json
        && input.session_key == stored.session_key
        && input.run_id == stored.run_id
        && input.channel == stored.channel
        && input.parent_session_key == stored.parent_session_key
        && input.parent_run_id == stored.parent_run_id
        && input.agent_id == stored.agent_id
        && input.persona_id == stored.persona_id
        && input.sender == stored.sender
        && input.recipient == stored.recipient
        && input.subject == stored.subject
        && input.goal_id == stored.goal_id
        && input.causation_event_id == stored.causation_event_id
        && input.correlation_id == stored.correlation_id
        && input.attempt_id == stored.attempt_id
        && input.lease_epoch == stored.lease_epoch
        && input.config_generation_id == stored.config_generation_id
        && input.config_source_revision == stored.config_source_revision
        && input.visibility == stored.visibility
}

fn settlement_id_from_facts(facts: &[u8]) -> anyhow::Result<String> {
    let value: serde_json::Value = serde_json::from_slice(facts).context("recovery outbox facts are invalid")?;
    value
        .get("terminal_id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty() && id.len() <= 1024)
        .map(str::to_owned)
        .context("recovery cost outbox settlement_id is invalid")
}
