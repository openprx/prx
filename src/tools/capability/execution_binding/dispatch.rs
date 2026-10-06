//! Keep the selected target bound across an adapter's internal refresh.
//!
//! This scoped metadata grants no permission. The execution service has already
//! verified its binding; a dynamic adapter must recheck its actual destination
//! after refresh and inside the ownership protecting physical dispatch.

use super::{CapabilityCatalog, CapabilityRef, ExecutionBinding, ToolExecutionError};
use crate::causal_tree::branch::SchemaFingerprint;
use crate::tools::capability::catalog::CatalogScope;
use std::future::Future;
use std::sync::Arc;

struct DispatchTarget {
    catalog: Arc<CapabilityCatalog>,
    capability: CapabilityRef,
}

tokio::task_local! {
    static DISPATCH_TARGET: Option<DispatchTarget>;
}

pub(crate) async fn scope_dispatch_binding<T>(
    binding: Option<&ExecutionBinding>,
    future: impl Future<Output = T>,
) -> T {
    let target = binding.map(|binding| DispatchTarget {
        catalog: Arc::clone(&binding.catalog),
        capability: binding.capability.clone(),
    });
    DISPATCH_TARGET.scope(target, future).await
}

/// Check fresh backend facts without accepting authority from tool arguments.
/// Calls outside the bound execution service keep their existing policy path.
/// A present binding with a missing current entry always fails closed.
pub(crate) fn verify_current_dispatch(
    backend_name: &str,
    public_name: &str,
    fingerprint: Option<SchemaFingerprint>,
    binding_digest: Option<[u8; 32]>,
) -> Result<(), ToolExecutionError> {
    DISPATCH_TARGET
        .try_with(|target| {
            let Some(target) = target else {
                return Ok(());
            };
            let view = target.catalog.view(&CatalogScope::default());
            let expected = view
                .entry_any(&target.capability.id)
                .ok_or(ToolExecutionError::StaleCapability)?;
            if expected.generation != target.capability.generation
                || expected.fingerprint != target.capability.fingerprint
                || expected.public_name != public_name
                || expected.root != backend_name
                || fingerprint != Some(expected.fingerprint)
                || binding_digest != expected.binding_digest
            {
                return Err(ToolExecutionError::StaleCapability);
            }
            Ok(())
        })
        .unwrap_or(Ok(()))
}
