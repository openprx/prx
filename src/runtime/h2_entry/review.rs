//! Local control authority of a directly launched controlled Gateway.
use super::ControlledH2Profile;
use anyhow::{Result, ensure};
use std::sync::Arc;

pub(crate) struct LocalControlAuthority {
    profile: Arc<ControlledH2Profile>,
}
impl LocalControlAuthority {
    pub(super) fn from_producer(profile: Arc<ControlledH2Profile>) -> Result<Self> {
        ensure!(
            profile.kind == super::EntryKind::Gateway,
            "local review requires direct Gateway launch"
        );
        profile.ensure_live()?;
        Ok(Self { profile })
    }
    pub(crate) const fn profile(&self) -> &Arc<ControlledH2Profile> {
        &self.profile
    }
}
