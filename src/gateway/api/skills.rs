use super::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

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
                    ))
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
    if req.name.contains("..") || req.name.contains('/') || req.name.contains('\\') {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Invalid skill name"})),
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

    let target_dir = skills_dir.join(&req.name);
    if target_dir.exists() {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "Skill already installed"})),
        ));
    }

    // Git clone
    let output = tokio::process::Command::new("git")
        .args([
            "clone",
            "--depth",
            "1",
            &req.url,
            &target_dir.display().to_string(),
        ])
        .output()
        .await;

    match output {
        Ok(out) if out.status.success() => {
            info!(name = req.name.as_str(), "Skill installed successfully");

            // Try to read description from SKILL.md
            let description = read_skill_description(&target_dir);

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
            warn!(
                name = req.name.as_str(),
                stderr = stderr.as_str(),
                "git clone failed"
            );
            // Clean up partial clone
            let _ = std::fs::remove_dir_all(&target_dir);
            Err((
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": format!("git clone failed: {stderr}")})),
            ))
        }
        Err(e) => {
            warn!(error = %e, "Failed to run git");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Failed to run git clone"})),
            ))
        }
    }
}

/// Read description from SKILL.md if it exists
fn read_skill_description(dir: &std::path::Path) -> String {
    let skill_md = dir.join("SKILL.md");
    if skill_md.exists() {
        if let Ok(content) = std::fs::read_to_string(&skill_md) {
            // Extract first paragraph or description line
            for line in content.lines() {
                let trimmed = line.trim();
                if !trimmed.is_empty() && !trimmed.starts_with('#') && !trimmed.starts_with("---") {
                    return trimmed.to_string();
                }
            }
        }
    }
    String::new()
}

// ---------------------------------------------------------------------------
// DELETE /api/skills/{name}
// ---------------------------------------------------------------------------

pub async fn uninstall_skill(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    // Validate name
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Invalid skill name"})),
        ));
    }

    let config = state.config.lock().clone();
    let skills_dir = config.workspace_dir.join("skills");
    let target_dir = skills_dir.join(&name);

    if !target_dir.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Skill not found"})),
        ));
    }

    match std::fs::remove_dir_all(&target_dir) {
        Ok(_) => {
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
