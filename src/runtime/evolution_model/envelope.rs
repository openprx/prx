//! Runtime-owned shared accounting scope for an evolution experiment.
//! Borrows existing physical accounting; owns no transport or publication.
use super::AccountingScope;
use std::sync::Arc;

pub(crate) struct EvolutionEnvelope {
    pub(super) identity: String,
    scope: parking_lot::Mutex<Option<(String, Arc<AccountingScope>)>>,
}

tokio::task_local! {
    static CURRENT: Arc<EvolutionEnvelope>;
}

impl EvolutionEnvelope {
    pub(crate) fn new(identity: String) -> Arc<Self> {
        Arc::new(Self {
            identity,
            scope: parking_lot::Mutex::new(None),
        })
    }

    pub(crate) fn scope<F: std::future::Future>(
        self: &Arc<Self>,
        work: F,
    ) -> impl std::future::Future<Output = F::Output> + use<F> {
        CURRENT.scope(Arc::clone(self), Box::pin(work))
    }

    pub(super) fn accounting(
        &self,
        binding: String,
        create: impl FnOnce() -> Arc<AccountingScope>,
    ) -> anyhow::Result<Arc<AccountingScope>> {
        let mut scope = self.scope.lock();
        if let Some((pinned, existing)) = scope.as_ref() {
            anyhow::ensure!(pinned == &binding, "experiment accounting configuration changed");
            return Ok(Arc::clone(existing));
        }
        let created = create();
        *scope = Some((binding, Arc::clone(&created)));
        Ok(created)
    }
}

pub(crate) fn current_envelope() -> Option<Arc<EvolutionEnvelope>> {
    CURRENT.try_with(Arc::clone).ok()
}

pub(crate) fn retain_envelope<F: std::future::Future>(
    envelope: Option<Arc<EvolutionEnvelope>>,
    work: F,
) -> impl std::future::Future<Output = F::Output> + use<F> {
    let work = Box::pin(work);
    async move {
        match envelope {
            Some(envelope) => CURRENT.scope(envelope, work).await,
            None => work.await,
        }
    }
}
