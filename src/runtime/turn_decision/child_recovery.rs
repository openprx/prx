//! Repair retained authenticated child telemetry; never admit child execution.
use super::{
    Arc, BTreeMap, Context, DecisionScope, Enqueue, Instant, OutcomeCorrelator, TerminalFacts, TurnDecisionService,
};
use crate::causal_tree::store::{ChildRecoveryCursor, ChildRecoveryLineage};
use crate::memory::MessageEvent;
use crate::memory::ingress_recovery::{IngressEventLookup, IngressLifetime, IngressRecoveryScope, remaining};
use crate::runtime::turn_cleanup::CleanupTracker;

#[derive(Default)]
pub(super) struct ChildRecoveryState {
    owners: parking_lot::Mutex<BTreeMap<String, OwnerCursor>>,
}

struct OwnerCursor {
    owner: std::sync::Weak<CleanupTracker>,
    cursor: Arc<tokio::sync::Mutex<Option<ChildRecoveryCursor>>>,
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
        if !self.generation.effective.causal_tree.enabled || !self.recordable || !self.durable_key {
            return Ok(report);
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
                let cursor = Arc::new(tokio::sync::Mutex::new(None));
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
        let reader = self.store.reader();
        let scan_scope = store_scope.clone();
        let after = *position;
        let page = super::super::ingress_recovery::owned_store_read(owner, move || {
            Ok(reader.child_recovery_candidates_until(&scan_scope, after, 64, deadline)?)
        })
        .await;
        let page = match page {
            Ok(page) => page,
            Err(_) => {
                report.scan_unavailable = true;
                report.deadline_reached = remaining(deadline).is_err();
                return Ok(report);
            }
        };
        if page.is_empty() {
            *position = None;
            report.wrapped = true;
            return Ok(report);
        }
        for candidate in page {
            if remaining(deadline).is_err() {
                report.deadline_reached = true;
                break;
            }
            // Unconfirmed history cannot pin enumeration forever. This is a
            // live scan position only: wrap/restart revisits unresolved rows.
            *position = Some(candidate.cursor);
            report.scanned += 1;
            let Some(lineage) = candidate.lineage else {
                report.unconfirmed += 1;
                continue;
            };
            let verified = self.verify_child_candidate(scope, &lineage, owner, deadline).await;
            let (event, replay) = match verified {
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
                .repair_verified_child(&event, &replay, &store_scope, lifetime, owner, deadline)
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
    ) -> anyhow::Result<Option<(MessageEvent, crate::agent::terminal::VerifiedTerminalReplay)>> {
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
        Ok(Some((event, replay)))
    }

    async fn repair_verified_child(
        &self,
        event: &MessageEvent,
        replay: &crate::agent::terminal::VerifiedTerminalReplay,
        scope: &DecisionScope,
        lifetime: &Arc<dyn IngressLifetime>,
        owner: &Arc<CleanupTracker>,
        deadline: Instant,
    ) -> anyhow::Result<Option<bool>> {
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
