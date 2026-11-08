//! Repair retained authenticated child telemetry; never admit child execution.
use super::{
    Arc, BTreeMap, Context, DecisionScope, Enqueue, Instant, OutcomeCorrelator, TerminalFacts, TurnDecisionService,
};
use crate::agent::terminal::replay_requires_journal;
use crate::causal_tree::store::{ChildRecoveryCursor, ChildRecoveryLineage};
use crate::memory::MessageEvent;
use crate::memory::ingress_recovery::{IngressEventLookup, IngressLifetime, IngressRecoveryScope, remaining};
use crate::runtime::child_source::authority_journal::{self, EvidenceCursor};
use crate::runtime::child_source::completion::{ChildOutcomeHook, JournalChildOutcomeHook};
use crate::runtime::child_source::recovery;
use crate::runtime::turn_cleanup::CleanupTracker;

#[derive(Default)]
pub(super) struct ChildRecoveryState {
    owners: parking_lot::Mutex<BTreeMap<String, OwnerCursor>>,
}

/// Separate bounded scan positions: journal evidence and legacy store rows.
#[derive(Default)]
struct RecoveryPositions {
    /// Which cursor the next call scans; flipped before every scan.
    next_legacy: bool,
    legacy: Option<ChildRecoveryCursor>,
    journal: Option<EvidenceCursor>,
}

/// A scanned row advances exactly one of the two positions.
enum Advance {
    Legacy(ChildRecoveryCursor),
    Journal(EvidenceCursor),
}

impl Advance {
    const fn apply(self, positions: &mut RecoveryPositions) {
        match self {
            Self::Legacy(cursor) => positions.legacy = Some(cursor),
            Self::Journal(cursor) => positions.journal = Some(cursor),
        }
    }
}

struct OwnerCursor {
    owner: std::sync::Weak<CleanupTracker>,
    cursor: Arc<tokio::sync::Mutex<RecoveryPositions>>,
}

/// Plane off, missing legacy evidence, or an unrecordable service skips repair.
pub(crate) fn child_recovery_gate(
    recovery: &crate::runtime::ingress_recovery::RecoveryScope,
    plane: bool,
    recordable: bool,
    durable_key: bool,
) -> Option<ChildRecoveryReport> {
    #[cfg(test)]
    recovery.spies().note_child_recovery();
    if recovery.authority.legacy.is_none() || !plane || !recordable || !durable_key {
        return Some(ChildRecoveryReport::default());
    }
    None
}

/// Counts are evidence statuses, not an exhaustive reconciliation assertion.
#[derive(Default, Debug)]
pub(crate) struct ChildRecoveryReport {
    pub(crate) scanned: usize,
    pub(crate) repaired: usize,
    pub(crate) already_recorded: usize,
    pub(crate) unconfirmed: usize,
    pub(crate) no_final: usize,
    pub(crate) other_verified_authority: usize,
    pub(crate) deadline_reached: bool,
    pub(crate) wrapped: bool,
    pub(crate) scan_unavailable: bool,
    pub(crate) deferred: bool,
}

