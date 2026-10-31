//! Web Console API routes for the WASM plugin system.
//!
//! - `GET /api/plugins` — list all loaded plugins
//! - `POST /api/plugins/{name}/reload` — reload a plugin by name
//! - `POST /api/plugins/{name}/enable` — move a disabled plugin back into service
//! - `POST /api/plugins/{name}/disable` — move an installed plugin out of service
//! - `POST /api/plugins/refresh` — rebuild every eligible plugin
//! - `POST /api/plugins/install` and `POST /api/plugins/update` — trust one reviewed artifact
//! - `DELETE /api/plugins/{name}` and `POST /api/plugins/{name}/remove` — remove a plugin

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::super::AppState;
use crate::security::policy::ResourceRiskLevel;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EmptyBody {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InstallBody {
    source: String,
    sha256: String,
    manifest_sha256: String,
    permissions: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct ConfirmQuery {
    #[serde(default)]
    confirm_high_risk: bool,
}

fn unavailable() -> (StatusCode, Json<Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": "WASM plugin system not available"})),
    )
}

fn bad_request(error: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (StatusCode::BAD_REQUEST, Json(json!({"error": error.to_string()})))
}

fn confirm_high_risk(confirmed: bool) -> Result<(), (StatusCode, Json<Value>)> {
    if confirmed {
        Ok(())
    } else {
        Err((
            StatusCode::PRECONDITION_REQUIRED,
            Json(json!({
                "error": "high-risk plugin change requires confirmation",
                "required": "confirm_high_risk",
                "risk": "high"
            })),
        ))
    }
}

/// Map a manage-tool failure onto the HTTP status the console can show.
///
/// Rollback text is matched before "already", and "already" before "not
/// installed", because a rollback message can also say the refresh failed.
fn tool_error_status(error: &str) -> StatusCode {
    let lower = error.to_ascii_lowercase();
    if lower.contains("read-only") || lower.contains("security policy") || lower.contains("approval") {
        StatusCode::FORBIDDEN
    } else if lower.contains("rolled back") || lower.contains("refresh failed") {
        StatusCode::INTERNAL_SERVER_ERROR
    } else if lower.contains("already enabled")
        || lower.contains("already installed")
        || lower.contains("already exists")
    {
        StatusCode::CONFLICT
    } else if lower.contains("not found") || lower.contains("not installed") {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::BAD_REQUEST
    }
}

#[cfg(feature = "wasm-plugins")]
async fn execute_manage(
    state: &AppState,
    runtime: &std::sync::Arc<crate::plugins::PluginRuntime>,
    args: Value,
) -> Result<Value, (StatusCode, Json<Value>)> {
    let config = state.config.load_full();
    let security = crate::runtime::bootstrap::build_security_policy(&config);
    let tool = runtime.manage_tool(security);
    let result = tool.execute(args).await.map_err(|error| bad_request(error))?;
    if !result.success {
        let message = result.error.unwrap_or_else(|| "plugin action failed".to_string());
        let status = tool_error_status(&message);
        return Err((status, Json(json!({"error": message}))));
    }
    serde_json::from_str(&result.output).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "plugin action returned a non-JSON result"})),
        )
    })
}

#[cfg(feature = "wasm-plugins")]
fn review_hashes(runtime: &crate::plugins::PluginRuntime, body: &InstallBody) -> Result<(), (StatusCode, Json<Value>)> {
    let source = runtime
        .resolve_install_source(&body.source)
        .map_err(|error| bad_request(error))?;
    let material = crate::plugins::trust::PluginMaterial::read(&source).map_err(|error| bad_request(error))?;
    if material.wasm_bytes.is_none() {
        return Err(bad_request("plugin component is missing"));
    }
    // Compared one field at a time. The request names the component hash
    // `sha256`, which is not the same identifier as `wasm_sha256`.
    if material.wasm_sha256 != body.sha256 {
        return Err(bad_request("plugin hashes differ from the reviewed artifacts"));
    }
    if material.manifest_sha256 != body.manifest_sha256 {
        return Err(bad_request("plugin hashes differ from the reviewed artifacts"));
    }
    Ok(())
}

