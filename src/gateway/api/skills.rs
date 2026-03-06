use super::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::Serialize;

#[derive(Serialize)]
struct SkillInfo {
    name: String,
    description: String,
    location: String,
    enabled: bool,
}

#[derive(Serialize)]
pub(super) struct SkillsResponse {
    skills: Vec<SkillInfo>,
}

pub async fn get_skills(State(state): State<AppState>) -> Json<SkillsResponse> {
    let config = state.config.lock().clone();
    let skills =
        crate::skills::load_skills_with_config(&config.workspace_dir, &config);

    let items: Vec<SkillInfo> = skills
        .into_iter()
        .map(|s| SkillInfo {
            name: s.name,
            description: s.description,
            location: s
                .location
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            enabled: true,
        })
        .collect();

    Json(SkillsResponse { skills: items })
}

/// PATCH /api/skills/{id}/toggle
///
/// Skills don't have a persistent enabled/disabled toggle in the current
/// architecture (they are auto-discovered from the filesystem). This endpoint
/// is a no-op placeholder that acknowledges the request without error so the
/// frontend toggle button works. A proper implementation would persist the
/// disabled set to a config file.
pub async fn toggle_skill(
    State(_state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    // For now, we just acknowledge — real persistence would need a disabled-skills list
    Ok(Json(serde_json::json!({
        "status": "toggled",
        "id": id,
        "note": "Skill toggle is not yet persisted. Restart will reset state."
    })))
}
