//! Atomic recovery terminal commit: events, attestation, and cost outbox.

use super::{SqliteMemory, ingress_recovery, recovery_authority, recovery_keys::KeyDirectory, recovery_outbox};
use crate::memory::ingress_recovery::{AtomicIngressMutation, remaining};
use crate::memory::recovery_authority::{RecoveryTerminalCommit, RecoveryTerminalConflict, RecoveryTerminalOutcome};
use crate::memory::traits::{MessageEvent, MessageEventInput};
use anyhow::Context;
use rusqlite::TransactionBehavior;
use std::cell::RefCell;
use std::time::Instant;

type AfterPrefix = Box<dyn FnOnce(&[MessageEvent], &mut MessageEventInput) -> anyhow::Result<()> + Send>;

thread_local! {
    static AFTER_PREFIX: RefCell<Option<AfterPrefix>> = const { RefCell::new(None) };
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
    AFTER_PREFIX.with(|slot| {
        *slot.borrow_mut() = Some(hook);
    });
    let result = work.await;
    AFTER_PREFIX.with(|slot| {
        slot.borrow_mut().take();
    });
    result
}

/// Rewrite the installed after-prefix hook. EncryptedScope uses this so the
/// replay seal sees events as the decorator presents them (plaintext).
pub(crate) fn map_after_prefix(map: impl FnOnce(AfterPrefix) -> AfterPrefix) {
    AFTER_PREFIX.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(hook) = slot.take() {
            *slot = Some(map(hook));
        }
    });
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
        let hook = AFTER_PREFIX.with(|slot| slot.borrow_mut().take());
        #[cfg(test)]
        let crash = CRASH_BEFORE_COMMIT.with(|cell| {
            let crash = cell.get();
            cell.set(false);
            crash
        });
        crate::runtime::blocking::spawn_blocking(move || {
            if let Some(hook) = hook {
                AFTER_PREFIX.with(|slot| {
                    *slot.borrow_mut() = Some(hook);
                });
            }
            #[cfg(test)]
            if crash {
                CRASH_BEFORE_COMMIT.with(|cell| cell.set(true));
            }
            let result = commit_blocking(&pool, &db_path, commit, deadline);
            AFTER_PREFIX.with(|slot| {
                slot.borrow_mut().take();
            });
            result
        })
        .await?
    }
}

fn commit_blocking(
    pool: &super::erasure_pool::MemoryPool,
    db_path: &std::path::Path,
    mut commit: RecoveryTerminalCommit,
    deadline: Instant,
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

        let hook = AFTER_PREFIX.with(|slot| slot.borrow_mut().take());
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
