//! Shared in-process parent limits and monetary ownership, without admission.
use super::recursive_allowance::RecursiveAllowance;
use crate::causal_tree::branch::{CapabilityRef, CatalogGeneration};
use crate::decision::key::DecisionKey;
use crate::tools::capability::child::{AuthorityEnvelope, CapabilityConstraints, ParentAuthorityId};
use std::{collections::BTreeSet, sync::Arc};

pub(crate) struct ParentAuthority {
    pub(crate) constraints: CapabilityConstraints,
    pub(crate) allowance: Arc<RecursiveAllowance>,
}

impl ParentAuthority {
    pub(crate) fn ensure_child_available(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.constraints.authority.remaining_depth > 0,
            "parent child depth is exhausted"
        );
        anyhow::ensure!(
            self.allowance.funded_money().is_some(),
            "parent child allowance is unavailable"
        );
        Ok(())
    }

    pub(crate) fn for_run(
        run: &str,
        allowed: BTreeSet<CapabilityRef>,
        authority: AuthorityEnvelope,
        catalog_generation: CatalogGeneration,
        key: Arc<DecisionKey>,
        allowance: Arc<RecursiveAllowance>,
    ) -> Self {
        Self {
            constraints: CapabilityConstraints {
                parent_authority_id: ParentAuthorityId::for_run(run, &key),
                allowed,
                authority,
                catalog_generation,
                key,
            },
            allowance,
        }
    }
}