impl TurnDecisionService {
    /// Called only inside the authenticated recorder's registered owned task.
    /// The caller keeps the actual backend lifetime alive to task completion.
    pub(crate) async fn reconcile_child_telemetry(
        &self,
        scope: &IngressRecoveryScope,
        lifetime: &Arc<dyn IngressLifetime>,
        owner: &Arc<CleanupTracker>,
        deadline: Instant,
    ) -> anyhow::Result<ChildRecoveryReport> {
        let mut report = ChildRecoveryReport::default();
        if let Some(skipped) = child_recovery_gate(
            &self.recovery,
            self.generation.effective.causal_tree.enabled,
            self.recordable,
            self.durable_key,
        ) {
            return Ok(skipped);
        }
        anyhow::ensure!(
            self.parent_grant.is_none(),
            "child repair requires the published ingress owner"
        );
        anyhow::ensure!(
            scope.workspace_id == self.generation.effective.workspace_dir.to_string_lossy()
                && !scope.owner_id.is_empty()
                && scope.owner_id.len() <= 4096,
            "child repair owner scope differs from the published service"
        );
        if remaining(deadline).is_err() {
            report.deadline_reached = true;
            return Ok(report);
        }
        if let Err(error) = lifetime.validate(deadline).await {
            if remaining(deadline).is_err() {
                report.deadline_reached = true;
                return Ok(report);
            }
            return Err(error);
        }
        let cursor = {
            let Some(mut owners) = self.child_recovery.owners.try_lock() else {
                report.deferred = true;
                return Ok(report);
            };
            owners.retain(|_, entry| entry.owner.strong_count() > 0 || Arc::strong_count(&entry.cursor) > 1);
            if let Some(entry) = owners.get_mut(&scope.owner_id) {
                entry.owner = Arc::downgrade(owner);
                Arc::clone(&entry.cursor)
            } else {
                if owners.len() >= 64 {
                    report.deferred = true;
                    return Ok(report);
                }
                let cursor = Arc::new(tokio::sync::Mutex::new(RecoveryPositions::default()));
                owners.insert(
                    scope.owner_id.clone(),
                    OwnerCursor {
                        owner: Arc::downgrade(owner),
                        cursor: Arc::clone(&cursor),
                    },
                );
                cursor
            }
        };
        let Ok(mut position) = tokio::time::timeout_at(deadline.into(), cursor.lock()).await else {
            report.deadline_reached = true;
            return Ok(report);
        };
        let store_workspace = self
            .store
            .workspace_id()
            .context("child recovery store scope missing")?;
        let store_scope = DecisionScope::Owner {
            workspace_id: Arc::clone(&store_workspace),
            owner_id: Arc::from(scope.owner_id.as_str()),
        };
        // Flipped before the scan, so a failing scan on one cursor never repeats it.
        let scan_legacy = position.next_legacy;
        position.next_legacy = !scan_legacy;
        let page: Vec<(Advance, Option<ChildRecoveryLineage>)> = if !scan_legacy {
            let (workspace, key) = (
                self.generation.effective.workspace_dir.clone(),
                Arc::clone(&self.state_key),
            );
            let (store_ws, owner_id) = (Arc::clone(&store_workspace), scope.owner_id.clone());
            let mut journal = position.journal;
            let scanned = super::super::ingress_recovery::owned_store_read(owner, move || {
                let page = authority_journal::next_evidence_page(
                    &mut journal,
                    &workspace,
                    &key,
                    &store_ws,
                    &owner_id,
                    64,
                    deadline,
                )?;
                Ok((journal, page))
            })
            .await;
            match scanned {
                Ok((journal, page)) => {
                    position.journal = journal;
                    page.into_iter()
                        .map(|item| (Advance::Journal(item.cursor), item.lineage))
                        .collect()
                }
                Err(_) => {
                    report.scan_unavailable = true;
                    report.deadline_reached = remaining(deadline).is_err();
                    return Ok(report);
                }
            }
        } else {
            let reader = self.store.reader();
            let scan_scope = store_scope.clone();
            let after = position.legacy;
            let page = super::super::ingress_recovery::owned_store_read(owner, move || {
                Ok(reader.child_recovery_candidates_until(&scan_scope, after, 64, deadline)?)
            })
            .await;
            match page {
                Ok(page) => page
                    .into_iter()
                    .map(|item| (Advance::Legacy(item.cursor), item.lineage))
                    .collect(),
                Err(_) => {
                    report.scan_unavailable = true;
                    report.deadline_reached = remaining(deadline).is_err();
                    return Ok(report);
                }
            }
        };
        if page.is_empty() {
            if scan_legacy {
                position.legacy = None;
            }
            report.wrapped = true;
            return Ok(report);
        }
        for (advance, lineage) in page {
            if remaining(deadline).is_err() {
                report.deadline_reached = true;
                break;
            }
            // Unconfirmed history cannot pin enumeration forever. This is a
            // live scan position only: wrap/restart revisits unresolved rows.
            advance.apply(&mut position);
            report.scanned += 1;
            let Some(lineage) = lineage else {
                report.unconfirmed += 1;
                continue;
            };
            let verified = self.verify_child_candidate(scope, &lineage, owner, deadline).await;
            let (event, replay, journal) = match verified {
                Ok(Some(value)) => value,
                Ok(None) => {
                    report.no_final += 1;
                    continue;
                }
                Err(_) => {
                    report.unconfirmed += 1;
                    continue;
                }
            };
            if replay.parent_decision_id().is_none() {
                report.other_verified_authority += 1;
                continue;
            }
            // No parent grant is constructed. The verifier checked the actual
            // original parent row/digest and both owner-scoped source records.
            if remaining(deadline).is_err() {
                report.deadline_reached = true;
                break;
            }
            if let Err(error) = lifetime.validate(deadline).await {
                if remaining(deadline).is_err() {
                    report.deadline_reached = true;
                    break;
                }
                return Err(error);
            }
            match self
                .repair_verified_child(&event, &replay, journal, &store_scope, lifetime, owner, deadline)
                .await?
            {
                Some(true) => report.repaired += 1,
                Some(false) => report.already_recorded += 1,
                None => report.unconfirmed += 1,
            }
        }
        report.deadline_reached |= remaining(deadline).is_err();
        Ok(report)
    }