/// GET /api/plugins — list all loaded plugins with status.
pub async fn list_plugins(State(state): State<AppState>) -> impl IntoResponse {
    #[cfg(feature = "wasm-plugins")]
    {
        if let Some(ref runtime) = state.plugin_runtime {
            let plugins = runtime.list_plugins().await;
            return (
                StatusCode::OK,
                Json(json!({
                    "plugins": plugins,
                    "count": plugins.len(),
                    "generation": runtime.generation_id(),
                })),
            );
        }
    }

    // Feature not enabled or no plugin manager
    let _ = state; // suppress unused warning when feature is off
    (
        StatusCode::OK,
        Json(json!({
            "plugins": [],
            "count": 0,
            "note": "WASM plugin system not enabled (compile with --features wasm-plugins)",
        })),
    )
}

/// POST /api/plugins/{name}/reload — reload a specific plugin.
pub async fn reload_plugin(State(state): State<AppState>, Path(name): Path<String>) -> impl IntoResponse {
    if let Err(error) = super::authorize_resource_mutation(&state, "gateway_api:plugins:reload", ResourceRiskLevel::Low)
    {
        return error;
    }

    #[cfg(feature = "wasm-plugins")]
    {
        if let Some(ref runtime) = state.plugin_runtime {
            return match runtime.reload_plugin(&name).await {
                Ok(generation) => (
                    StatusCode::OK,
                    Json(json!({
                        "success": true,
                        "message": format!("plugin '{name}' reloaded"),
                        "generation": generation,
                    })),
                ),
                Err(e) => (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "success": false,
                        "error": e.to_string(),
                    })),
                ),
            };
        }
    }

    let _ = (state, name);
    unavailable()
}

/// POST /api/plugins/{name}/enable — enable one disabled plugin. `{}` only.
pub async fn enable_plugin(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(_body): Json<EmptyBody>,
) -> impl IntoResponse {
    mutate_named(
        &state,
        "gateway_api:plugins:enable",
        ResourceRiskLevel::Low,
        "enable",
        name,
    )
    .await
}

/// POST /api/plugins/{name}/disable — disable one installed plugin. `{}` only.
pub async fn disable_plugin(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(_body): Json<EmptyBody>,
) -> impl IntoResponse {
    mutate_named(
        &state,
        "gateway_api:plugins:disable",
        ResourceRiskLevel::Low,
        "disable",
        name,
    )
    .await
}

/// POST /api/plugins/refresh — rebuild every eligible plugin. Not first discovery.
pub async fn refresh_plugins(State(state): State<AppState>, Json(_body): Json<EmptyBody>) -> impl IntoResponse {
    if let Err(error) =
        super::authorize_resource_mutation(&state, "gateway_api:plugins:refresh", ResourceRiskLevel::Low)
    {
        return error;
    }
    #[cfg(feature = "wasm-plugins")]
    {
        if let Some(runtime) = state.plugin_runtime.clone() {
            return match execute_manage(&state, &runtime, json!({"action": "refresh"})).await {
                Ok(value) => (StatusCode::OK, Json(value)),
                Err(error) => error,
            };
        }
    }
    let _ = state;
    unavailable()
}

/// POST /api/plugins/install — review hashes, then let install record trust.
///
/// Prior trust is option A: one operator action. This handler checks the
/// submitted hashes and does not call `policy.record`. `PluginManageTool`'s
/// install records trust after a successful compile. A prior `prx plugin trust`
/// record is not required.
pub async fn install_plugin(
    State(state): State<AppState>,
    Query(query): Query<ConfirmQuery>,
    Json(body): Json<InstallBody>,
) -> impl IntoResponse {
    install_or_update(state, body, false, query.confirm_high_risk).await
}

