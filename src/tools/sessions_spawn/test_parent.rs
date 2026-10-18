use super::{Provider, SessionsSpawnTool};
use std::sync::Arc;

impl SessionsSpawnTool {
    pub(super) fn test_parent_provider(&self) -> &Arc<dyn Provider> {
        self.test_parent_provider
            .as_ref()
            .expect("test parent provider is explicit")
    }

    pub(crate) fn with_test_parent_provider(mut self, provider: Arc<dyn Provider>) -> Self {
        self.test_parent_provider = Some(provider);
        self
    }
}
