use super::*;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::json;
use std::net::SocketAddr;
use tempfile::TempDir;
use tower::ServiceExt;

fn setup_state(tmp: &TempDir, token: &str) -> WebhookState {
    let db_path = tmp.path().join("memory").join("brain.db");
    let repository: Arc<dyn WebhookRepository> =
        Arc::new(SqliteWebhookRepository::new(db_path, tmp.path().to_string_lossy().to_string()).unwrap());
    WebhookState {
        token: Arc::<str>::from(token.to_string()),
        signing_secret: None,
        repository,
        rate_limiter: Arc::new(WebhookRateLimiter::new(
            WEBHOOK_DEFAULT_RATE_LIMIT_PER_MINUTE,
            Duration::from_secs(WEBHOOK_RATE_LIMIT_WINDOW_SECS),
        )),
        security: Arc::new(SecurityPolicy::default()),
    }
}

fn setup_state_with_limits(
    tmp: &TempDir,
    token: &str,
    rate_limit_per_minute: u32,
    _idempotency_max_keys: usize,
) -> WebhookState {
    let db_path = tmp.path().join("memory").join("brain.db");
    let repository: Arc<dyn WebhookRepository> =
        Arc::new(SqliteWebhookRepository::new(db_path, tmp.path().to_string_lossy().to_string()).unwrap());
    WebhookState {
        token: Arc::<str>::from(token.to_string()),
        signing_secret: None,
        repository,
        rate_limiter: Arc::new(WebhookRateLimiter::new(
            rate_limit_per_minute,
            Duration::from_secs(WEBHOOK_RATE_LIMIT_WINDOW_SECS),
        )),
        security: Arc::new(SecurityPolicy::default()),
    }
}

/// Build a webhook state whose [`SecurityPolicy`] runs under `ReadOnly`
/// autonomy, so the persist [`SideEffectGate`] must reject writes (FIX-P1-03).
fn setup_readonly_state(tmp: &TempDir, token: &str) -> WebhookState {
    let mut state = setup_state(tmp, token);
    let readonly = SecurityPolicy {
        autonomy: crate::security::policy::AutonomyLevel::ReadOnly,
        ..SecurityPolicy::default()
    };
    state.security = Arc::new(readonly);
    state
}