/// POST /api/plugins/update — same prior-trust rule as install.
pub async fn update_plugin(
    State(state): State<AppState>,
    Query(query): Query<ConfirmQuery>,
    Json(body): Json<InstallBody>,
) -> impl IntoResponse {
    install_or_update(state, body, true, query.confirm_high_risk).await
}

/// DELETE /api/plugins/{name} — remove a plugin. High risk.
pub async fn remove_plugin(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<ConfirmQuery>,
) -> impl IntoResponse {
    remove_named(state, name, query.confirm_high_risk).await
}

/// POST /api/plugins/{name}/remove — same removal as DELETE. `{}` only.
pub async fn remove_plugin_post(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<ConfirmQuery>,
    Json(_body): Json<EmptyBody>,
) -> impl IntoResponse {
    remove_named(state, name, query.confirm_high_risk).await
}

async fn mutate_named(
    state: &AppState,
    operation: &str,
    risk: ResourceRiskLevel,
    action: &str,
    name: String,
) -> (StatusCode, Json<Value>) {
    if let Err(error) = super::authorize_resource_mutation(state, operation, risk) {
        return error;
    }
    #[cfg(feature = "wasm-plugins")]
    {
        if let Some(runtime) = state.plugin_runtime.clone() {
            return match execute_manage(state, &runtime, json!({"action": action, "name": name})).await {
                Ok(value) => (StatusCode::OK, Json(value)),
                Err(error) => error,
            };
        }
    }
    let _ = (state, action, name);
    unavailable()
}

async fn remove_named(state: AppState, name: String, confirmed: bool) -> (StatusCode, Json<Value>) {
    if let Err(error) =
        super::authorize_resource_mutation(&state, "gateway_api:plugins:remove", ResourceRiskLevel::High)
    {
        return error;
    }
    #[cfg(feature = "wasm-plugins")]
    {
        if let Some(runtime) = state.plugin_runtime.clone() {
            if let Err(error) = confirm_high_risk(confirmed) {
                return error;
            }
            return match execute_manage(&state, &runtime, json!({"action": "remove", "name": name})).await {
                Ok(value) => (StatusCode::OK, Json(value)),
                Err(error) => error,
            };
        }
    }
    let _ = (state, name, confirmed);
    unavailable()
}

/// Install or update after authorization, a live runtime, confirmation, and
/// the hash review. The manage tool is the only trust writer.
async fn install_or_update(
    state: AppState,
    body: InstallBody,
    update: bool,
    confirmed: bool,
) -> (StatusCode, Json<Value>) {
    let operation = if update {
        "gateway_api:plugins:update"
    } else {
        "gateway_api:plugins:install"
    };
    if let Err(error) = super::authorize_resource_mutation(&state, operation, ResourceRiskLevel::High) {
        return error;
    }
    #[cfg(feature = "wasm-plugins")]
    {
        if let Some(runtime) = state.plugin_runtime.clone() {
            if let Err(error) = confirm_high_risk(confirmed) {
                return error;
            }
            if let Err(error) = review_hashes(&runtime, &body) {
                return error;
            }
            let action = if update { "update" } else { "install" };
            return match execute_manage(
                &state,
                &runtime,
                json!({
                    "action": action,
                    "source": body.source,
                    "grant_permissions": body.permissions,
                }),
            )
            .await
            {
                Ok(value) => (StatusCode::OK, Json(value)),
                Err(error) => error,
            };
        }
    }
    let _ = (state, body, update, confirmed);
    unavailable()
}

#[cfg(test)]
mod tests {
    use super::super::test_support::serve_config;
    use super::tool_error_status;
    use axum::http::StatusCode;

