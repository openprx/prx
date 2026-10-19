//! Keep authority/control messages independent of admission-local telemetry waits.
use super::{QueueItem, QueuedEvent, applied_input_matches, report_incomplete, to_message_input};
use crate::decision::telemetry::{CausalEventSink, IncompleteReason};
use crate::memory::Memory;
use std::{collections::VecDeque, sync::Arc};
use tokio::sync::mpsc;

pub(super) struct Worker {
    pub(super) receiver: mpsc::Receiver<QueueItem>,
    pub(super) memory: Arc<dyn Memory>,
    pub(super) observer: Arc<dyn CausalEventSink>,
    pub(super) workspace: String,
    pub(super) generation: u64,
    pub(super) revision: String,
    pub(super) deferral: Arc<super::deferral::PersistenceDeferral>,
}

impl Worker {
    pub(super) async fn run(mut self) {
        let registry = Arc::clone(&self.deferral);
        let mut deferred = VecDeque::new();
        let mut closed = false;
        'work: loop {
            let changed = registry.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            // Ordinary permits bound the deferred backlog as well as queued
            // and in-flight events. Never wait on a guard while reading control.
            for _ in 0..super::EVENT_QUEUE_CAPACITY {
                match self.receiver.try_recv() {
                    Ok(QueueItem::Event(event)) => deferred.push_back(event),
                    Ok(control) => {
                        self.control(control, &mut deferred).await;
                        continue 'work;
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        closed = true;
                        break;
                    }
                }
            }
            if closed {
                self.flush_all(&mut deferred).await;
                return;
            }
            let batch = take_ready(&mut deferred, false);
            if !batch.is_empty() {
                self.persist(batch).await;
                continue;
            }
            let next_deadline = deferred
                .iter()
                .filter_map(|event| event.deferral.as_ref().map(|state| state.deadline))
                .min();
            tokio::select! {
                item = self.receiver.recv() => match item {
                    Some(QueueItem::Event(event)) => deferred.push_back(event),
                    Some(control) => self.control(control,&mut deferred).await,
                    None => closed = true,
                },
                () = changed => {},
                () = async {
                    if let Some(deadline) = next_deadline { tokio::time::sleep_until(deadline).await; }
                    else { std::future::pending::<()>().await; }
                } => {},
            }
            if closed {
                self.flush_all(&mut deferred).await;
                return;
            }
        }
    }

    async fn flush_all(&self, deferred: &mut VecDeque<QueuedEvent>) {
        while !deferred.is_empty() {
            self.persist(take_ready(deferred, true)).await;
        }
    }

    async fn persist(&self, batch: Vec<QueuedEvent>) {
        let inputs = batch
            .iter()
            .map(|queued| to_message_input(&queued.event, &self.workspace, self.generation, &self.revision))
            .collect();
        let results = self.memory.append_message_events(inputs).await;
        if results.len() != batch.len() {
            for queued in batch {
                report_incomplete(
                    self.observer.as_ref(),
                    queued.event.lineage,
                    IncompleteReason::StoreFailed,
                );
            }
            return;
        }
        for (queued, result) in batch.into_iter().zip(results) {
            if result.is_err() {
                report_incomplete(
                    self.observer.as_ref(),
                    queued.event.lineage,
                    IncompleteReason::StoreFailed,
                );
            }
        }
    }

    async fn control(&self, control: QueueItem, deferred: &mut VecDeque<QueuedEvent>) {
        match control {
            QueueItem::Drain(reply) => {
                // The barrier makes earlier events durable now; it never waits
                // for a live admission to release its optional I/O preference.
                self.flush_all(deferred).await;
                let _ = reply.send(());
            }
            QueueItem::Applied {
                event,
                idempotency_key,
                original_generation,
                acknowledgement,
            } => {
                let (generation, revision) = original_generation
                    .as_ref()
                    .map_or((self.generation, self.revision.as_str()), |(id, revision)| {
                        (*id, revision.as_ref())
                    });
                let mut input = to_message_input(&event, &self.workspace, generation, revision);
                let event_id = uuid::Uuid::new_v4().to_string();
                input.event_id = Some(event_id.clone());
                input.idempotency_key = Some(idempotency_key);
                let persisted = match self.memory.append_message_event(input.clone()).await {
                    Ok(stored) if applied_input_matches(&stored, &input) => {
                        if stored.event_id == event_id {
                            self.observer.emit(*event);
                        }
                        true
                    }
                    Ok(_) => {
                        report_incomplete(self.observer.as_ref(), event.lineage, IncompleteReason::Conflict);
                        false
                    }
                    Err(_) => {
                        report_incomplete(self.observer.as_ref(), event.lineage, IncompleteReason::StoreFailed);
                        false
                    }
                };
                if let Some(reply) = acknowledgement {
                    let _ = reply.send(persisted);
                }
            }
            // Callers route ordinary events into the same bounded backlog.
            QueueItem::Event(event) => deferred.push_back(event),
        }
    }
}

fn take_ready(deferred: &mut VecDeque<QueuedEvent>, force: bool) -> Vec<QueuedEvent> {
    let mut batch = Vec::new();
    let mut index = 0;
    while index < deferred.len() && batch.len() < crate::memory::traits::MESSAGE_EVENT_BATCH_LIMIT {
        if force
            || deferred
                .get(index)
                .is_some_and(|event| event.deferral.as_ref().is_none_or(|state| state.ready()))
        {
            if let Some(event) = deferred.remove(index) {
                batch.push(event);
            }
        } else {
            index += 1;
        }
    }
    batch
}
