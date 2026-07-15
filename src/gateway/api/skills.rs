use super::AppState;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::security::policy::ResourceRiskLevel;

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
    let skills = crate::skills::load_skills_with_config(&config.workspace_dir, &config);

    let items: Vec<SkillInfo> = skills
        .into_iter()
        .map(|s| SkillInfo {
            name: s.name,
            description: s.description,
            location: s.location.map(|p| p.display().to_string()).unwrap_or_default(),
            enabled: true,
        })
        .collect();

    Json(SkillsResponse { skills: items })
}

/// PATCH /api/skills/{id}/toggle
pub async fn toggle_skill(
    State(_state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    Ok(Json(serde_json::json!({
        "status": "toggled",
        "id": id,
        "note": "Skill toggle is not yet persisted. Restart will reset state."
    })))
}

// ---------------------------------------------------------------------------
// GET /api/skills/discover
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct DiscoverQuery {
    #[serde(default = "default_source")]
    pub source: String,
    #[serde(default)]
    pub query: Option<String>,
}

fn default_source() -> String {
    "github".into()
}

#[derive(Serialize)]
pub struct DiscoverResponse {
    results: Vec<DiscoverResult>,
}

#[derive(Serialize)]
pub struct DiscoverResult {
    name: String,
    url: String,
    description: String,
    stars: u64,
    language: Option<String>,
    source: String,
    owner: String,
    has_license: bool,
}

pub async fn discover_skills(
    State(state): State<AppState>,
    Query(params): Query<DiscoverQuery>,
) -> Result<Json<DiscoverResponse>, (StatusCode, Json<serde_json::Value>)> {
    use crate::skillforge::scout::{GitHubScout, Scout, ScoutSource};

    let source: ScoutSource = params.source.parse().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("Unsupported skill discovery source '{}'", params.source)
            })),
        )
    })?;

    let results = match source {
        ScoutSource::GitHub => {
            let _config = state.config.lock().clone();
            // Try to get github token from env or config
            let token = std::env::var("GITHUB_TOKEN").ok();
            let mut scout = match GitHubScout::new(token) {
                Ok(s) => s,
                Err(e) => {
                    return Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({"error": e.to_string()})),
                    ));
                }
            };
            if let Some(ref q) = params.query {
                scout.set_queries(vec![q.clone()]);
            }
            match scout.discover().await {
                Ok(items) => items,
                Err(e) => {
                    warn!(error = %e, "GitHub scout failed");
                    return Err((
                        StatusCode::BAD_GATEWAY,
                        Json(serde_json::json!({"error": format!("GitHub search failed: {e}")})),
                    ));
                }
            }
        }
        ScoutSource::ClawHub | ScoutSource::HuggingFace => {
            // Not yet implemented
            vec![]
        }
    };

    let items: Vec<DiscoverResult> = results
        .into_iter()
        .map(|r| DiscoverResult {
            name: r.name,
            url: r.url,
            description: r.description,
            stars: r.stars,
            language: r.language,
            source: format!("{:?}", r.source),
            owner: r.owner,
            has_license: r.has_license,
        })
        .collect();

    Ok(Json(DiscoverResponse { results: items }))
}

// ---------------------------------------------------------------------------
// POST /api/skills/install
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct InstallRequest {
    url: String,
    name: String,
}

#[derive(Serialize)]
pub struct InstallResponse {
    status: String,
    skill: Option<InstalledSkillInfo>,
    error: Option<String>,
}

#[derive(Serialize)]
pub struct InstalledSkillInfo {
    name: String,
    description: String,
    location: String,
}

/// Validate that a skill install URL targets an allowed host (strict host match).
fn is_allowed_skill_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or("");
    // Reject userinfo in authority (e.g. "user@host")
    if authority.contains('@') {
        return false;
    }
    // Strip port to isolate the hostname
    let host = authority.split(':').next().unwrap_or("");
    matches!(host, "github.com" | "huggingface.co")
}