    #[test]
    fn plugin_action_errors_map_to_http_status() {
        assert_eq!(
            tool_error_status("Security policy: read-only mode, cannot perform 'enable'"),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            tool_error_status("plugin 'a' refresh failed and enable was rolled back: boom"),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            tool_error_status("plugin 'a' did not load; enable was rolled back"),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(tool_error_status("plugin 'a' is already enabled"), StatusCode::CONFLICT);
        assert_eq!(
            tool_error_status("plugin 'a' is already installed; use update"),
            StatusCode::CONFLICT
        );
        assert_eq!(
            tool_error_status("disabled plugin 'a' already exists"),
            StatusCode::CONFLICT
        );
        assert_eq!(
            tool_error_status("disabled plugin 'a' was not found"),
            StatusCode::NOT_FOUND
        );
        assert_eq!(tool_error_status("plugin 'a' is not installed"), StatusCode::NOT_FOUND);
        assert_eq!(
            tool_error_status("plugin hashes differ from the reviewed artifacts"),
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn plugin_mutations_are_forbidden_when_autonomy_is_read_only() {
        let served = serve_config(|config| {
            config.autonomy.level = crate::security::AutonomyLevel::ReadOnly;
        })
        .await;
        let install = serde_json::json!({
            "source": "plugin-sources/missing",
            "sha256": "abc",
            "manifest_sha256": "def",
            "permissions": []
        });
        for (method, path, body) in [
            (
                reqwest::Method::POST,
                "/api/plugins/voice-talk-realtime/enable",
                Some(serde_json::json!({})),
            ),
            (
                reqwest::Method::POST,
                "/api/plugins/voice-talk-realtime/disable",
                Some(serde_json::json!({})),
            ),
            (
                reqwest::Method::POST,
                "/api/plugins/refresh",
                Some(serde_json::json!({})),
            ),
            (reqwest::Method::POST, "/api/plugins/install", Some(install.clone())),
            (reqwest::Method::POST, "/api/plugins/update", Some(install)),
            (
                reqwest::Method::POST,
                "/api/plugins/voice-talk-realtime/remove",
                Some(serde_json::json!({})),
            ),
            (reqwest::Method::DELETE, "/api/plugins/voice-talk-realtime", None),
        ] {
            let (status, body) = served.call(method.clone(), path, body).await;
            assert_eq!(status, 403, "{method} {path} {body}");
            assert!(body.contains("read-only mode"), "{body}");
        }
    }

    #[tokio::test]
    async fn plugin_mutations_are_unavailable_without_a_runtime() {
        let served = serve_config(|_| {}).await;
        for (method, path) in [
            (reqwest::Method::POST, "/api/plugins/voice-talk-realtime/enable"),
            (reqwest::Method::POST, "/api/plugins/voice-talk-realtime/disable"),
            (reqwest::Method::POST, "/api/plugins/refresh"),
            (reqwest::Method::POST, "/api/plugins/voice-talk-realtime/remove"),
            (reqwest::Method::DELETE, "/api/plugins/voice-talk-realtime"),
        ] {
            let (status, body) = served.call(method.clone(), path, Some(serde_json::json!({}))).await;
            assert_eq!(status, 503, "{method} {path} {body}");
            assert!(body.contains("WASM plugin system not available"), "{body}");
        }
        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/plugins/install?confirm_high_risk=true",
                Some(serde_json::json!({
                    "source": "plugin-sources/missing",
                    "sha256": "abc",
                    "manifest_sha256": "def",
                    "permissions": []
                })),
            )
            .await;
        assert_eq!(status, 503, "{body}");
    }

    #[tokio::test]
    async fn plugin_enable_rejects_unknown_fields() {
        let served = serve_config(|_| {}).await;
        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/plugins/voice-talk-realtime/enable",
                Some(serde_json::json!({"extra": 1})),
            )
            .await;
        assert_eq!(status, 422, "{body}");
    }
}

#[cfg(test)]
#[cfg(feature = "wasm-plugins")]
mod runtime_tests {
    use std::path::{Path, PathBuf};

    use super::super::test_support::Served;
    use super::super::test_support::{app_state, json, serve_state};
    use crate::config::Config;
    use crate::plugins::PluginPolicy;
    use crate::plugins::trust::PluginMaterial;

