use super::*;
#[tokio::test]
async fn disabled_plane_uses_existing_strategy_without_pending() {
    use crate::tools::execution::tests::admission_matrix::{assert_existing_strategy, ledger_context};
    let path = tempfile::tempdir().unwrap();
    let mut context = ledger_context(path.path()).await;
    let observer = context.approval_ledger.take().unwrap();
    let mut config = crate::config::Config::default();
    config.workspace_dir = path.path().to_owned();
    config.causal_tree.enabled = false;
    let generation = crate::config::new_shared(config.clone()).pin();
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(Vec::new());
    let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
        Arc::clone(&registry),
        Arc::new(SecurityEffectPolicy::new(Arc::new(SecurityPolicy::default()))),
        Arc::new(DenyApprovalStrategy),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    ));
    let catalog = ProviderCatalogView::from_config(&config);
    let transports = CountingTransports(AtomicUsize::new(0));
    let mut service = build_turn_decision_service(
        TurnSurfaceKind::Interactive,
        TurnServiceBuild {
            parent_grant: None,
            generation: Arc::clone(&generation),
            registry,
            execution,
            catalog_sources: sources(&generation, &config),
            surface_policy: SurfacePolicy::default(),
            provider_catalog: &catalog,
            transports: &transports,
            events: Arc::new(SilentEvents),
            memory: Arc::clone(&observer.memory),
            security: Arc::new(SecurityPolicy::default()),
        },
    )
    .await
    .unwrap();
    // Isolate the plane guard from the independent durable/recordable guards.
    // A loaded key and durable source must not opt a disabled plane into review.
    let owning = Arc::get_mut(&mut service).unwrap();
    owning.recordable = true;
    owning.durable_key = true;
    owning.state_key = Arc::clone(&observer.state_key);
    context.approval_ledger = service.execution_approval_ledger();
    assert!(
        context.approval_ledger.is_none(),
        "disabled plane must not install an unreviewable ledger"
    );
    assert_existing_strategy(context, observer).await;
}
