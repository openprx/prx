//! Runtime-owned native DATA capability for one admitted case.
use crate::config::Config;
use crate::tools::owner_filesystem::OwnerFilesystem;
use anyhow::{Result, ensure};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Runtime-only construction result; serde cannot manufacture a case scope.
pub(crate) struct ControlledCaseScope {
    filesystem: Arc<OwnerFilesystem>,
    workspace: PathBuf,
}
impl std::fmt::Debug for ControlledCaseScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ControlledCaseScope")
    }
}
impl ControlledCaseScope {
    pub(crate) fn create(private_root: &Path, state: &Path, owner: &str) -> Result<Arc<Self>> {
        let workspace = state.canonicalize()?;
        let filesystem = Arc::new(OwnerFilesystem::open(
            private_root,
            &workspace.to_string_lossy(),
            owner,
            &workspace,
        )?);
        Ok(Arc::new(Self { filesystem, workspace }))
    }
    pub(crate) fn data_root(&self) -> &Path {
        self.filesystem.namespace_root()
    }
    pub(crate) fn verify_config(&self, config: &Config) -> Result<()> {
        crate::runtime::h2_entry::ControlledH2Profile::validate_trial_config(config)?;
        ensure!(
            config.workspace_dir.canonicalize()? == self.workspace,
            "controlled case workspace changed"
        );
        Ok(())
    }
    pub(crate) fn apply(self: &Arc<Self>, context: &mut crate::tools::ToolExecutionContext) -> Result<()> {
        ensure!(
            self.filesystem
                .matches(&context.envelope.workspace_id, &context.envelope.resolved_owner_id()),
            "controlled case execution identity changed"
        );
        context.owner_filesystem = Some(Arc::clone(&self.filesystem));
        context.controlled_case = Some(Arc::clone(self));
        Ok(())
    }
    pub(crate) fn registry(
        &self,
        config: &Config,
        security: &Arc<crate::security::SecurityPolicy>,
    ) -> Result<Vec<Box<dyn crate::tools::Tool>>> {
        self.verify_config(config)?;
        let mut tools: Vec<Box<dyn crate::tools::Tool>> = vec![
            Box::new(crate::tools::SkillReadTool::new(self.workspace.clone(), config.clone())),
            Box::new(crate::tools::capability::search_tool::CapabilitySearchTool),
        ];
        for name in &config.tool_tiering.always_include {
            match name.as_str() {
                "skill_read" => {}
                "file_read" => tools.push(Box::new(crate::tools::FileReadTool::new_controlled(
                    Arc::clone(security),
                    config.memory.acl_enabled,
                    Arc::clone(&self.filesystem),
                ))),
                "file_write" => tools.push(Box::new(crate::tools::FileWriteTool::new_controlled(
                    Arc::clone(security),
                    Arc::clone(&self.filesystem),
                ))),
                "file_edit" => tools.push(Box::new(crate::tools::FileEditTool::new_controlled(
                    Arc::clone(security),
                    Arc::clone(&self.filesystem),
                ))),
                _ => anyhow::bail!("controlled case requested a non-native data adapter"),
            }
        }
        Ok(tools)
    }
}

impl PartialEq for ControlledCaseScope {
    fn eq(&self, other: &Self) -> bool {
        self.workspace == other.workspace && self.filesystem == other.filesystem
    }
}
impl Eq for ControlledCaseScope {}