    fn copy_fixture(dest: &Path) {
        std::fs::create_dir_all(dest).expect("test: plugin dir");
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/voice-talk-realtime");
        std::fs::copy(fixture.join("plugin.toml"), dest.join("plugin.toml")).expect("test: copy manifest");
        std::fs::copy(fixture.join("plugin.wasm"), dest.join("plugin.wasm")).expect("test: copy wasm");
    }

    async fn serve_enabled(prepare: impl FnOnce(&Path)) -> (Served, PathBuf) {
        let dir = tempfile::TempDir::new().expect("test: temp");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("test: workspace");
        prepare(&workspace);
        let config = Config {
            config_path: dir.path().join("config.toml"),
            workspace_dir: workspace.clone(),
            default_provider: Some("openai".into()),
            default_model: Some("gpt-4o".into()),
            plugins: crate::config::plugins::PluginsConfig { enabled: true },
            ..Config::default()
        };
        std::fs::write(&config.config_path, "[plugins]\nenabled = true\n").expect("test: config file");
        let runtime = crate::plugins::init_plugin_runtime(&config, None)
            .await
            .expect("test: plugin runtime");
        let mut state = app_state(config);
        state.plugin_runtime = Some(runtime);
        let served = serve_state(dir, state).await;
        (served, workspace)
    }

    fn reviewed_body(source_dir: &Path, source: &str, sha256: &str) -> serde_json::Value {
        let material = PluginMaterial::read(source_dir).expect("test: material");
        serde_json::json!({
            "source": source,
            "sha256": sha256,
            "manifest_sha256": material.manifest_sha256,
            "permissions": material.manifest.permissions.required,
        })
    }

