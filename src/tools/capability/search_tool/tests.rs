use super::*;
use crate::tools::capability::catalog::CatalogSources;
use std::sync::atomic::{AtomicUsize, Ordering};

struct OfferTool(Arc<AtomicUsize>);
#[async_trait]
impl Tool for OfferTool {
    fn name(&self) -> &str {
        "prx_offer"
    }
    fn description(&self) -> &str {
        "PRX searchable offer"
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{}})
    }
    async fn execute(&self, _: Value) -> anyhow::Result<ToolResult> {
        self.0.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("discovery cannot dispatch")
    }
}

#[tokio::test]
async fn discovery_scope_publishes_offers_without_execution_authority() -> anyhow::Result<()> {
    let calls = Arc::new(AtomicUsize::new(0));
    let tools: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(OfferTool(Arc::clone(&calls)))]);
    let workspace = tempfile::tempdir()?;
    let security = Arc::new(crate::security::SecurityPolicy::from_config(
        &crate::config::Config::default().autonomy,
        workspace.path(),
    ));
    use crate::tools::execution::{
        AdapterOwnedPreparation, DenyApprovalStrategy, SecurityEffectPolicy, ToolExecutionService,
        TracingToolExecutionAudit,
    };
    let descriptors = Arc::new(ToolExecutionService::from_shared_boxed_registry(
        Arc::clone(&tools),
        Arc::new(SecurityEffectPolicy::new(security)),
        Arc::new(DenyApprovalStrategy),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    ));
    let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let catalog = Arc::new(CapabilityCatalog::build(
        &tools,
        &CatalogSources::default(),
        &SurfacePolicy::default(),
    )?);
    let view = catalog.view(&CatalogScope::default());
    let entry = view
        .iter_entries()
        .find(|entry| entry.public_name == "prx_offer")
        .context("fixture entry")?;
    let reference = CapabilityRef {
        id: entry.id.clone(),
        fingerprint: entry.fingerprint,
        generation: entry.generation,
    };
    let offers = Arc::new(parking_lot::Mutex::new(DiscoveryOffers::default()));
    let scope = DiscoveryScope::published(
        Arc::clone(&catalog),
        SurfacePolicy::default(),
        BTreeMap::from([(
            entry.id.clone(),
            DiscoveryEntry {
                reference: reference.clone(),
                core: false,
                searchable: true,
            },
        )]),
        Arc::downgrade(&offers),
        Arc::downgrade(&closed),
        Arc::downgrade(&descriptors),
    );
    let port = CapabilitySearchPort::new(Arc::new(scope));
    let result = with_search_authority(
        Some(port.clone()),
        CapabilitySearchTool.execute(json!({"query":"prx_offer"})),
    )
    .await?;
    assert!(result.success);
    let result: Value = serde_json::from_str(&result.output)?;
    assert_eq!(result.get("hits").and_then(Value::as_array).context("hits")?.len(), 1);
    assert_eq!(offers.lock().find("prx_offer"), Some(reference));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    closed.store(true, Ordering::Release);
    assert!(
        with_search_authority(
            Some(port.clone()),
            CapabilitySearchTool.execute(json!({"query":"prx_offer"}))
        )
        .await
        .is_err()
    );
    closed.store(false, Ordering::Release);
    offers.lock().clear();
    assert!(
        with_search_authority(Some(port), CapabilitySearchTool.execute(json!({"query":"prx_offer"})))
            .await
            .is_err()
    );
    assert!(
        CapabilitySearchTool
            .execute(json!({"query":"prx_offer"}))
            .await
            .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    Ok(())
}
