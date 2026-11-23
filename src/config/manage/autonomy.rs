//! Typed autonomy selection for the local management CLI.
use super::{ConfigActor, ManageError, ManagedPlan, ManagementService};

impl ManagementService {
    /// Plan a level change under the ordinary revision and high-risk guards.
    pub async fn plan_autonomy(
        &self,
        level: crate::security::AutonomyLevel,
        actor: ConfigActor,
    ) -> Result<ManagedPlan, ManageError> {
        let mut candidate = (*self.config()).clone();
        candidate.autonomy.level = level;
        self.plan_candidate(candidate, self.revision(), actor).await
    }
}
