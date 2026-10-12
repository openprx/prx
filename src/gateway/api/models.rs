//! Model, default, route and agent-binding endpoints (SP-PMM §2.8,
//! PMM-F-2).
//!
//! | Endpoint | Service call |
//! | --- | --- |
//! | `GET /api/models?provider=&model=&operation=&executable=` | `model_catalog` (`model_entry` when both `provider` and `model` are named) |
//! | `POST`/`PATCH`/`DELETE /api/models` | `ManagedChange::{AddModel, UpdateModel, RemoveModel}` |
//! | `GET /api/defaults` | the chat and embedding defaults |
//! | `PUT /api/defaults` | `ManagedChange::SetDefault` |
//! | `GET`/`POST /api/routes`, `PATCH`/`DELETE /api/routes/{hint}` | `ManagedChange::{AddRoute, UpdateRoute, RemoveRoute}` |
//! | `GET /api/agents`, `PUT /api/agents/{name}` | `ManagedChange::BindAgent` |
//!
//! A model id can contain `/` (`openai/gpt-4o`), so it travels in the query
//! or the body, never in the path. Every write is authorized at Low risk
//! (like `POST /api/config`), planned and committed through the one
//! management service (`manage::apply_change`).

use super::AppState;
use super::manage::{self, OperationParam, Write, WriteQuery, manage_error_response, verbatim};
use crate::config::manage::service::{
    ModelEntry, ModelFilter, model_catalog, model_entry, parse_operation, redact_provider_text,
};
use crate::config::manage::{ManageError, ManagedChange, RootCredential, SelectionOperation};
use crate::config::schema::ProviderModelConfig;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;

/// Unwrap the write options of a request or answer its refusal.
macro_rules! write_of {
    ($query:expr, $headers:expr) => {
        match Write::of(&$query, &$headers) {
            Ok(write) => write,
            Err(refused) => return *refused,
        }
    };
}

/// The query of `GET /api/models`.
#[derive(Deserialize)]
pub struct ModelsQuery {
    /// Only this provider (compared by identity).
    #[serde(default)]
    provider: Option<String>,
    /// Only this model (a model id may contain `/`: percent-encode it).
    #[serde(default)]
    model: Option<String>,
    /// Only models the evidence does not rule out for this operation
    /// (`chat`, `embedding`, ...).
    #[serde(default)]
    operation: Option<String>,
    /// Only models the evidence proves can execute the operation (chat when
    /// none is named).
    #[serde(default)]
    executable: bool,
}

/// `GET /api/models`: the models by operation, from the service's read
/// model (the one `prx models list|show` prints): configured selections,
/// declarations and every cached model, each with what the evidence says
/// about every operation and chat feature, its source and freshness. With
/// both `provider` and `model` it describes that one model, listed or not.
pub async fn get_models(State(state): State<AppState>, Query(query): Query<ModelsQuery>) -> Response {
    let operation = match query.operation.as_deref().map(parse_operation).transpose() {
        Ok(operation) => operation,
        Err(error) => return manage_error_response(error, None, &verbatim),
    };
    let config = manage::service(&state).config();
    // The catalog evidence includes the model cache, a file read.
    let listed = crate::runtime::blocking::spawn_blocking(move || -> Result<Vec<ModelEntry>, ManageError> {
        let ModelsQuery {
            provider,
            model,
            executable,
            ..
        } = query;
        if let (Some(provider), Some(model)) = (provider.as_deref(), model.as_deref()) {
            return model_entry(&config, provider, model).map(|entry| vec![entry]);
        }
        let mut entries = model_catalog(
            &config,
            &ModelFilter {
                provider,
                operation,
                executable,
            },
        );
        if let Some(model) = model.as_deref().map(str::trim) {
            entries.retain(|entry| entry.model.eq_ignore_ascii_case(model));
        }
        Ok(entries)
    })
    .await
    .unwrap_or_else(|error| {
        Err(ManageError::Invalid(format!(
            "the model catalog reader failed ({error})"
        )))
    });
    match listed {
        Ok(models) => Json(json!({"models": models})).into_response(),
        Err(error) => manage_error_response(error, None, &verbatim),
    }
}