pub async fn install_skill(
    State(state): State<AppState>,
    Json(req): Json<InstallRequest>,
) -> Result<Json<InstallResponse>, (StatusCode, Json<serde_json::Value>)> {
    // Validate name (no path traversal)
    if let Err(error) = crate::skills::validate_skill_name(&req.name) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": error.to_string()})),
        ));
    }

    // Validate URL — parse host strictly to prevent prefix-based bypass
    // (e.g. "https://github.com.evil.example/...")
    if !is_allowed_skill_url(&req.url) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Only GitHub and HuggingFace HTTPS URLs are supported"})),
        ));
    }

    super::authorize_resource_mutation(&state, "gateway_api:skills:install", ResourceRiskLevel::Low)?;

    let config = state.config.lock().clone();
    let skills_dir = config.workspace_dir.join("skills");

    // Create skills directory if it doesn't exist
    if let Err(e) = std::fs::create_dir_all(&skills_dir) {
        warn!(error = %e, "Failed to create skills directory");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "Failed to create skills directory"})),
        ));
    }

    let (staging_dir, target_dir) = crate::skills::skill_staging_paths(&skills_dir, &req.name).map_err(|error| {
        (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": error.to_string()})),
        )
    })?;

    // This explicit mutation endpoint is a control-plane operation. Clone into
    // an inactive same-filesystem directory, validate, then atomically rename.
    let output = tokio::process::Command::new("git")
        .args(["clone", "--depth", "1", &req.url])
        .arg(&staging_dir)
        .output()
        .await;

    match output {
        Ok(out) if out.status.success() => {
            let activation = crate::skills::mark_staged_skill_untrusted(&staging_dir, &req.url)
                .and_then(|()| crate::skills::activate_staged_skill(&staging_dir, &target_dir, &config.workspace_dir));
            if let Err(error) = activation {
                crate::skills::cleanup_staged_skill(&staging_dir);
                warn!(name = req.name.as_str(), error = %error, "staged skill validation or activation failed");
                return Err((
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(serde_json::json!({"error": error.to_string()})),
                ));
            }
            info!(name = req.name.as_str(), "Skill installed successfully");

            let description = crate::skills::load_skills_with_config(&config.workspace_dir, &config)
                .into_iter()
                .find(|skill| skill.name.eq_ignore_ascii_case(&req.name))
                .map(|skill| skill.description)
                .unwrap_or_default();

            Ok(Json(InstallResponse {
                status: "ok".into(),
                skill: Some(InstalledSkillInfo {
                    name: req.name,
                    description,
                    location: target_dir.display().to_string(),
                }),
                error: None,
            }))
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            warn!(name = req.name.as_str(), stderr = stderr.as_str(), "git clone failed");
            crate::skills::cleanup_staged_skill(&staging_dir);
            Err((
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": format!("git clone failed: {stderr}")})),
            ))
        }
        Err(e) => {
            crate::skills::cleanup_staged_skill(&staging_dir);
            warn!(error = %e, "Failed to run git");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Failed to run git clone"})),
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// DELETE /api/skills/{name}
// ---------------------------------------------------------------------------

pub async fn uninstall_skill(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    // Validate name
    if let Err(error) = crate::skills::validate_skill_name(&name) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": error.to_string()})),
        ));
    }

    super::authorize_resource_mutation(&state, "gateway_api:skills:uninstall", ResourceRiskLevel::Low)?;

    let config = state.config.lock().clone();
    let skills_dir = config.workspace_dir.join("skills");
    let target_dir = skills_dir.join(&name);

    if std::fs::symlink_metadata(&target_dir).is_err() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Skill not found"})),
        ));
    }

    let removal = std::fs::symlink_metadata(&target_dir).and_then(|metadata| {
        if metadata.file_type().is_symlink() || metadata.is_file() {
            std::fs::remove_file(&target_dir)
        } else {
            std::fs::remove_dir_all(&target_dir)
        }
    });
    match removal {
        Ok(()) => {
            crate::skills::invalidate_skill_catalog(&config.workspace_dir);
            info!(name = name.as_str(), "Skill uninstalled");
            Ok(Json(serde_json::json!({
                "status": "ok",
                "name": name
            })))
        }
        Err(e) => {
            warn!(name = name.as_str(), error = %e, "Failed to remove skill directory");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Failed to remove skill"})),
            ))
        }
    }
}