    async fn verify_child_candidate(
        &self,
        scope: &IngressRecoveryScope,
        lineage: &ChildRecoveryLineage,
        owner: &Arc<CleanupTracker>,
        deadline: Instant,
    ) -> anyhow::Result<Option<(MessageEvent, crate::agent::terminal::VerifiedTerminalReplay, bool)>> {
        remaining(deadline)?;
        let Some(event) = self
            .memory
            .read_ingress_event(
                scope,
                &IngressEventLookup::OwnedIdempotencyKey(format!("turn:{}:final", lineage.run_id)),
                deadline,
            )
            .await?
        else {
            return Ok(None);
        };
        let store_workspace = self
            .store
            .workspace_id()
            .context("child recovery store scope missing")?;
        if replay_requires_journal(&self.state_key, &event, None)? {
            let replay = recovery::verify_candidate(
                self.memory.as_ref(),
                &self.generation.effective.workspace_dir,
                &self.state_key,
                &store_workspace,
                scope,
                lineage,
                &event,
                owner,
                deadline,
            )
            .await?;
            return Ok(Some((event, replay, true)));
        }
        let replay = crate::agent::terminal::verify_terminal_replay_until(
            self.memory.as_ref(),
            &self.store.reader(),
            &self.state_key,
            &store_workspace,
            &scope.workspace_id,
            &scope.owner_id,
            &lineage.run_id,
            &event,
            deadline,
            owner,
        )
        .await?;
        anyhow::ensure!(
            replay.source().source != crate::memory::MessageEventSource::SessionWorker,
            "worker segment requires launch provenance for legacy selection"
        );
        anyhow::ensure!(
            replay.source().event_id == lineage.source_event_id
                && replay.source().run_id.as_deref() == Some(lineage.run_id.as_str())
                && replay.source().session_key.as_deref() == Some(lineage.session_key.as_str()),
            "child discovery lineage differs from authenticated terminal"
        );
        // The enumerated row can be any hop. Verify that it belongs to
        // the authenticated source; repair always uses replay's last hop.
        let reader = self.store.reader();
        let decision_scope = DecisionScope::Owner {
            workspace_id: store_workspace,
            owner_id: Arc::from(scope.owner_id.as_str()),
        };
        let id = lineage.decision_id;
        let row = super::super::ingress_recovery::owned_store_read(owner, move || {
            Ok(reader.replay_record_until(&decision_scope, &id, deadline)?)
        })
        .await?
        .context("child discovery decision disappeared")?;
        anyhow::ensure!(
            row.source_event_id.as_deref() == Some(lineage.source_event_id.as_str())
                && row.decision.run_id.as_deref() == Some(lineage.run_id.as_str())
                && row.decision.session_key.as_deref() == Some(lineage.session_key.as_str()),
            "child discovery decision lineage changed"
        );
        Ok(Some((event, replay, false)))
    }

