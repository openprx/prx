#![cfg(test)]
//! Targeted agent selection: selector, name grammar, rollout switch, autonomy
//! cap and allowlist semantics.
use super::*;
use crate::config::DelegateAgentConfig;
use crate::config::agent_name::{AGENT_NAME_MAX_LEN, AgentName, AgentNameError, is_targetable};
use crate::security::AutonomyLevel;
use std::collections::BTreeSet;

use super::super::target::{AgentField, TargetError, ToolAllowlist, TurnTarget, effective_autonomy, resolve_target};

fn agent(agentic: bool, allowed_tools: &[&str]) -> DelegateAgentConfig {
    DelegateAgentConfig {
        provider: "ollama".to_owned(),
        model: "test-model".to_owned(),
        system_prompt: None,
        api_key: None,
        credential_ref: None,
        temperature: None,
        max_depth: 8,
        agentic,
        allowed_tools: allowed_tools.iter().map(|name| (*name).to_owned()).collect(),
        identity_dir: None,
        memory_scope: None,
        spawn_enabled: None,
        enabled: true,
        autonomy: None,
    }
}

fn config_with(agents: Vec<(&str, DelegateAgentConfig)>, switch: bool) -> Config {
    let mut config = Config::default();
    config.gateway.targeted_agents = switch;
    config.agents = agents.into_iter().map(|(name, cfg)| (name.to_owned(), cfg)).collect();
    config
}

fn named(raw: &str) -> TurnTarget {
    TurnTarget::from_field(AgentField::Text(raw.to_owned())).expect("test: a valid name")
}

fn http_request(body: serde_json::Value) -> Request {
    let mut request = Request::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("test: request");
    request
        .extensions_mut()
        .insert(super::super::super::ApiPrincipal::of_token("prx-test-token"));
    request
}

struct Fixture {
    state: AppState,
    provider: Arc<CapturingProvider>,
    memory: Arc<crate::memory::SqliteMemory>,
    _temp: tempfile::TempDir,
}

const SESSION: &str = "prx-target-session";

async fn fixture(config: impl FnOnce(&mut Config)) -> Fixture {
    let temp = tempfile::tempdir().expect("test: tempdir");
    let mut base = Config::default();
    base.workspace_dir = temp.path().to_owned();
    config(&mut base);
    let provider = Arc::new(CapturingProvider::default());
    let mut state = test_app_state(base, provider.clone());
    let memory = Arc::new(crate::memory::SqliteMemory::new(temp.path()).expect("test: sqlite memory"));
    state.mem = memory.clone();
    memory
        .create_conversation_session(SESSION, "api:local")
        .await
        .expect("test: session");
    Fixture {
        state,
        provider,
        memory,
        _temp: temp,
    }
}

async fn post(fixture: &Fixture, session: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    let response = post_session_message(
        State(fixture.state.clone()),
        Path(session.to_owned()),
        http_request(body),
    )
    .await
    .into_response();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("test: body");
    (status, serde_json::from_slice(&bytes).expect("test: json body"))
}

fn error_code(body: &serde_json::Value) -> Option<&str> {
    body.pointer("/error/code").and_then(serde_json::Value::as_str)
}

async fn assert_untouched(fixture: &Fixture) {
    assert!(fixture.provider.requests.lock().is_empty(), "no provider access");
    let stored = fixture
        .memory
        .get_conversation_session(SESSION)
        .await
        .expect("test: read session")
        .expect("test: session exists");
    assert_eq!(stored.message_count, 0, "no input was recorded");
}

