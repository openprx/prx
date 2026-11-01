//! Channel capability publication from the actual component-owned registry.
use crate::runtime::turn_decision::TurnDecisionService;
use std::collections::BTreeMap;
use std::sync::{Arc, Weak};

pub(super) struct ChannelDecisionSlot {
    mcp_tool: Option<Arc<crate::tools::McpTool>>,
    plugin_generation: u64,
    published: tokio::sync::Mutex<Published>,
}

#[derive(Default)]
struct Published {
    latest: Option<Arc<TurnDecisionService>>,
    generations: BTreeMap<u64, Weak<TurnDecisionService>>,
}

impl ChannelDecisionSlot {
    pub(super) fn new(mcp_tool: Option<Arc<crate::tools::McpTool>>, plugin_generation: u64) -> Self {
        Self {
            mcp_tool,
            plugin_generation,
            published: tokio::sync::Mutex::new(Published::default()),
        }
    }

    pub(super) async fn publish(
        &self,
        ctx: &super::ChannelRuntimeContext,
        generation: &Arc<crate::config::ConfigGeneration>,
        security: &Arc<crate::security::SecurityPolicy>,
        memory: Arc<dyn crate::memory::Memory>,
    ) -> anyhow::Result<Arc<TurnDecisionService>> {
        let owner = crate::runtime::turn_cleanup::current_tracker()
            .ok_or_else(|| anyhow::anyhow!("channel decision publication requires a cleanup owner"))?;
        // Only publication is serialized. Decisions and model turns run without
        // this lock; old generations stay available while their turns own them.
        let mut published = self.published.lock().await;
        published.generations.retain(|_, service| service.strong_count() > 0);
        if let Some(service) = published.generations.get(&generation.id.0).and_then(Weak::upgrade)
            && Arc::ptr_eq(service.memory(), &memory)
        {
            owner.retain_child_service(Arc::clone(&service))?;
            return Ok(service);
        }
        let service = crate::runtime::turn_publication::TurnDecisionPublication {
            generation: Arc::clone(generation),
            registry: Arc::clone(&ctx.tools_registry),
            mcp_tool: self.mcp_tool.clone(),
            security: Arc::clone(security),
            observer: Arc::clone(&ctx.observer),
            hooks: Arc::clone(&ctx.hooks),
            memory: Arc::clone(&memory),
            plugin_generation: self.plugin_generation,
        }
        .publish(crate::causal_tree::state::TurnSurfaceKind::Channel)
        .await?;
        if let Err(error) = owner.retain_child_service(Arc::clone(&service)) {
            service
                .shutdown(std::time::Instant::now() + std::time::Duration::from_secs(2))
                .await;
            return Err(error);
        }
        published.generations.insert(generation.id.0, Arc::downgrade(&service));
        published.latest = Some(Arc::clone(&service));
        Ok(service)
    }
}

#[cfg(test)]
impl Default for ChannelDecisionSlot {
    fn default() -> Self {
        Self::new(None, 0)
    }
}