#[tokio::test]
async fn token_auth_missing_rejected() {
    let tmp = TempDir::new().unwrap();
    let app = router(setup_state(&tmp, "secret"));

    let req = Request::builder()
        .method("POST")
        .uri("/webhook")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "source": "custom",
                "event_type": "issue.created",
                "external_id": "issue#1",
                "title": "test",
                "content": "test",
                "timestamp": Utc::now().to_rfc3339()
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn token_auth_invalid_rejected() {
    let tmp = TempDir::new().unwrap();
    let app = router(setup_state(&tmp, "secret"));

    let req = Request::builder()
        .method("POST")
        .uri("/webhook/events")
        .header("content-type", "application/json")
        .header("authorization", "Bearer wrong")
        .body(Body::from(
            json!({
                "source": "custom",
                "event_type": "issue.created",
                "external_id": "issue#1",
                "title": "test",
                "content": "test",
                "timestamp": Utc::now().to_rfc3339()
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn token_auth_valid_accepts_and_persists_topic() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("memory").join("brain.db");
    let app = router(setup_state(&tmp, "secret"));

    let req = Request::builder()
        .method("POST")
        .uri("/webhook/events")
        .header("content-type", "application/json")
        .header("authorization", "Bearer secret")
        .body(Body::from(
            json!({
                "source": "custom",
                "event_type": "issue.created",
                "project": "openpr",
                "external_id": "issue#42",
                "external_url": "https://example.com/issues/42",
                "title": "Issue 42",
                "content": "details",
                "actor": "openprx_user",
                "timestamp": Utc::now().to_rfc3339()
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let topic_id = parsed.get("topic_id").and_then(serde_json::Value::as_str).unwrap();
    assert!(!topic_id.is_empty(), "expected a non-empty value");

    let conn = Connection::open(db_path).unwrap();
    let topic_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM topics WHERE external_id = 'issue#42'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(topic_count, 1);

    let visibility: String = conn
        .query_row(
            "SELECT visibility FROM memories WHERE topic_id = ?1 ORDER BY created_at DESC LIMIT 1",
            params![topic_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(visibility, "owner");
}

#[tokio::test]
async fn same_external_id_in_different_projects_keeps_separate_topics() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("memory").join("brain.db");
    let app = router(setup_state(&tmp, "secret"));

    for project in ["openpr", "lc"] {
        let req = Request::builder()
            .method("POST")
            .uri("/webhook/events")
            .header("content-type", "application/json")
            .header("authorization", "Bearer secret")
            .body(Body::from(
                json!({
                    "source": "custom",
                    "event_type": "issue.created",
                    "project": project,
                    "external_id": "issue#42",
                    "title": format!("{project} issue"),
                    "content": "details",
                    "actor": "openprx_user",
                    "timestamp": Utc::now().to_rfc3339()
                })
                .to_string(),
            ))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    let conn = Connection::open(db_path).unwrap();
    let topic_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM topics WHERE external_id = 'issue#42'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(topic_count, 2);
}

#[test]
fn openpr_payload_maps_to_generic_event() {
    let payload = json!({
        "workspace_id": "openpr",
        "issue_identifier": 88,
        "event_type": "issue.closed",
        "title": "Close issue",
        "content": "merged",
        "actor": "project_bot",
        "timestamp": "2026-02-23T00:00:00Z"
    });

    let mapped = parse_webhook_event(payload).unwrap();
    assert_eq!(mapped.source, "openpr");
    assert_eq!(mapped.external_id, "issue#88");
    assert_eq!(mapped.event_type, "issue.closed");
    assert_eq!(mapped.project.as_deref(), Some("openpr"));
}

#[tokio::test]
async fn webhook_server_starts_with_listener() {
    let tmp = TempDir::new().unwrap();
    let state = setup_state(&tmp, "secret");
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        let _ = run_with_listener(listener, state).await;
    });

    let client = reqwest::Client::new();
    let response = client
        .post(format!("http://{addr}/webhook"))
        .bearer_auth("secret")
        .json(&json!({
            "source": "custom",
            "event_type": "issue.created",
            "external_id": "issue#100",
            "title": "Boot",
            "content": "ok",
            "timestamp": Utc::now().to_rfc3339()
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    handle.abort();
    let _ = handle.await;
}

#[tokio::test]
async fn webhook_idempotency_key_rejects_duplicate_replay() {
    let tmp = TempDir::new().unwrap();
    let app = router(setup_state_with_limits(&tmp, "secret", 60, 1024));
    let body = json!({
        "source": "custom",
        "event_type": "issue.created",
        "external_id": "issue#200",
        "title": "Idempotent",
        "content": "same",
        "timestamp": Utc::now().to_rfc3339()
    })
    .to_string();

    let req1 = Request::builder()
        .method("POST")
        .uri("/webhook/events")
        .header("content-type", "application/json")
        .header("authorization", "Bearer secret")
        .header("X-Idempotency-Key", "dup-key")
        .body(Body::from(body.clone()))
        .unwrap();
    let resp1 = app.clone().oneshot(req1).await.unwrap();
    assert_eq!(resp1.status(), StatusCode::OK);

    let req2 = Request::builder()
        .method("POST")
        .uri("/webhook/events")
        .header("content-type", "application/json")
        .header("authorization", "Bearer secret")
        .header("X-Idempotency-Key", "dup-key")
        .body(Body::from(body))
        .unwrap();
    let resp2 = app.oneshot(req2).await.unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);
    let body2 = to_bytes(resp2.into_body(), usize::MAX).await.unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&body2).unwrap();
    assert_eq!(parsed["status"], "duplicate");
}

#[tokio::test]
async fn committed_ingestion_replays_after_state_restart_without_duplicate_rows() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("memory").join("brain.db");
    let body = json!({
        "source": "custom",
        "event_type": "issue.created",
        "project": "openpr",
        "external_id": "issue#restart-replay",
        "title": "Restart replay",
        "content": "durable webhook replay remains stable across repository reconstruction",
        "timestamp": "2026-07-15T13:15:00Z"
    })
    .to_string();
    let make_request = || {
        Request::builder()
            .method("POST")
            .uri("/webhook/events")
            .header("content-type", "application/json")
            .header("authorization", "Bearer secret")
            .header("X-Idempotency-Key", "restart-replay-key")
            .body(Body::from(body.clone()))
            .unwrap()
    };

    let first = router(setup_state(&tmp, "secret"))
        .oneshot(make_request())
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);

    let replay = router(setup_state(&tmp, "secret"))
        .oneshot(make_request())
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::OK);
    let replay_body = to_bytes(replay.into_body(), usize::MAX).await.unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&replay_body).unwrap();
    assert_eq!(parsed["status"], "duplicate");
    assert!(parsed["topic_id"].as_str().is_some());

    let conn = Connection::open(db_path).unwrap();
    let ingestion_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM webhook_ingestions WHERE external_id = 'issue#restart-replay'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let memory_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE key LIKE 'webhook:custom:issue#restart-replay:%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let outbox_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_events
                  WHERE event_type = 'webhook.event.committed'
                    AND subject_table = 'webhook_ingestions'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let stored_key: String = conn
        .query_row(
            "SELECT ingestion_key FROM webhook_ingestions WHERE external_id = 'issue#restart-replay'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(ingestion_count, 1);
    assert_eq!(memory_count, 1);
    assert_eq!(outbox_count, 1);
    assert!(!stored_key.contains("restart-replay-key"));
}

#[tokio::test]
async fn same_idempotency_key_with_different_body_conflicts() {
    let tmp = TempDir::new().unwrap();
    let app = router(setup_state(&tmp, "secret"));
    let make_request = |title: &str| {
        Request::builder()
            .method("POST")
            .uri("/webhook/events")
            .header("content-type", "application/json")
            .header("authorization", "Bearer secret")
            .header("X-Idempotency-Key", "body-bound-key")
            .body(Body::from(
                json!({
                    "source": "custom",
                    "event_type": "issue.created",
                    "external_id": "issue#body-bound",
                    "title": title,
                    "content": "same external identity with a different request body",
                    "timestamp": "2026-07-15T13:20:00Z"
                })
                .to_string(),
            ))
            .unwrap()
    };

    let first = app.clone().oneshot(make_request("first body")).await.unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let conflict = app.oneshot(make_request("second body")).await.unwrap();
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    let conflict_body = to_bytes(conflict.into_body(), usize::MAX).await.unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&conflict_body).unwrap();
    assert_eq!(parsed["status"], "request_conflict");
}

#[tokio::test]
async fn pending_ingestion_reports_processing_and_expired_lease_is_reclaimed() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("memory").join("brain.db");
    let repository = SqliteWebhookRepository::new(db_path.clone(), tmp.path().to_string_lossy().to_string()).unwrap();
    let event = WebhookEvent {
        source: "custom".to_string(),
        event_type: "issue.created".to_string(),
        project: Some("openpr".to_string()),
        external_id: "issue#pending".to_string(),
        external_url: None,
        title: "Pending lease".to_string(),
        content: "pending request recovery".to_string(),
        actor: None,
        timestamp: "2026-07-15T13:25:00Z".to_string(),
    };
    let request_hash = webhook_request_hash(b"pending-body");
    let event_identity = webhook_event_identity(&event, &request_hash);
    let first = repository
        .claim(
            "event:pending".to_string(),
            event_identity.clone(),
            request_hash.clone(),
            &event,
        )
        .await
        .unwrap();
    assert!(matches!(first, WebhookClaimOutcome::Acquired(_)));

    let processing = repository
        .claim(
            "event:pending".to_string(),
            event_identity.clone(),
            request_hash.clone(),
            &event,
        )
        .await
        .unwrap();
    assert!(matches!(processing, WebhookClaimOutcome::Processing));

    let conn = Connection::open(db_path).unwrap();
    conn.execute(
        "UPDATE webhook_ingestions SET lease_expires_at = 0 WHERE ingestion_key = 'event:pending'",
        [],
    )
    .unwrap();
    let reclaimed = repository
        .claim("event:pending".to_string(), event_identity, request_hash, &event)
        .await
        .unwrap();
    assert!(matches!(
        reclaimed,
        WebhookClaimOutcome::Acquired(WebhookIngestionClaim { generation: 2, .. })
    ));
}

#[test]
fn configured_unsupported_webhook_backend_fails_closed() {
    let tmp = TempDir::new().unwrap();
    let mut config = Config::default();
    config.workspace_dir = tmp.path().to_path_buf();
    config.memory.backend = "markdown".to_string();
    config.webhook.token = Some("receiver-token".to_string());

    let result = repository_from_config(&config);
    assert!(result.is_err(), "markdown webhook repository must fail closed");
    let error = result.err().map_or_else(String::new, |error| error.to_string());
    assert!(error.contains("does not support configured memory backend 'markdown'"));
    let validation_error = config
        .validate()
        .err()
        .map_or_else(String::new, |error| error.to_string());
    assert!(validation_error.contains("requires memory backend 'sqlite', 'lucid', or 'postgres'"));
}

#[tokio::test]
async fn configured_signing_secret_requires_valid_hmac() {
    let tmp = TempDir::new().unwrap();
    let mut state = setup_state(&tmp, "secret");
    state.signing_secret = Some(Arc::from("signing-secret"));
    let app = router(state);
    let body = json!({
        "source": "custom",
        "event_type": "issue.created",
        "external_id": "issue#signed",
        "title": "Signed event",
        "content": "valid HMAC permits durable standalone webhook ingestion",
        "timestamp": "2026-07-15T13:30:00Z"
    })
    .to_string();

    let missing = Request::builder()
        .method("POST")
        .uri("/webhook/events")
        .header("content-type", "application/json")
        .header("authorization", "Bearer secret")
        .body(Body::from(body.clone()))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(missing).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );

    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(b"signing-secret").unwrap();
    mac.update(body.as_bytes());
    let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
    let signed = Request::builder()
        .method("POST")
        .uri("/webhook/events")
        .header("content-type", "application/json")
        .header("authorization", "Bearer secret")
        .header("X-Webhook-Signature", signature)
        .body(Body::from(body))
        .unwrap();
    assert_eq!(app.oneshot(signed).await.unwrap().status(), StatusCode::OK);
}

#[tokio::test]
async fn failed_ingestion_rolls_back_all_state_and_same_key_retries() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("memory").join("brain.db");
    let app = router(setup_state(&tmp, "secret"));
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER fail_webhook_memory_insert
             BEFORE INSERT ON memories
             WHEN NEW.key LIKE 'webhook:%'
             BEGIN
                 SELECT RAISE(ABORT, 'injected webhook memory failure');
             END;",
    )
    .unwrap();

    let body = json!({
        "source": "custom",
        "event_type": "issue.created",
        "project": "openpr",
        "external_id": "issue#transaction-retry",
        "title": "Transactional retry",
        "content": "this content is long enough to exercise webhook memory persistence",
        "actor": "project_bot",
        "timestamp": "2026-07-15T13:00:00Z"
    })
    .to_string();
    let make_request = || {
        Request::builder()
            .method("POST")
            .uri("/webhook/events")
            .header("content-type", "application/json")
            .header("authorization", "Bearer secret")
            .header("X-Idempotency-Key", "transaction-retry-key")
            .body(Body::from(body.clone()))
            .unwrap()
    };

    let failed = app.clone().oneshot(make_request()).await.unwrap();
    assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);

    let topic_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM topics WHERE external_id = 'issue#transaction-retry'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let participant_count: i64 = conn
        .query_row(
            "SELECT COUNT(*)
                   FROM topic_participants tp
                   JOIN topics t ON t.id = tp.topic_id
                  WHERE t.external_id = 'issue#transaction-retry'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let memory_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE key LIKE 'webhook:custom:issue#transaction-retry:%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(topic_count, 0, "failed ingestion must not strand a topic");
    assert_eq!(participant_count, 0, "failed ingestion must not strand a participant");
    assert_eq!(memory_count, 0, "failed ingestion must not strand memory");

    let failed_status: String = conn
        .query_row(
            "SELECT status FROM webhook_ingestions WHERE external_id = 'issue#transaction-retry'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(failed_status, "failed");

    conn.execute_batch("DROP TRIGGER fail_webhook_memory_insert;").unwrap();
    let retry = app.oneshot(make_request()).await.unwrap();
    assert_eq!(retry.status(), StatusCode::OK);

    let committed_status: String = conn
        .query_row(
            "SELECT status FROM webhook_ingestions WHERE external_id = 'issue#transaction-retry'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(committed_status, "committed");
    let committed_topic_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM topics WHERE external_id = 'issue#transaction-retry'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let committed_memory_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE key LIKE 'webhook:custom:issue#transaction-retry:%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let outbox_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_events
                  WHERE event_type = 'webhook.event.committed'
                    AND subject_table = 'webhook_ingestions'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(committed_topic_count, 1);
    assert_eq!(committed_memory_count, 1);
    assert_eq!(outbox_count, 1);
}

#[tokio::test]
async fn readonly_autonomy_blocks_persist_with_forbidden() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("memory").join("brain.db");
    let app = router(setup_readonly_state(&tmp, "secret"));

    let req = Request::builder()
        .method("POST")
        .uri("/webhook/events")
        .header("content-type", "application/json")
        .header("authorization", "Bearer secret")
        .body(Body::from(
            json!({
                "source": "custom",
                "event_type": "issue.created",
                "project": "openpr",
                "external_id": "issue#readonly",
                "title": "Readonly blocked",
                "content": "must not persist",
                "timestamp": Utc::now().to_rfc3339()
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    // Auth/signature/idempotency all pass; the SideEffectGate denies the
    // write because autonomy is ReadOnly, so the handler returns 403.
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // And nothing was written to the topic store.
    let conn = Connection::open(db_path).unwrap();
    let topic_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM topics WHERE external_id = 'issue#readonly'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(topic_count, 0);
}

#[tokio::test]
async fn supervised_autonomy_allows_persist() {
    // Sanity counterpart to the ReadOnly test: default autonomous policy
    // permits a Low-risk persist, proving the gate is not over-blocking.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("memory").join("brain.db");
    let app = router(setup_state(&tmp, "secret"));

    let req = Request::builder()
        .method("POST")
        .uri("/webhook/events")
        .header("content-type", "application/json")
        .header("authorization", "Bearer secret")
        .body(Body::from(
            json!({
                "source": "custom",
                "event_type": "issue.created",
                "project": "openpr",
                "external_id": "issue#supervised",
                "title": "Supervised allowed",
                "content": "persisted",
                "timestamp": Utc::now().to_rfc3339()
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let conn = Connection::open(db_path).unwrap();
    let topic_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM topics WHERE external_id = 'issue#supervised'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(topic_count, 1);
}

#[tokio::test]
async fn webhook_rate_limit_rejects_excess_requests() {
    let tmp = TempDir::new().unwrap();
    let app = router(setup_state_with_limits(&tmp, "secret", 1, 1024));
    let make_req = || {
        Request::builder()
            .method("POST")
            .uri("/webhook/events")
            .header("content-type", "application/json")
            .header("authorization", "Bearer secret")
            .body(Body::from(
                json!({
                    "source": "custom",
                    "event_type": "issue.created",
                    "external_id": format!("issue#{}", Uuid::new_v4()),
                    "title": "rl",
                    "content": "rl",
                    "timestamp": Utc::now().to_rfc3339()
                })
                .to_string(),
            ))
            .unwrap()
    };

    let first = app.clone().oneshot(make_req()).await.unwrap();
    assert_eq!(first.status(), StatusCode::OK);

    let second = app.oneshot(make_req()).await.unwrap();
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[test]
fn standalone_repository_is_injected_at_daemon_boundary() {
    let webhook_source = include_str!("mod.rs");
    let runner = webhook_source
        .split_once("pub(crate) async fn run_configured_with_repository_generation(")
        .unwrap()
        .1
        .split_once("pub(crate) fn repository_from_config(")
        .unwrap()
        .0;
    assert!(runner.contains("repository: WebhookRepositoryHandle"));
    assert!(!runner.contains("repository_from_config(config)"));

    let daemon_source = include_str!("../daemon/mod.rs");
    let webhook_supervisor = daemon_source
        .split_once("let (webhook_handle, webhook_controller) = spawn_config_generation_supervisor(")
        .unwrap()
        .1
        .split_once("handles.push(webhook_handle)")
        .unwrap()
        .0;
    assert!(webhook_supervisor.contains("repository_from_config(&cfg)"));
    assert!(webhook_supervisor.contains("run_configured_with_repository_generation("));
}

/// Every PostgreSQL checkout this repository makes must ask for the
/// restricted session context.
///
/// The pool's default is the *trusted* system context, so a checkout that
/// forgets to name a context silently gains an RLS bypass -- which is
/// exactly the regression this guards. Asserted against the source text
/// because the risk is a future checkout site being added without the
/// constant, which no runtime assertion on the existing sites would catch.
#[test]
fn postgres_webhook_checkouts_all_request_the_restricted_context() {
    assert_eq!(
        WEBHOOK_SESSION_CONTEXT,
        crate::memory::postgres::PostgresSessionContext::Restricted,
        "webhook ingestion handles externally-originated payloads and must stay under RLS"
    );

    let webhook_source = include_str!("mod.rs");
    let repository = webhook_source
        .split_once("impl PostgresWebhookRepository {")
        .unwrap()
        .1
        .split_once("\n#[async_trait]")
        .unwrap()
        .0;
    assert!(
        !repository.contains(".checkout_in(crate::memory::postgres::PostgresSessionContext::System)"),
        "webhook must never check out the trusted system context"
    );
    for (call, expected) in [
        ("self.pool.checkout_in(", 3),
        ("self.pool.checkout_in(WEBHOOK_SESSION_CONTEXT)", 3),
    ] {
        assert_eq!(
            repository.matches(call).count(),
            expected,
            "every `{call}` site must carry WEBHOOK_SESSION_CONTEXT"
        );
    }
    // The schema bootstrap is a write path like any other. Scanned inside
    // `repository` rather than the whole file so this assertion cannot
    // match the literal in its own source.
    assert!(
        repository.contains("init_pool.with_client_in(WEBHOOK_SESSION_CONTEXT,"),
        "schema initialization must also run under the restricted context"
    );
    assert!(
        !repository.contains("init_pool.with_client(|"),
        "schema initialization must not fall back to the default system context"
    );
}

/// The restricted context is actually established on the connection the
/// webhook repository hands to its queries, and the trusted context is
/// still available to the *next* user of that same pooled connection.
///
/// This reads the session GUCs rather than provoking a policy denial, so
/// it holds under any role -- including the superuser setups where RLS
/// short-circuits and an effect-based assertion would prove nothing.
#[tokio::test]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_webhook_session_context_does_not_leak_from_env() {
    let db_url = crate::memory::postgres::live_postgres_url();

    let schema = format!("openprx_whctx_{}", Uuid::new_v4().simple());
    let repository = PostgresWebhookRepository::new(
        &db_url,
        &schema,
        "memories",
        Some(5),
        // One connection, so the "next user" below is necessarily the same
        // backend the webhook checkout just released.
        1,
        "postgres-webhook-session-context".to_string(),
    )
    .unwrap();

    let pool = Arc::clone(&repository.pool);
    crate::runtime::blocking::spawn_blocking(move || {
        let webhook_backend: i32 = {
            let mut checkout = pool.checkout_in(WEBHOOK_SESSION_CONTEXT).unwrap();
            let client = checkout.client_mut().unwrap();
            let row = client
                .query_one(
                    "SELECT current_setting('prx.rls_bypass', true), \
                                current_setting('prx.current_workspace', true), \
                                current_setting('prx.current_owner', true), \
                                pg_backend_pid()",
                    &[],
                )
                .unwrap();
            assert_eq!(row.get::<_, String>(0), "off", "webhook checkout must not bypass RLS");
            assert_eq!(
                row.get::<_, String>(1),
                "",
                "webhook checkout must not inherit a workspace"
            );
            assert_eq!(
                row.get::<_, String>(2),
                "",
                "webhook checkout must not inherit an owner"
            );
            row.get(3)
        };

        let trusted_backend: i32 = pool
            .with_client_in(crate::memory::postgres::PostgresSessionContext::System, |client| {
                let row = client.query_one("SELECT current_setting('prx.rls_bypass', true), pg_backend_pid()", &[])?;
                assert_eq!(
                    row.get::<_, String>(0),
                    "on",
                    "a webhook checkout must not strip the bypass from the connection it returns"
                );
                Ok(row.get(1))
            })
            .unwrap();

        assert_eq!(
            webhook_backend, trusted_backend,
            "the leak check is only meaningful when both checkouts share a backend"
        );
    })
    .await
    .unwrap();

    drop(repository);
    crate::runtime::blocking::spawn_blocking(move || {
        let mut client = PostgresClient::connect(&db_url, NoTls).unwrap();
        client
            .batch_execute(&format!(
                "DROP SCHEMA {} CASCADE",
                crate::memory::postgres::quote_identifier(&schema)
            ))
            .unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_webhook_repository_conformance_from_env() {
    let db_url = crate::memory::postgres::live_postgres_url();

    let schema = format!("openprx_wh_{}", Uuid::new_v4().simple());
    let repository = PostgresWebhookRepository::new(
        &db_url,
        &schema,
        "memories",
        Some(5),
        crate::memory::postgres::POSTGRES_DEFAULT_POOL_MAX_SIZE,
        "postgres-webhook-conformance".to_string(),
    )
    .unwrap();
    let event = WebhookEvent {
        source: "custom".to_string(),
        event_type: "issue.created".to_string(),
        project: Some("openpr".to_string()),
        external_id: "issue#postgres-conformance".to_string(),
        external_url: Some("https://example.invalid/issues/1".to_string()),
        title: "PostgreSQL webhook conformance".to_string(),
        content: "configured PostgreSQL webhook repository stores an atomic projection".to_string(),
        actor: Some("project_bot".to_string()),
        timestamp: "2026-07-15T14:00:00Z".to_string(),
    };
    let request_hash = webhook_request_hash(b"postgres-webhook-conformance");
    let event_identity = webhook_event_identity(&event, &request_hash);
    let claim = match repository
        .claim(
            "event:postgres-conformance".to_string(),
            event_identity.clone(),
            request_hash.clone(),
            &event,
        )
        .await
        .unwrap()
    {
        WebhookClaimOutcome::Acquired(claim) => claim,
        outcome => panic!("expected acquired PostgreSQL claim, got {outcome:?}"),
    };
    let topic_id = repository.commit(&claim, &event, &event.content, true).await.unwrap();
    assert!(!topic_id.is_empty(), "expected a non-empty value");

    assert!(matches!(
        repository
            .claim(
                "event:postgres-conformance".to_string(),
                event_identity,
                request_hash,
                &event,
            )
            .await
            .unwrap(),
        WebhookClaimOutcome::Committed { topic_id: replayed } if replayed == topic_id
    ));

    let query_url = db_url.clone();
    let qualified_ingestions = repository.qualified_ingestions.to_string();
    let qualified_memories = repository.qualified_memories.to_string();
    let qualified_memory_events = repository.qualified_memory_events.to_string();
    let (ingestion_count, memory_count, event_count) = crate::runtime::blocking::spawn_blocking(move || {
        let mut client = PostgresClient::connect(&query_url, NoTls).unwrap();
        let ingestion_count: i64 = client
            .query_one(
                &format!("SELECT COUNT(*) FROM {qualified_ingestions} WHERE status = 'committed'"),
                &[],
            )
            .unwrap()
            .get(0);
        let memory_count: i64 = client
            .query_one(
                &format!(
                    "SELECT COUNT(*) FROM {qualified_memories} \
                         WHERE key LIKE 'webhook:custom:issue#postgres-conformance:%'"
                ),
                &[],
            )
            .unwrap()
            .get(0);
        let event_count: i64 = client
            .query_one(
                &format!(
                    "SELECT COUNT(*) FROM {qualified_memory_events} \
                         WHERE event_type = 'webhook.event.committed'"
                ),
                &[],
            )
            .unwrap()
            .get(0);
        (ingestion_count, memory_count, event_count)
    })
    .await
    .unwrap();
    assert_eq!((ingestion_count, memory_count, event_count), (1, 1, 1));

    drop(repository);
    crate::runtime::blocking::spawn_blocking(move || {
        let mut client = PostgresClient::connect(&db_url, NoTls).unwrap();
        client
            .batch_execute(&format!(
                "DROP SCHEMA {} CASCADE",
                crate::memory::postgres::quote_identifier(&schema)
            ))
            .unwrap();
    })
    .await
    .unwrap();
}

fn seed_named_scope(conn: &Connection) -> String {
    const REV: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const U: &str = "00000000-0000-4000-8000-0000000000a1";
    const S: &str = "00000000-0000-4000-8000-0000000000c1";
    conn.execute(
        "INSERT INTO agent_incarnations VALUES ('ws', 'ow', 'n:x', ?1, 'active', ?2)",
        params![U, REV],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO agent_scopes VALUES (?1, 'ws', 'ow', 'sess', 'n:x', ?2, 1)",
        params![S, U],
    )
    .unwrap();
    S.to_string()
}

#[tokio::test]
async fn webhook_does_not_mutate_a_named_topic() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("memory").join("brain.db");
    let app = router(setup_state(&tmp, "secret"));
    let conn = Connection::open(&db_path).unwrap();
    let scope = seed_named_scope(&conn);
    let named_id = crate::memory::topic::create_topic(
        &conn,
        "Issue 42",
        Some("openpr"),
        Some("issue#42"),
        "fp-named-issue",
        Some(&scope),
    )
    .unwrap();
    let updated_before: String = conn
        .query_row(
            "SELECT updated_at FROM topics WHERE id = ?1",
            params![named_id],
            |row| row.get(0),
        )
        .unwrap();

    let req = Request::builder()
        .method("POST")
        .uri("/webhook/events")
        .header("content-type", "application/json")
        .header("authorization", "Bearer secret")
        .body(Body::from(
            json!({
                "source": "custom",
                "event_type": "issue.created",
                "project": "openpr",
                "external_id": "issue#42",
                "external_url": "https://example.com/issues/42",
                "title": "Issue 42",
                "content": "details",
                "timestamp": Utc::now().to_rfc3339()
            })
            .to_string(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let topic_id = parsed.get("topic_id").and_then(serde_json::Value::as_str).unwrap();
    assert_ne!(topic_id, named_id, "webhook must not reuse a Named topic");

    let updated_after: String = conn
        .query_row(
            "SELECT updated_at FROM topics WHERE id = ?1",
            params![named_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(updated_after, updated_before, "Named topic must stay untouched");
    let named_stamp: Option<String> = conn
        .query_row(
            "SELECT ma_scope_id FROM topics WHERE id = ?1",
            params![named_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(named_stamp.as_deref(), Some(scope.as_str()));
    let sidecar_stamp: Option<String> = conn
        .query_row(
            "SELECT ma_scope_id FROM topics WHERE id = ?1",
            params![topic_id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(sidecar_stamp.is_none(), "webhook sidecar stays unstamped");
    let named_participants: i64 = conn
        .query_row(
            "SELECT count(*) FROM topic_participants WHERE topic_id = ?1",
            params![named_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        named_participants, 0,
        "webhook must not add participants to a Named topic"
    );
}

#[tokio::test]
async fn webhook_refuses_unstamped_overwrite_of_a_named_memory() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("memory").join("brain.db");
    let app = router(setup_state(&tmp, "secret"));
    let conn = Connection::open(&db_path).unwrap();
    let scope = seed_named_scope(&conn);
    let raw_key = "idem-named-overwrite";
    let ingestion_key = format!("header:{}", webhook_scoped_key_digest("secret", raw_key));
    let memory_key = format!("webhook:custom:issue#99:{ingestion_key}");
    conn.execute(
            "INSERT INTO memories (id, key, content, category, created_at, updated_at, visibility, sensitivity, ma_scope_id)
             VALUES ('m-named', ?1, 'named-original', 'core', 't', 't', 'owner', 'normal', ?2)",
            params![memory_key, scope],
        )
        .unwrap();

    let req = Request::builder()
        .method("POST")
        .uri("/webhook/events")
        .header("content-type", "application/json")
        .header("authorization", "Bearer secret")
        .header("X-Idempotency-Key", raw_key)
        .body(Body::from(
            json!({
                "source": "custom",
                "event_type": "issue.created",
                "project": "openpr",
                "external_id": "issue#99",
                "title": "Issue 99",
                "content": "details that would overwrite",
                "timestamp": Utc::now().to_rfc3339()
            })
            .to_string(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let content: String = conn
        .query_row(
            "SELECT content FROM memories WHERE key = ?1",
            params![memory_key],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(content, "named-original");
    let stamp: Option<String> = conn
        .query_row(
            "SELECT ma_scope_id FROM memories WHERE key = ?1",
            params![memory_key],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stamp.as_deref(), Some(scope.as_str()));
}