/// `POST /api/models`: add a curated `[[providers.models]]` declaration.
/// Body: the declaration (`provider`, optional `model`, and at least one
/// fact: `operations`, a feature, a dialect or `context_window`).
pub async fn post_model(
    State(state): State<AppState>,
    Query(query): Query<WriteQuery>,
    headers: HeaderMap,
    Json(declaration): Json<ProviderModelConfig>,
) -> Response {
    let write = write_of!(query, headers);
    manage::apply_change(&state, ManagedChange::AddModel(declaration), write).await
}

/// `PATCH /api/models`: change the declaration the body's `provider` and
/// `model` name; every other field the body declares replaces the stored one.
pub async fn patch_model(
    State(state): State<AppState>,
    Query(query): Query<WriteQuery>,
    headers: HeaderMap,
    Json(declaration): Json<ProviderModelConfig>,
) -> Response {
    let write = write_of!(query, headers);
    manage::apply_change(&state, ManagedChange::UpdateModel(declaration), write).await
}

/// The key of a declaration: its provider and model (none: the
/// provider-wide declaration).
#[derive(Deserialize)]
pub struct ModelKey {
    provider: String,
    #[serde(default)]
    model: Option<String>,
}

/// `DELETE /api/models`: remove the declaration the body names.
pub async fn delete_model(
    State(state): State<AppState>,
    Query(query): Query<WriteQuery>,
    headers: HeaderMap,
    Json(key): Json<ModelKey>,
) -> Response {
    let write = write_of!(query, headers);
    manage::apply_change(
        &state,
        ManagedChange::RemoveModel {
            provider: key.provider,
            model: key.model,
        },
        write,
    )
    .await
}

/// `GET /api/defaults`: the default chat selection (with its resolver
/// status) and the default embedder.
pub async fn get_defaults(State(state): State<AppState>) -> Response {
    let service = manage::service(&state);
    let config = service.config();
    let (verified, status) = match crate::providers::selection::default_status(&config) {
        Ok((verified, line)) => (Some(verified), line),
        Err(refusal) => (None, refusal),
    };
    Json(json!({
        "chat": {
            "provider": redact_provider_text(crate::providers::selection::default_provider(&config)),
            "model": crate::providers::selection::default_model(&config),
            "resolved": verified.is_some(),
            "verified": verified.unwrap_or(false),
            "status": redact_provider_text(&status),
        },
        "embedding": {
            "provider": redact_provider_text(&config.memory.embedding_provider),
            "model": config.memory.embedding_model,
        },
        "revision": service.revision(),
    }))
    .into_response()
}

/// The body of `PUT /api/defaults`.
#[derive(Deserialize)]
pub struct DefaultBody {
    #[serde(default)]
    operation: OperationParam,
    provider: String,
    model: String,
    /// What happens to the root connection when the default chat provider
    /// changes: `keep` or `clear` (refused without one while it holds
    /// anything).
    #[serde(default)]
    root_credential: RootCredential,
}

/// `PUT /api/defaults`: set the default model of an operation.
pub async fn put_defaults(
    State(state): State<AppState>,
    Query(query): Query<WriteQuery>,
    headers: HeaderMap,
    Json(body): Json<DefaultBody>,
) -> Response {
    let write = write_of!(query, headers);
    manage::apply_change(
        &state,
        ManagedChange::SetDefault {
            operation: body.operation.into(),
            provider: body.provider,
            model: body.model,
            root_credential: body.root_credential,
        },
        write,
    )
    .await
}

/// The `operation` query of the route endpoints.
#[derive(Deserialize)]
pub struct OperationQuery {
    #[serde(default)]
    operation: OperationParam,
    #[serde(default)]
    confirm_high_risk: bool,
}

impl OperationQuery {
    const fn write_query(&self) -> WriteQuery {
        WriteQuery::confirming(self.confirm_high_risk)
    }
}