#[tokio::test]
async fn target_selector_preserves_main() {
    // Omission and "main" reach the Main path (here: the session lookup, which
    // answers a plain string error for a session that does not exist), with the
    // switch off and on, and whatever `[agents.main]` holds.
    for switch in [false, true] {
        let fixture = fixture(|config| {
            config.gateway.targeted_agents = switch;
            config.agents.insert("main".to_owned(), agent(false, &[]));
        })
        .await;
        for body in [
            serde_json::json!({"message": "hello"}),
            serde_json::json!({"message": "hello", "agent": "main"}),
        ] {
            let (status, value) = post(&fixture, "no-such-session", body).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            assert_eq!(
                value.get("error").and_then(serde_json::Value::as_str),
                Some("Session not found"),
                "the Main path was entered: {value}"
            );
        }
        // Explicit null is the one refused spelling.
        let (status, value) = post(
            &fixture,
            SESSION,
            serde_json::json!({"message": "hello", "agent": null}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error_code(&value), Some("agent_invalid"));
        let (status, value) = post(&fixture, SESSION, serde_json::json!({"message": "hello", "agent": 7})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error_code(&value), Some("agent_invalid"));
        assert_untouched(&fixture).await;
    }
    assert_eq!(TurnTarget::from_field(AgentField::Omitted), Ok(TurnTarget::Main));
    assert_eq!(
        TurnTarget::from_field(AgentField::Text("main".to_owned())),
        Ok(TurnTarget::Main)
    );
    assert_eq!(TurnTarget::from_field(AgentField::Null), Err(TargetError::Invalid));
}

#[tokio::test]
async fn target_unknown_or_disabled_has_no_effects() {
    let mut off = agent(false, &[]);
    off.enabled = false;
    // Switch off: every otherwise-valid named target is 409, existing or not,
    // and never runs as Main.
    let closed = fixture(|config| {
        config.agents.insert("helper".to_owned(), agent(false, &[]));
    })
    .await;
    for name in ["helper", "ghost"] {
        let (status, value) = post(&closed, SESSION, serde_json::json!({"message": "hi", "agent": name})).await;
        assert_eq!(status, StatusCode::CONFLICT, "{name}: {value}");
        assert_eq!(error_code(&value), Some("agent_disabled"));
    }
    assert_untouched(&closed).await;

    // Switch on: unknown is 404, disabled is 409, both resolved before the
    // session lookup, so even a missing session reports the target error.
    let open = fixture(|config| {
        config.gateway.targeted_agents = true;
        config.agents.insert("helper".to_owned(), agent(false, &[]));
        config.agents.insert("sleeper".to_owned(), off);
    })
    .await;
    for (name, session, status, code) in [
        ("ghost", SESSION, StatusCode::NOT_FOUND, "agent_unknown"),
        ("sleeper", SESSION, StatusCode::CONFLICT, "agent_disabled"),
        ("ghost", "no-such-session", StatusCode::NOT_FOUND, "agent_unknown"),
    ] {
        let (got, value) = post(&open, session, serde_json::json!({"message": "hi", "agent": name})).await;
        assert_eq!(got, status, "{name}: {value}");
        assert_eq!(error_code(&value), Some(code));
    }
    assert_untouched(&open).await;
}

#[test]
fn target_autonomy_is_capped() {
    use AutonomyLevel::{Full, ReadOnly, Supervised};
    for (agent_level, main, expected) in [
        (None, Supervised, Supervised),
        (Some(Full), Supervised, Supervised),
        (Some(ReadOnly), Full, ReadOnly),
        (Some(Supervised), Full, Supervised),
        (Some(Full), ReadOnly, ReadOnly),
        (Some(Full), Full, Full),
    ] {
        assert_eq!(effective_autonomy(agent_level, main), expected);
    }

    let mut capped = agent(false, &[]);
    capped.autonomy = Some(Full);
    let mut config = config_with(vec![("worker", capped)], true);
    config.autonomy.level = Supervised;
    let plan = resolve_target(&config, &named("worker"))
        .expect("test: resolves")
        .expect("test: a named plan");
    assert_eq!(plan.autonomy, Supervised, "a sub-agent never exceeds Main");
    // The policy the target's turn is published with carries that level.
    let mut snapshot = config.clone();
    snapshot.autonomy.level = plan.autonomy;
    assert_eq!(
        crate::runtime::bootstrap::build_security_policy(&snapshot).autonomy,
        Supervised
    );

    config.agents.get_mut("worker").expect("test: worker").autonomy = Some(ReadOnly);
    config.autonomy.level = Full;
    let plan = resolve_target(&config, &named("worker"))
        .expect("test: resolves")
        .expect("test: a named plan");
    assert_eq!(plan.autonomy, ReadOnly, "a lower sub-agent level is kept");
}

#[test]
fn agent_name_rejects_invalid_start() {
    for raw in [
        "",
        "1agent",
        "9",
        "Agent",
        "A",
        "-agent",
        "_agent",
        " agent",
        "\u{e9}agent",
    ] {
        assert_eq!(AgentName::parse(raw), Err(AgentNameError::InvalidStart), "{raw:?}");
        assert_eq!(
            TurnTarget::from_field(AgentField::Text(raw.to_owned())),
            Err(TargetError::Invalid),
            "{raw:?}"
        );
    }
}

#[tokio::test]
async fn agent_name_rejects_invalid_characters() {
    for raw in [
        "a b", "a/b", "a.b", "a\\b", "a\u{e9}", "a\u{3b1}", "aB", "a\u{0}", "a\n", "a%2Fb",
    ] {
        assert_eq!(AgentName::parse(raw), Err(AgentNameError::InvalidCharacter), "{raw:?}");
    }
    let fixture = fixture(|config| config.gateway.targeted_agents = true).await;
    let (status, value) = post(&fixture, SESSION, serde_json::json!({"message": "hi", "agent": "a b"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_code(&value), Some("agent_invalid"));
    assert_untouched(&fixture).await;
}

#[tokio::test]
async fn agent_name_rejects_overlength() {
    assert!(AgentName::parse(&"a".repeat(AGENT_NAME_MAX_LEN)).is_ok());
    let long = "a".repeat(AGENT_NAME_MAX_LEN + 1);
    assert_eq!(AgentName::parse(&long), Err(AgentNameError::TooLong));
    let fixture = fixture(|config| config.gateway.targeted_agents = true).await;
    let (status, value) = post(&fixture, SESSION, serde_json::json!({"message": "hi", "agent": long})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_code(&value), Some("agent_invalid"));
    assert_untouched(&fixture).await;
}

#[tokio::test]
async fn agent_name_is_exact_at_all_ingresses() {
    // Config load: non-conforming and reserved legacy names load (with a
    // warning) for delegate/spawn, but are never targetable.
    let config = config_with(
        vec![
            ("main", agent(false, &[])),
            ("Researcher", agent(false, &[])),
            ("white space", agent(false, &[])),
            ("ok-agent", agent(false, &[])),
        ],
        true,
    );
    config.validate().expect("test: legacy agent names still load");
    for key in config.agents.keys() {
        assert_eq!(is_targetable(key), key == "ok-agent", "{key}");
    }
    assert!(
        resolve_target(&config, &named("ok-agent"))
            .expect("test: resolves")
            .is_some()
    );

    // Request field / decoded path segment: exact match, no folding or trimming.
    for raw in [
        "Researcher",
        "OK-AGENT",
        "Ok-Agent",
        " ok-agent",
        "ok-agent ",
        "ok-agent\n",
        "white space",
    ] {
        assert_eq!(
            TurnTarget::from_field(AgentField::Text(raw.to_owned())),
            Err(TargetError::Invalid),
            "{raw:?}"
        );
    }
    // `main` is the one explicit Main spelling; it never resolves to the legacy entry.
    assert_eq!(
        TurnTarget::from_field(AgentField::Text("main".to_owned())),
        Ok(TurnTarget::Main)
    );
    assert!(
        resolve_target(&config, &TurnTarget::Main)
            .expect("test: Main resolves")
            .is_none()
    );

    // Over HTTP the same strings are refused before any effect.
    let fixture = fixture(|cfg| {
        cfg.gateway.targeted_agents = true;
        cfg.agents.insert("ok-agent".to_owned(), agent(false, &[]));
    })
    .await;
    for raw in ["OK-AGENT", "Researcher", " ok-agent"] {
        let (status, value) = post(&fixture, SESSION, serde_json::json!({"message": "hi", "agent": raw})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{raw:?}");
        assert_eq!(error_code(&value), Some("agent_invalid"));
    }
    assert_untouched(&fixture).await;
}

#[tokio::test]
async fn target_allowlist_semantics() {
    let resolve = |cfg: DelegateAgentConfig| {
        resolve_target(&config_with(vec![("worker", cfg)], true), &named("worker"))
            .map(|plan| plan.map(|plan| plan.tools))
    };
    // Non-agentic: no general execution tools, whatever the list holds.
    assert_eq!(resolve(agent(false, &["file_read"])), Ok(Some(ToolAllowlist::None)));
    // Wildcard inherits only the admitted host surface.
    assert_eq!(resolve(agent(true, &["*"])), Ok(Some(ToolAllowlist::Inherit)));
    assert_eq!(resolve(agent(true, &[" * "])), Ok(Some(ToolAllowlist::Inherit)));
    // Mixed wildcard and empty lists are refused.
    for list in [&["*", "file_read"][..], &[][..], &[" ", ""][..]] {
        assert_eq!(resolve(agent(true, list)), Err(TargetError::ToolsInvalid), "{list:?}");
    }
    // Explicit names intersect the host surface; cron and xin never reach a target.
    let explicit = resolve(agent(true, &["file_read", "cron"]))
        .expect("test: resolves")
        .expect("test: plan");
    assert_eq!(
        explicit,
        ToolAllowlist::Only(BTreeSet::from(["file_read".to_owned(), "cron".to_owned()]))
    );
    let surface = explicit.surface();
    assert_eq!(surface.allowed_tools, Some(BTreeSet::from(["file_read".to_owned()])));
    let excluded = BTreeSet::from(["cron".to_owned(), "xin".to_owned()]);
    assert_eq!(surface.excluded_names, excluded);
    let inherit = ToolAllowlist::Inherit.surface();
    assert_eq!(inherit.allowed_tools, None);
    assert_eq!(
        inherit.excluded_names, excluded,
        "the wildcard does not widen past the exclusion"
    );
    assert_eq!(ToolAllowlist::None.surface().allowed_tools, Some(BTreeSet::new()));

    // Over HTTP an invalid agentic list is a typed 400 with no effects.
    let fixture = fixture(|config| {
        config.gateway.targeted_agents = true;
        config
            .agents
            .insert("worker".to_owned(), agent(true, &["*", "file_read"]));
    })
    .await;
    let (status, value) = post(
        &fixture,
        SESSION,
        serde_json::json!({"message": "hi", "agent": "worker"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_code(&value), Some("agent_tools_invalid"));
    assert_untouched(&fixture).await;
}
