use super::*;
use crate::runtime::h2_entry::{
    EntryKind,
    profile::tests::{Fixture, plan},
};
fn invocation(fixture: &Fixture, action: EntryAction) -> Arc<EntryInvocation> {
    let origin = fixture.origin();
    let source = origin.record();
    let call = EntryCall {
        action,
        tool: "skills_manage".into(),
        args_sha256: "a".repeat(64),
        call_digest: "b".repeat(64),
        capability: "prx-capability".into(),
        capability_generation: hex::encode([2_u8; 32]),
        catalog_generation: hex::encode([3_u8; 32]),
        actor: source.actor.clone(),
        workspace: source.workspace.clone(),
        session: source.session.clone(),
        run: source.run.clone(),
        event: source.event.clone(),
    };
    EntryInvocation::admit(origin, call, None).unwrap()
}
#[test]
fn exact_call_cannot_change_action_or_arguments_and_consumes_once() {
    let fixture = Fixture::new(EntryKind::Terminal);
    let invocation = invocation(&fixture, EntryAction::Catalog);
    assert!(invocation.consume(EntryAction::Plan, &"a".repeat(64)).is_err());
    assert!(invocation.consume(EntryAction::Catalog, &"c".repeat(64)).is_err());
    let command = invocation.consume(EntryAction::Catalog, &"a".repeat(64)).unwrap();
    command
        .verify(
            EntryAction::Catalog,
            &"a".repeat(64),
            &fixture.config,
            &fixture.profile.owner,
        )
        .unwrap();
    assert!(
        command
            .verify(
                EntryAction::Catalog,
                &"a".repeat(64),
                &fixture.config,
                &crate::skills::lifecycle::OwnerScope::new("owner:other").unwrap()
            )
            .is_err()
    );
    assert!(invocation.consume(EntryAction::Catalog, &"a".repeat(64)).is_err());
}
#[tokio::test]
async fn tokio_spawn_never_inherits_scope_but_explicit_continuation_does() {
    let fixture = Fixture::new(EntryKind::Terminal);
    let command = invocation(&fixture, EntryAction::Plan)
        .consume(EntryAction::Plan, &"a".repeat(64))
        .unwrap();
    command
        .continuation_scope(async {
            let held = current_run_continuation().unwrap();
            assert!(
                tokio::spawn(async { current_run_continuation().is_none() && current_invocation().is_err() })
                    .await
                    .unwrap()
            );
            assert!(
                tokio::spawn(async move { held.scope(async { current_run_continuation().is_some() }).await })
                    .await
                    .unwrap()
            );
        })
        .await;
    assert!(current_run_continuation().is_none());
}
#[tokio::test]
async fn sync_scope_restores_after_panic_and_returned_future_gets_no_authority() {
    let fixture = Fixture::new(EntryKind::Terminal);
    let command = invocation(&fixture, EntryAction::Plan)
        .consume(EntryAction::Plan, &"a".repeat(64))
        .unwrap();
    let held = command.continuation;
    let future = held.sync_scope(|| async { current_run_continuation().is_none() });
    assert!(future.await);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| held.sync_scope(|| panic!("fixture"))));
    assert!(panic.is_err());
    assert!(current_run_continuation().is_none());
}
#[tokio::test]
async fn root_scope_rejects_old_boot_other_target_and_revoked_original() {
    let fixture = Fixture::new(EntryKind::Terminal);
    let command = invocation(&fixture, EntryAction::Plan)
        .consume(EntryAction::Plan, &"a".repeat(64))
        .unwrap();
    let root = plan(&fixture, "bound-root");
    let mut pin = ControlledAdmissionPin {
        version: 1,
        source: fixture.origin().record().clone(),
    };
    let held = command
        .continuation
        .for_store_plan(
            &root,
            &pin,
            &fixture.config.workspace_dir,
            fixture.profile.owner.as_str(),
        )
        .unwrap();
    assert!(held.ensure_root(root.digest()).is_ok());
    assert!(held.for_root(&"f".repeat(64)).is_err());
    assert!(
        held.ensure_active(&fixture.config.workspace_dir, "owner:other")
            .is_err()
    );
    pin.source.boot = "old-boot".into();
    assert!(
        command
            .continuation
            .for_store_plan(
                &root,
                &pin,
                &fixture.config.workspace_dir,
                fixture.profile.owner.as_str()
            )
            .is_err()
    );
    fixture.profile.revoke();
    assert!(held.profile().is_err());
    assert!(
        held.ensure_active(&fixture.config.workspace_dir, fixture.profile.owner.as_str())
            .is_err()
    );
}
#[test]
fn gateway_mutation_without_private_local_owner_stamp_cannot_admit() {
    let fixture = Fixture::new(EntryKind::Gateway);
    let read = invocation(&fixture, EntryAction::Catalog);
    let mut mutation = read.call.clone();
    mutation.action = EntryAction::Publish;
    assert!(EntryInvocation::admit(fixture.origin(), mutation, None).is_err());
}