/// `GET /api/routes?operation=chat|embedding`: the routes of one table, by
/// hint. A route's own key is never shown, only whether it has one.
pub async fn get_routes(State(state): State<AppState>, Query(query): Query<OperationQuery>) -> Response {
    let config = manage::service(&state).config();
    let routes: Vec<_> = match SelectionOperation::from(query.operation) {
        SelectionOperation::Chat => config
            .model_routes
            .iter()
            .map(|route| (&route.hint, &route.provider, &route.model, route.api_key.as_deref()))
            .collect(),
        SelectionOperation::Embedding => config
            .embedding_routes
            .iter()
            .map(|route| (&route.hint, &route.provider, &route.model, route.api_key.as_deref()))
            .collect(),
    };
    let routes = routes
        .into_iter()
        .map(|(hint, provider, model, key)| {
            json!({
                "hint": hint,
                "provider": redact_provider_text(provider),
                "model": model,
                "has_own_key": key.is_some_and(|key| !key.trim().is_empty()),
            })
        })
        .collect::<Vec<_>>();
    Json(json!({"routes": routes})).into_response()
}

/// The body of `POST /api/routes`.
#[derive(Deserialize)]
pub struct AddRouteBody {
    #[serde(default)]
    operation: OperationParam,
    hint: String,
    provider: String,
    model: String,
}

/// `POST /api/routes`: add a route.
pub async fn post_route(
    State(state): State<AppState>,
    Query(query): Query<WriteQuery>,
    headers: HeaderMap,
    Json(body): Json<AddRouteBody>,
) -> Response {
    let write = write_of!(query, headers);
    manage::apply_change(
        &state,
        ManagedChange::AddRoute {
            operation: body.operation.into(),
            hint: body.hint,
            provider: body.provider,
            model: body.model,
        },
        write,
    )
    .await
}