    #[tokio::test]
    async fn plugin_routes_follow_the_manage_tool() {
        let source_rel = "plugin-sources/voice-talk-realtime";
        let (served, workspace) = serve_enabled(|workspace| {
            copy_fixture(&workspace.join(source_rel));
        })
        .await;
        let source_dir = workspace.join(source_rel);
        let material = PluginMaterial::read(&source_dir).expect("test: material");
        let policy = PluginPolicy::from_config(&served.state.config.load_full()).expect("test: policy");
        assert!(
            policy
                .record_for_name("voice-talk-realtime")
                .expect("test: trust read")
                .is_none()
        );

        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/plugins/install",
                Some(reviewed_body(&source_dir, source_rel, "0".repeat(64).as_str())),
            )
            .await;
        assert_eq!(status, 428, "{body}");
        assert_eq!(
            json(&body).get("required").and_then(serde_json::Value::as_str),
            Some("confirm_high_risk"),
            "{body}"
        );
        assert!(
            policy
                .record_for_name("voice-talk-realtime")
                .expect("test: trust read")
                .is_none()
        );
        assert!(!workspace.join("plugins/voice-talk-realtime").exists());

        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/plugins/install?confirm_high_risk=true",
                Some(reviewed_body(&source_dir, source_rel, "not-the-component-hash")),
            )
            .await;
        assert_eq!(status, 400, "{body}");
        assert!(
            body.contains("plugin hashes differ from the reviewed artifacts"),
            "{body}"
        );
        assert!(
            policy
                .record_for_name("voice-talk-realtime")
                .expect("test: trust read")
                .is_none()
        );

        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/plugins/install?confirm_high_risk=true",
                Some(reviewed_body(&source_dir, source_rel, &material.wasm_sha256)),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        let installed = json(&body);
        assert_eq!(
            installed.get("action").and_then(serde_json::Value::as_str),
            Some("install")
        );
        assert!(
            installed.get("generation").is_some_and(serde_json::Value::is_number),
            "{body}"
        );
        assert!(
            policy
                .record_for_name("voice-talk-realtime")
                .expect("test: trust read")
                .is_some()
        );

        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/plugins/install?confirm_high_risk=true",
                Some(reviewed_body(&source_dir, source_rel, &material.wasm_sha256)),
            )
            .await;
        assert_eq!(status, 409, "{body}");

        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/plugins/update?confirm_high_risk=true",
                Some(reviewed_body(&source_dir, source_rel, &material.wasm_sha256)),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            json(&body).get("action").and_then(serde_json::Value::as_str),
            Some("update")
        );

        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/plugins/voice-talk-realtime/enable",
                Some(serde_json::json!({})),
            )
            .await;
        assert_eq!(status, 409, "{body}");

        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/plugins/voice-talk-realtime/disable",
                Some(serde_json::json!({})),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        let disabled = json(&body);
        assert_eq!(
            disabled.get("plugin").and_then(serde_json::Value::as_str),
            Some("voice-talk-realtime")
        );
        assert_eq!(
            disabled.get("enabled").and_then(serde_json::Value::as_bool),
            Some(false)
        );

        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/plugins/voice-talk-realtime/enable",
                Some(serde_json::json!({})),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        let enabled = json(&body);
        assert_eq!(enabled.get("enabled").and_then(serde_json::Value::as_bool), Some(true));
        assert_eq!(
            enabled
                .get("plugin")
                .and_then(|plugin| plugin.get("name"))
                .and_then(serde_json::Value::as_str),
            Some("voice-talk-realtime")
        );

        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/plugins/refresh",
                Some(serde_json::json!({})),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            json(&body).get("refreshed").and_then(serde_json::Value::as_bool),
            Some(true)
        );

        let (status, body) = served
            .call(reqwest::Method::DELETE, "/api/plugins/voice-talk-realtime", None)
            .await;
        assert_eq!(status, 428, "{body}");
        assert!(workspace.join("plugins/voice-talk-realtime").is_dir());

        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/plugins/voice-talk-realtime/remove",
                Some(serde_json::json!({})),
            )
            .await;
        assert_eq!(status, 428, "{body}");

        let (status, body) = served
            .call(
                reqwest::Method::DELETE,
                "/api/plugins/voice-talk-realtime?confirm_high_risk=true",
                None,
            )
            .await;
        assert_eq!(status, 200, "{body}");
        let removed = json(&body);
        assert_eq!(removed.get("removed").and_then(serde_json::Value::as_bool), Some(true));
        assert!(
            removed
                .get("recoverable_backup")
                .is_some_and(serde_json::Value::is_string),
            "{body}"
        );
        assert!(
            policy
                .record_for_name("voice-talk-realtime")
                .expect("test: trust read")
                .is_none()
        );

        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/plugins/missing-plugin/enable",
                Some(serde_json::json!({})),
            )
            .await;
        assert_eq!(status, 404, "{body}");
    }

    #[tokio::test]
    async fn enable_partial_failure_is_an_error_and_rolls_back() {
        let (served, workspace) = serve_enabled(|workspace| {
            let dest = workspace.join(".plugins-disabled/not-the-manifest-name");
            copy_fixture(&dest);
            let config = Config {
                config_path: workspace.parent().expect("test: state dir").join("config.toml"),
                workspace_dir: workspace.to_path_buf(),
                plugins: crate::config::plugins::PluginsConfig { enabled: true },
                ..Config::default()
            };
            let policy = PluginPolicy::from_config(&config).expect("test: policy");
            let material = PluginMaterial::read(&dest).expect("test: material");
            let record = material
                .record_for(&material.manifest.permissions.required)
                .expect("test: record");
            policy.record(record).expect("test: trust");
        })
        .await;

        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/plugins/not-the-manifest-name/enable",
                Some(serde_json::json!({})),
            )
            .await;
        assert_eq!(status, 500, "{body}");
        assert!(body.contains("rolled back"), "{body}");
        assert!(!body.contains("\"enabled\": true"), "{body}");
        assert!(workspace.join(".plugins-disabled/not-the-manifest-name").is_dir());
        assert!(!workspace.join("plugins/not-the-manifest-name").exists());
    }
}