/// Uses the real recorded Chat issuer and producer binding; never compiled in production.
pub(crate) async fn test_recorded_terminal_scope<T, F, Fut>(work: F) -> T
where
    F: FnOnce(crate::config::Config) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let fixture = Fixture::new(EntryKind::Terminal);
    crate::decision::DecisionKey::load_or_create(&fixture.config.workspace_dir).unwrap();
    let generation = crate::config::new_shared(fixture.config.clone()).pin();
    let memory: Arc<dyn crate::memory::Memory> =
        Arc::new(crate::memory::SqliteMemory::new(fixture.directory.path()).unwrap());
    let (source, _) = crate::chat::decision_source::admission_failure_tests::recorded_approval_source(
        &generation,
        memory,
        "prx controlled planning",
        crate::agent::turn_contracts::ChatMode::Edit,
        "prx_admission_source",
        1,
    )
    .await;
    let mut producer = crate::runtime::h2_entry::origin::tests::producer(fixture.profile.clone());
    let proof = producer
        .submitted("prx-planning-message", "prx controlled planning")
        .unwrap();
    let origin = proof.bind(source.as_ref()).unwrap();
    let source = origin.record();
    let call = EntryCall {
        action: EntryAction::Plan,
        tool: "skills_manage".into(),
        args_sha256: "a".repeat(64),
        call_digest: "b".repeat(64),
        capability: "prx-test-exact-capability".into(),
        capability_generation: "c".repeat(64),
        catalog_generation: "d".repeat(64),
        actor: source.actor.clone(),
        workspace: source.workspace.clone(),
        session: source.session.clone(),
        run: source.run.clone(),
        event: source.event.clone(),
    };
    let invocation = EntryInvocation::admit(origin, call, None).unwrap();
    let command = invocation.consume(EntryAction::Plan, &"a".repeat(64)).unwrap();
    command.continuation_scope(work(fixture.config.clone())).await
}

#[tokio::test]
async fn invocation_scope_is_not_inherited_by_spawn_and_one_port_cannot_execute_twice() {
    let fixture = Fixture::new(EntryKind::Terminal);
    let invocation = invocation(&fixture, EntryAction::Catalog);
    let cancel = tokio_util::sync::CancellationToken::new();
    scope_invocation(Some((invocation.clone(), cancel)), async {
        assert!(current_invocation().is_ok());
        assert!(tokio::spawn(async { current_invocation().is_err() }).await.unwrap());
    })
    .await;
    assert!(current_invocation().is_err());
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let threads = [(); 2].map(|()| {
        let invocation = invocation.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            invocation.consume(EntryAction::Catalog, &"a".repeat(64)).is_ok()
        })
    });
    barrier.wait();
    assert_eq!(
        threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .filter(|accepted| *accepted)
            .count(),
        1
    );
    // Catalog does not allocate/claim an execution root.
    assert_eq!(fixture.claimed_roots(), 0);
}
#[test]
fn public_request_changes_cannot_hide_behind_runtime_metadata_stripping() {
    let original = serde_json::json!({"action":"lifecycle_experiment_catalog","request":{}});
    let changed =
        serde_json::json!({"action":"lifecycle_experiment_catalog","request":{"after_skill":"another-skill"}});
    assert_ne!(
        arguments_digest(&original).unwrap(),
        arguments_digest(&changed).unwrap()
    );
    let forged = serde_json::json!({"action":"lifecycle_experiment_catalog","request":{"owner":"forged"}});
    assert_ne!(arguments_digest(&original).unwrap(), arguments_digest(&forged).unwrap());
    assert!(
        serde_json::from_value::<crate::skills::lifecycle::ExperimentCatalogRequest>(
            forged.get("request").unwrap().clone()
        )
        .is_err()
    );
}