    async fn repair_verified_child(
        &self,
        event: &MessageEvent,
        replay: &crate::agent::terminal::VerifiedTerminalReplay,
        journal: bool,
        scope: &DecisionScope,
        lifetime: &Arc<dyn IngressLifetime>,
        owner: &Arc<CleanupTracker>,
        deadline: Instant,
    ) -> anyhow::Result<Option<bool>> {
        // Runs before the matching-row check and outside its error-to-None
        // conversion. A journal error propagates; it is never a recorded success.
        let store_workspace = self
            .store
            .workspace_id()
            .context("child recovery store scope missing")?;
        if remaining(deadline).is_err() || lifetime.validate(deadline).await.is_err() {
            return Ok(None);
        }
        if journal {
            // Journal-required children are settled from independent evidence
            // only. No projection write, drain or store readback takes part.
            let written = recovery::settle(
                &self.generation.effective.workspace_dir,
                &self.state_key,
                &store_workspace,
                replay,
                event,
                owner,
                deadline,
            )
            .await?;
            if remaining(deadline).is_err() || lifetime.validate(deadline).await.is_err() {
                return Ok(None);
            }
            return Ok(Some(written));
        }
        JournalChildOutcomeHook::new(
            self.generation.effective.workspace_dir.clone(),
            Arc::clone(&self.state_key),
            store_workspace,
        )
        .persist(replay, event, Some(owner), Some(deadline))
        .await?;
        if remaining(deadline).is_err() || lifetime.validate(deadline).await.is_err() {
            return Ok(None);
        }
        let prepared = async {
            remaining(deadline)?;
            let workspace = self.generation.effective.workspace_dir.to_string_lossy();
            let source_owner = replay
                .source()
                .owner_id
                .as_deref()
                .context("verified child owner missing")?;
            let facts = TerminalFacts::from_persisted_marker(event, replay.decision(), source_owner, &workspace)?
                .with_verified_resume(replay);
            let expected = OutcomeCorrelator::persisted_record(replay.decision(), &facts)?;
            let link = crate::causal_tree::store::ChildOutcomeLink::from_replay(replay, &expected)
                .context("verified child parent link is inconsistent")?;
            let reader = self.store.reader();
            let read_scope = scope.clone();
            let matching = super::super::ingress_recovery::owned_store_read(owner, move || {
                Ok(reader.child_outcome_matches_until(&read_scope, &link, deadline)?)
            })
            .await?;
            Ok::<_, anyhow::Error>((expected, matching))
        }
        .await;
        let (expected, matching) = match prepared {
            Ok(value) => value,
            Err(_) => return Ok(None),
        };
        if matching {
            return Ok(Some(false));
        }
        let writer = self.store.writer().context("child recovery writer missing")?;
        if remaining(deadline).is_err() || lifetime.validate(deadline).await.is_err() {
            return Ok(None);
        }
        if writer.record_outcome(expected.clone()) != Enqueue::Queued {
            return Ok(None);
        }
        anyhow::ensure!(writer.drain(deadline).await, "child recovery outcome did not drain");
        let reader = self.store.reader();
        let read_scope = scope.clone();
        let read_expected = expected.clone();
        if !super::super::ingress_recovery::owned_store_read(owner, move || {
            Ok(reader.outcome_matches_until(&read_scope, &read_expected, deadline)?)
        })
        .await
        .unwrap_or(false)
        {
            return Ok(None);
        }
        let link = crate::causal_tree::store::ChildOutcomeLink::from_replay(replay, &expected)
            .context("verified child link changed")?;
        if remaining(deadline).is_err() || lifetime.validate(deadline).await.is_err() {
            return Ok(None);
        }
        if writer.record_child_outcome(link) != Enqueue::Queued {
            return Ok(None);
        }
        anyhow::ensure!(writer.drain(deadline).await, "child recovery link did not drain");
        let reader = self.store.reader();
        let read_scope = scope.clone();
        let link = crate::causal_tree::store::ChildOutcomeLink::from_replay(replay, &expected)
            .context("verified child link changed")?;
        if !super::super::ingress_recovery::owned_store_read(owner, move || {
            Ok(reader.child_outcome_matches_until(&read_scope, &link, deadline)?)
        })
        .await
        .unwrap_or(false)
        {
            return Ok(None);
        }
        Ok(Some(true))
    }
}