/// The body of `PATCH /api/routes/{hint}`.
#[derive(Deserialize)]
pub struct UpdateRouteBody {
    #[serde(default)]
    operation: OperationParam,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

/// `PATCH /api/routes/{hint}`: change a route's provider or model.
pub async fn patch_route(
    State(state): State<AppState>,
    Path(hint): Path<String>,
    Query(query): Query<WriteQuery>,
    headers: HeaderMap,
    Json(body): Json<UpdateRouteBody>,
) -> Response {
    let write = write_of!(query, headers);
    manage::apply_change(
        &state,
        ManagedChange::UpdateRoute {
            operation: body.operation.into(),
            hint,
            provider: body.provider,
            model: body.model,
        },
        write,
    )
    .await
}

/// `DELETE /api/routes/{hint}?operation=chat|embedding`: remove a route; a
/// route a selection still names is refused with every dependent listed.
pub async fn delete_route(
    State(state): State<AppState>,
    Path(hint): Path<String>,
    Query(query): Query<OperationQuery>,
    headers: HeaderMap,
) -> Response {
    let write = write_of!(query.write_query(), headers);
    manage::apply_change(
        &state,
        ManagedChange::RemoveRoute {
            operation: query.operation.into(),
            hint,
        },
        write,
    )
    .await
}

/// `GET /api/agents`: every delegate agent's model binding.
pub async fn get_agents(State(state): State<AppState>) -> Response {
    let config = manage::service(&state).config();
    let mut agents = config.agents.iter().collect::<Vec<_>>();
    agents.sort_by(|left, right| left.0.cmp(right.0));
    let agents = agents
        .into_iter()
        .map(|(name, agent)| {
            json!({
                "name": name,
                "provider": redact_provider_text(&agent.provider),
                "model": agent.model,
            })
        })
        .collect::<Vec<_>>();
    Json(json!({"agents": agents})).into_response()
}

/// The body of `PUT /api/agents/{name}`.
#[derive(Deserialize)]
pub struct BindAgentBody {
    provider: String,
    model: String,
}

/// `PUT /api/agents/{name}`: bind an existing agent to a provider and
/// model (its other settings are not touched).
pub async fn put_agent(
    State(state): State<AppState>,
    Path(agent): Path<String>,
    Query(query): Query<WriteQuery>,
    headers: HeaderMap,
    Json(body): Json<BindAgentBody>,
) -> Response {
    let write = write_of!(query, headers);
    manage::apply_change(
        &state,
        ManagedChange::BindAgent {
            agent,
            provider: body.provider,
            model: body.model,
        },
        write,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{at, json, serve_config};
    use reqwest::Method;
    use serde_json::json;

    /// F-2: a model id with `/` round-trips through the body (declaration,
    /// route, default, agent binding), through the query of `GET
    /// /api/models`, and back out of every read.
    #[tokio::test]
    async fn a_model_id_with_a_slash_round_trips_through_query_and_body() {
        let served = serve_config(|config| {
            config.reliability.fallback_providers = vec!["openrouter".into()];
            config.agents.insert(
                "coder".into(),
                serde_json::from_value(json!({"provider": "openai", "model": "gpt-4o"})).expect("test: agent"),
            );
        })
        .await;
        const MODEL: &str = "openai/gpt-4o-mini";

        let (status, reply) = served
            .call(
                Method::POST,
                "/api/models",
                Some(json!({"provider": "openrouter", "model": MODEL, "operations": ["chat"]})),
            )
            .await;
        assert_eq!(status, 200, "{reply}");
        assert!(
            at(&json(&reply), "/plan")
                .as_str()
                .is_some_and(|plan| plan.contains(&format!("\"{MODEL}\""))),
            "{reply}"
        );
        let (status, reply) = served
            .call(
                Method::PATCH,
                "/api/models",
                Some(json!({"provider": "openrouter", "model": MODEL, "native_tools": true})),
            )
            .await;
        assert_eq!(status, 200, "{reply}");
        let query = format!("/api/models?provider=openrouter&model={}", urlencoding::encode(MODEL));
        let (status, reply) = served.call(Method::GET, &query, None).await;
        assert_eq!(status, 200, "{reply}");
        let listed = json(&reply);
        assert_eq!(at(&listed, "/models/0/model"), MODEL, "{reply}");
        assert_eq!(at(&listed, "/models/0/source"), "operator declaration", "{reply}");
        assert_eq!(at(&listed, "/models/0/operations/chat"), "supported", "{reply}");
        assert!(at(&listed, "/models/1").is_null(), "{reply}");
        let (status, reply) = served
            .call(
                Method::GET,
                "/api/models?provider=openrouter&operation=chat&executable=true",
                None,
            )
            .await;
        assert_eq!(status, 200, "{reply}");
        assert!(
            at(&json(&reply), "/models")
                .as_array()
                .is_some_and(|models| models.iter().any(|entry| at(entry, "/model") == MODEL)),
            "{reply}"
        );
        let declared = served.loaded().providers.models;
        assert_eq!(declared.len(), 1);
        let declared = declared.first().expect("test: the declaration");
        assert_eq!(declared.model.as_deref(), Some(MODEL));
        assert_eq!(declared.native_tools, Some(true));

        let (status, reply) = served
            .call(
                Method::POST,
                "/api/routes",
                Some(json!({"hint": "fast", "provider": "openrouter", "model": MODEL})),
            )
            .await;
        assert_eq!(status, 200, "{reply}");
        let (status, reply) = served
            .call(
                Method::PUT,
                "/api/agents/coder",
                Some(json!({"provider": "openrouter", "model": MODEL})),
            )
            .await;
        assert_eq!(status, 200, "{reply}");
        let (status, reply) = served
            .call(
                Method::PUT,
                "/api/defaults",
                Some(json!({"provider": "openrouter", "model": MODEL})),
            )
            .await;
        assert_eq!(status, 200, "{reply}");

        let (_, reply) = served.call(Method::GET, "/api/defaults", None).await;
        assert_eq!(at(&json(&reply), "/chat/model"), MODEL, "{reply}");
        let (_, reply) = served.call(Method::GET, "/api/routes?operation=chat", None).await;
        assert_eq!(at(&json(&reply), "/routes/0/model"), MODEL, "{reply}");
        let (_, reply) = served.call(Method::GET, "/api/agents", None).await;
        assert_eq!(at(&json(&reply), "/agents/0/model"), MODEL, "{reply}");

        // The route and the declaration change by their keys (hint in the
        // path, provider and model in the body).
        let (status, reply) = served
            .call(
                Method::PATCH,
                "/api/routes/fast",
                Some(json!({"model": "openai/gpt-4o"})),
            )
            .await;
        assert_eq!(status, 200, "{reply}");
        let (status, reply) = served
            .call(
                Method::DELETE,
                "/api/models",
                Some(json!({"provider": "openrouter", "model": MODEL})),
            )
            .await;
        assert_eq!(status, 200, "{reply}");
        assert!(served.loaded().providers.models.is_empty(), "expected an empty value");
        assert_eq!(served.loaded().default_model.as_deref(), Some(MODEL));
    }

    /// D-078 carried: the dry run calls the validation the write runs, so a
    /// selection the evidence refuses is refused by the plan with the same
    /// text, and nothing is written by either.
    #[tokio::test]
    async fn a_dry_run_refuses_what_the_write_refuses_with_the_same_text() {
        let served = serve_config(|config| {
            config.providers.models = vec![crate::config::schema::ProviderModelConfig {
                provider: "openai".into(),
                model: Some("text-embedding-3-small".into()),
                operations: Some(vec![crate::config::schema::ModelOperationSetting::Embedding]),
                native_tools: None,
                vision_input: None,
                streaming: None,
                structured_output: None,
                tool_schema_dialect: None,
                tool_call_dialect: None,
                context_window: None,
            }];
        })
        .await;
        let before = served.disk();
        let route =
            json!({"kind": "add_route", "hint": "fast", "provider": "openai", "model": "text-embedding-3-small"});

        let (status, planned) = served
            .call(Method::POST, "/api/config/plans", Some(json!({"change": route})))
            .await;
        assert_eq!(status, 400, "{planned}");
        let (status, applied) = served
            .call(Method::POST, "/api/config/apply", Some(json!({"change": route})))
            .await;
        assert_eq!(status, 400, "{applied}");
        let (status, object) = served
            .call(
                Method::POST,
                "/api/routes",
                Some(json!({"hint": "fast", "provider": "openai", "model": "text-embedding-3-small"})),
            )
            .await;
        assert_eq!(status, 400, "{object}");
        let error = at(&json(&planned), "/error");
        assert!(
            error
                .as_str()
                .is_some_and(|error| error.contains("model route `hint:fast` refused")),
            "{planned}"
        );
        assert_eq!(at(&json(&applied), "/error"), error);
        assert_eq!(at(&json(&object), "/error"), error);
        assert_eq!(served.disk(), before);
    }

    /// PMME-f F-3 through the Gateway: switching the default provider while
    /// the root `api_key` is set needs the explicit choice (400 without it)
    /// and is then a high-risk `credential_owner_changed` plan (428) applied
    /// only against the reported revision.
    #[tokio::test]
    async fn switching_the_default_away_from_the_root_key_needs_the_choice_and_confirmation() {
        const KEY: &str = "pmmf-root-key-sentinel";
        let served = serve_config(|config| config.api_key = Some(KEY.into())).await;
        let before = served.disk();
        let switch = json!({"provider": "anthropic", "model": "claude-sonnet-4-5"});

        let (status, reply) = served.call(Method::PUT, "/api/defaults", Some(switch.clone())).await;
        assert_eq!(status, 400, "{reply}");
        assert!(
            reply.contains("belongs to the current default provider 'openai'"),
            "{reply}"
        );

        // `clear` removes the key with the switch: a credential change, 428.
        let mut clear = switch.clone();
        if let Some(object) = clear.as_object_mut() {
            object.insert("root_credential".into(), json!("clear"));
        }
        let (status, reply) = served.call(Method::PUT, "/api/defaults", Some(clear)).await;
        assert_eq!(status, 428, "{reply}");
        assert!(reply.contains("writes the credential at api_key"), "{reply}");
        assert_eq!(served.disk(), before);

        let mut keep = switch;
        if let Some(object) = keep.as_object_mut() {
            object.insert("root_credential".into(), json!("keep"));
        }
        let (status, reply) = served.call(Method::PUT, "/api/defaults", Some(keep.clone())).await;
        assert_eq!(status, 428, "{reply}");
        let refused = json(&reply);
        assert!(
            at(&refused, "/risk_reasons").as_array().is_some_and(|reasons| reasons
                .iter()
                .any(|reason| at(reason, "/kind") == "credential_owner_changed")),
            "{reply}"
        );
        assert!(!reply.contains(KEY), "{reply}");
        assert_eq!(served.disk(), before);

        let revision = at(&refused, "/revision").as_str().expect("test: revision").to_string();
        let (status, reply) = served
            .call_if_match(
                Method::PUT,
                "/api/defaults?confirm_high_risk=true",
                &revision,
                Some(keep),
            )
            .await;
        assert_eq!(status, 200, "{reply}");
        let loaded = served.loaded();
        assert_eq!(loaded.default_provider.as_deref(), Some("anthropic"));
        assert_eq!(loaded.api_key.as_deref(), Some(KEY));
    }

    /// D-089 1: a provider URL's query values and fragment never come back
    /// from a management endpoint, whether it is the default, a fallback,
    /// a route, an agent or the embedder; nor in a refusal that names the
    /// provider, nor in the connection test.
    #[tokio::test]
    async fn provider_query_values_and_fragments_are_never_echoed() {
        const QUERY: &str = "pmmfqsentinel";
        const FRAGMENT: &str = "pmmffragsentinel";
        let url = format!("custom:http://127.0.0.1:2/v1?sig={QUERY}&flag#{FRAGMENT}");
        let served = {
            let url = url.clone();
            serve_config(move |config| {
                config.default_provider = Some(url.clone());
                config.default_model = Some("local-model".into());
                config.reliability.fallback_providers = vec![url.clone()];
                config.model_routes = vec![crate::config::ModelRouteConfig {
                    hint: "fast".into(),
                    provider: url.clone(),
                    model: "local-model".into(),
                    api_key: None,
                    credential_ref: None,
                }];
                config.memory.embedding_provider = url.clone();
                config.agents.insert(
                    "coder".into(),
                    serde_json::from_value(json!({"provider": url, "model": "local-model"})).expect("test: agent"),
                );
                config.memory.embedding_model = "local-embedder".into();
            })
            .await
        };
        let references = crate::config::manage::credentials::CredentialReferences::for_config(&served.loaded())
            .expect("test: references");
        references
            .set(&url, "shared", "pmmf-owner-value-1234")
            .expect("test: a reference owned by the URL provider");

        let mut replies = Vec::new();
        for path in [
            "/api/defaults",
            "/api/routes?operation=chat",
            "/api/agents",
            "/api/providers",
            "/api/models",
        ] {
            let (status, reply) = served.call(Method::GET, path, None).await;
            assert_eq!(status, 200, "{path}: {reply}");
            replies.push((path.to_string(), reply));
        }
        // The owner text of a 409.
        let (status, reply) = served
            .call(Method::DELETE, "/api/providers/openai/credential?name=shared", None)
            .await;
        assert_eq!(status, 409, "{reply}");
        replies.push(("409".to_string(), reply));
        // The connection test: refused at the URL layer or answered, never
        // echoing either value.
        let encoded = urlencoding::encode(&url).into_owned();
        let (_, reply) = served
            .call(Method::POST, &format!("/api/providers/{encoded}/test"), Some(json!({})))
            .await;
        replies.push(("test".to_string(), reply));
        for (surface, reply) in replies {
            assert!(!reply.contains(QUERY), "{surface} echoed the query value: {reply}");
            assert!(!reply.contains(FRAGMENT), "{surface} echoed the fragment: {reply}");
        }
    }
}