pub(crate) fn test_scope_root_claims() -> usize {
    let current = current_run_continuation().unwrap();
    crate::runtime::h2_entry::profile::tests::claimed_roots(&current.profile().unwrap())
}
/// Installs only an exact private test port from an already held real source.
pub(crate) async fn test_exact_call_scope<F: std::future::Future>(args: &serde_json::Value, work: F) -> F::Output {
    let current = current_run_continuation().unwrap();
    let origin = Arc::new(VerifiedEntryOrigin {
        profile: current.profile().unwrap(),
        record: current.body.source.clone(),
    });
    let source = origin.record();
    let call = EntryCall {
        action: EntryAction::parse(args.get("action").unwrap().as_str().unwrap()).unwrap(),
        tool: "skills_manage".into(),
        args_sha256: arguments_digest(args).unwrap(),
        call_digest: "1".repeat(64),
        capability: "prx-typed-test-call".into(),
        capability_generation: "2".repeat(64),
        catalog_generation: "3".repeat(64),
        actor: source.actor.clone(),
        workspace: source.workspace.clone(),
        session: source.session.clone(),
        run: source.run.clone(),
        event: source.event.clone(),
    };
    let invocation = EntryInvocation::admit(origin, call, None).unwrap();
    scope_invocation(Some((invocation, tokio_util::sync::CancellationToken::new())), work).await
}

/// Observe the real task locals at destruction without adding an authority producer.
struct PendingScopeDrop(Arc<parking_lot::Mutex<Vec<(bool, bool)>>>);
impl std::future::Future for PendingScopeDrop {
    type Output = ();
    fn poll(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<()> {
        std::task::Poll::Pending
    }
}
impl Drop for PendingScopeDrop {
    fn drop(&mut self) {
        self.0
            .lock()
            .push((current_run_continuation().is_some(), current_invocation().is_ok()));
    }
}

#[test]
fn boxed_scope_unpolled_and_polled_drop_preserve_original_task_local_boundaries() {
    let fixture = Fixture::new(EntryKind::Terminal);
    let admitted = invocation(&fixture, EntryAction::Catalog);
    let command = admitted.consume(EntryAction::Catalog, &"a".repeat(64)).unwrap();
    let drops = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let make_work = || {
        scope_invocation(
            Some((Arc::clone(&admitted), tokio_util::sync::CancellationToken::new())),
            command.continuation_scope(command.continuation.scope(retain_run_continuation(
                Some(command.continuation.clone()),
                PendingScopeDrop(Arc::clone(&drops)),
            ))),
        )
    };
    let unpolled = make_work();
    assert!(current_run_continuation().is_none());
    assert!(current_invocation().is_err());
    drop(unpolled);
    assert_eq!(*drops.lock(), vec![(false, false)]);

    let mut started = Box::pin(make_work());
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(std::future::Future::poll(started.as_mut(), &mut context).is_pending());
    assert!(current_run_continuation().is_none());
    assert!(current_invocation().is_err());
    drop(started);
    assert_eq!(*drops.lock(), vec![(false, false), (true, true)]);
    assert!(current_run_continuation().is_none());
    assert!(current_invocation().is_err());
    assert!(admitted.consume(EntryAction::Catalog, &"a".repeat(64)).is_err());
}

#[tokio::test]
async fn retained_source_survives_delayed_poll_but_revocation_before_use_still_refuses() {
    let fixture = Fixture::new(EntryKind::Terminal);
    let command = invocation(&fixture, EntryAction::Catalog)
        .consume(EntryAction::Catalog, &"a".repeat(64))
        .unwrap();
    let held = command.continuation;
    let expected_boot = held.body.source.boot.clone();
    let captured = retain_run_continuation(Some(held.clone()), async {
        current_run_continuation().unwrap().body.source.boot.clone()
    });
    let other_fixture = Fixture::new(EntryKind::Terminal);
    let other = invocation(&other_fixture, EntryAction::Catalog)
        .consume(EntryAction::Catalog, &"a".repeat(64))
        .unwrap();
    assert_ne!(expected_boot, other.continuation.body.source.boot);
    assert_eq!(other.continuation_scope(captured).await, expected_boot);
    assert!(current_run_continuation().is_none());

    let delayed = held.scope(async {
        current_run_continuation()
            .unwrap()
            .ensure_active(&fixture.config.workspace_dir, fixture.profile.owner.as_str())
    });
    fixture.profile.revoke();
    assert!(delayed.await.is_err());
    assert!(current_run_continuation().is_none());
}
