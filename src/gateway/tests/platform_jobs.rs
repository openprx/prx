#![cfg(test)]

use super::*;

const PLATFORM_LINQ_SECRET: &str = "linq-signing-secret";

const PLATFORM_NC_SECRET: &str = "nextcloud-webhook-secret";

async fn local_platform_fixture<F, Fut>(business: F)
where
    F: FnOnce(AppState, Arc<BlockFirstProvider>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    use futures_util::FutureExt as _;
    let transport = platform_test_transport::LocalTransport::start()
        .await
        .expect("owned platform TLS endpoint");
    let endpoint = transport.endpoint.clone();
    let client = transport.client.clone();
    let outcome = std::panic::AssertUnwindSafe(owned_blocking_webhook_fixture(
        |mut state, provider, _callers| async move {
            state.whatsapp = Some(Arc::new(
                WhatsAppChannel::new(
                    "wa-token".into(),
                    "endpoint-id".into(),
                    "verify-token".into(),
                    vec!["*".into()],
                )
                .with_test_transport(&endpoint, client.clone())
                .expect("WhatsApp loopback only"),
            ));
            state.whatsapp_app_secret = Some(Arc::from(PLATFORM_WA_SECRET));
            state.linq = Some(Arc::new(
                LinqChannel::new("linq-token".into(), "+15550000000".into(), vec!["*".into()])
                    .with_test_transport(&endpoint, client.clone())
                    .expect("Linq loopback only"),
            ));
            state.linq_signing_secret = Some(Arc::from(PLATFORM_LINQ_SECRET));
            state.nextcloud_talk = Some(Arc::new(
                NextcloudTalkChannel::new(endpoint.clone(), "app-token".into(), vec!["*".into()])
                    .with_test_transport(&endpoint, client)
                    .expect("Nextcloud loopback only"),
            ));
            state.nextcloud_talk_webhook_secret = Some(Arc::from(PLATFORM_NC_SECRET));
            business(state, provider).await;
        },
    ))
    .catch_unwind()
    .await;
    let drained = transport.finish().await;
    if let Err(error) = &drained {
        let subscriber = tracing_subscriber::fmt()
            .with_test_writer()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::ERROR)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(%error, "owned platform transport cleanup failed");
        });
    }
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
    drained.expect("owned platform TLS listener joined");
}

#[tokio::test]
async fn platform_real_adapters_send_only_to_owned_tls_endpoint() {
    use futures_util::FutureExt as _;
    let transport = platform_test_transport::LocalTransport::start()
        .await
        .expect("owned sender endpoint");
    let work = Box::pin(async {
        let channels: Vec<Box<dyn Channel>> = vec![
            Box::new(
                WhatsAppChannel::new(
                    "fixture-token".into(),
                    "fixture-endpoint".into(),
                    "fixture-verify".into(),
                    vec![],
                )
                .with_test_transport(&transport.endpoint, transport.client.clone())
                .expect("local WhatsApp"),
            ),
            Box::new(
                LinqChannel::new("fixture-token".into(), "fixture-sender".into(), vec![])
                    .with_test_transport(&transport.endpoint, transport.client.clone())
                    .expect("local Linq"),
            ),
            Box::new(
                NextcloudTalkChannel::new(transport.endpoint.clone(), "fixture-token".into(), vec![])
                    .with_test_transport(&transport.endpoint, transport.client.clone())
                    .expect("local Nextcloud"),
            ),
        ];
        for channel in channels {
            channel
                .send(&SendMessage::new("PRX delivery probe", "fixture-recipient"))
                .await
                .expect("actual local send");
        }
        let records = transport.records();
        assert_eq!(records.len(), 3);
        assert!(records[0].0.starts_with("POST /v18.0/fixture-endpoint/messages "));
        assert_eq!(
            records[0].1.pointer("/text/body").and_then(serde_json::Value::as_str),
            Some("PRX delivery probe")
        );
        assert!(
            records[1]
                .0
                .starts_with("POST /api/partner/v3/chats/fixture-recipient/messages ")
        );
        assert_eq!(
            records[1]
                .1
                .pointer("/message/parts/0/value")
                .and_then(serde_json::Value::as_str),
            Some("PRX delivery probe")
        );
        assert!(
            records[2]
                .0
                .starts_with("POST /ocs/v2.php/apps/spreed/api/v1/chat/fixture-recipient?format=json ")
        );
        assert_eq!(
            records[2].1.get("message").and_then(serde_json::Value::as_str),
            Some("PRX delivery probe")
        );
    });
    let outcome = std::panic::AssertUnwindSafe(work).catch_unwind().await;
    let drained = transport.finish().await;
    if let Err(error) = &drained {
        let subscriber = tracing_subscriber::fmt()
            .with_test_writer()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::ERROR)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(%error, "owned platform transport cleanup failed");
        });
    }
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
    drained.expect("sender endpoint joined");
}

fn platform_webhook_state(provider: Arc<dyn Provider>) -> AppState {
    AppState {
        whatsapp: Some(Arc::new(WhatsAppChannel::new(
            "wa-token".into(),
            "endpoint-id".into(),
            "verify-token".into(),
            vec!["*".into()],
        ))),
        whatsapp_app_secret: Some(Arc::from(PLATFORM_WA_SECRET)),
        linq: Some(Arc::new(LinqChannel::new(
            "linq-token".into(),
            "+15550000000".into(),
            vec!["*".into()],
        ))),
        linq_signing_secret: Some(Arc::from(PLATFORM_LINQ_SECRET)),
        nextcloud_talk: Some(Arc::new(NextcloudTalkChannel::new(
            "https://cloud.example.com".into(),
            "app-token".into(),
            vec!["*".into()],
        ))),
        nextcloud_talk_webhook_secret: Some(Arc::from(PLATFORM_NC_SECRET)),
        ..webhook_test_state(provider)
    }
}

/// Distinct payload bodies per test: the delivery ledger is process-wide, so
/// two tests sharing a body would see each other's claims.
fn whatsapp_payload(marker: &str) -> String {
    serde_json::json!({
        "object": "whatsapp_business_account",
        "entry": [{"changes": [{"value": {"messages": [{
            "from": "15551230000",
            "id": format!("wamid.{marker}"),
            "timestamp": "1700000000",
            "text": {"body": format!("hello {marker}")},
        }]}}]}],
    })
    .to_string()
}

fn linq_payload(marker: &str) -> String {
    serde_json::json!({
        "event_type": "message.received",
        "data": {
            "from": "15551230000",
            "chat_id": format!("chat-{marker}"),
            "message": {"parts": [{"type": "text", "value": format!("hello {marker}")}]},
        },
    })
    .to_string()
}

fn nextcloud_payload(marker: &str) -> String {
    serde_json::json!({
        "type": "message",
        "object": {"token": format!("room-{marker}")},
        "message": {
            "id": marker,
            "actorType": "users",
            "actorId": "user_a",
            "messageType": "comment",
            "message": format!("hello {marker}"),
        },
    })
    .to_string()
}

fn whatsapp_headers(body: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let signature = compute_whatsapp_signature_header(PLATFORM_WA_SECRET, body.as_bytes());
    headers.insert(
        "X-Hub-Signature-256",
        HeaderValue::from_str(&signature).expect("test: signature header"),
    );
    headers
}

fn linq_headers(body: &str) -> HeaderMap {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let timestamp = chrono::Utc::now().timestamp().to_string();
    let mut mac = Hmac::<Sha256>::new_from_slice(PLATFORM_LINQ_SECRET.as_bytes()).expect("test: linq hmac key");
    mac.update(format!("{timestamp}.{body}").as_bytes());
    let signature = hex::encode(mac.finalize().into_bytes());

    let mut headers = HeaderMap::new();
    headers.insert(
        "X-Webhook-Timestamp",
        HeaderValue::from_str(&timestamp).expect("test: timestamp header"),
    );
    headers.insert(
        "X-Webhook-Signature",
        HeaderValue::from_str(&signature).expect("test: signature header"),
    );
    headers
}

fn nextcloud_headers(body: &str) -> HeaderMap {
    let random = "seed-value";
    let signature = compute_nextcloud_signature_hex(PLATFORM_NC_SECRET, random, body);
    let mut headers = HeaderMap::new();
    headers.insert(
        "X-Nextcloud-Talk-Random",
        HeaderValue::from_str(random).expect("test: random header"),
    );
    headers.insert(
        "X-Nextcloud-Talk-Signature",
        HeaderValue::from_str(&signature).expect("test: signature header"),
    );
    headers
}

async fn response_json(response: axum::response::Response) -> serde_json::Value {
    let payload = response.into_body().collect().await.expect("test: body").to_bytes();
    serde_json::from_slice(&payload).expect("test: json body")
}

/// Bound the wait so the inline-await mutation fails in seconds instead of
/// hanging the suite. Nothing in production reads a clock here.
const PLATFORM_ACK_TEST_BUDGET: Duration = Duration::from_secs(20);

/// Drive one platform webhook while its turn is wedged inside the provider,
/// and assert the three properties the route now owes the platform: an
/// immediate ack, a running job the operator can see, and a kill that ends it.
async fn assert_acks_before_the_turn_finishes(
    provider: Arc<BlockFirstProvider>,
    expected_label_prefix: &str,
    response: axum::response::Response,
) {
    assert_eq!(response.status(), StatusCode::OK, "the platform must see a 2xx ack");
    let body = response_json(response).await;
    assert_eq!(body["status"], "accepted", "the ack reports acceptance, not completion");
    assert_eq!(body["kind"], jobs::KIND_CHANNEL_WEBHOOK);

    let job_id = body["job_id"].as_str().expect("test: job id");
    let job_uuid = Uuid::parse_str(job_id).expect("test: job uuid");
    let work_id = runtime::registry::WorkId::parse(body["work_id"].as_str().expect("test: work id"))
        .expect("test: work id parses");

    // The turn is genuinely under way *after* the platform was answered.
    tokio::time::timeout(PLATFORM_ACK_TEST_BUDGET, provider.first_started.notified())
        .await
        .expect("test: the turn must start in the background after the ack");

    let registry_row = runtime::registry::snapshot(work_id).expect("test: job must appear in `prx tasks list`");
    assert!(
        registry_row.name.starts_with(expected_label_prefix),
        "registry row should name the delivery target, got {}",
        registry_row.name
    );
    assert_eq!(
        jobs::snapshot(job_uuid).map(|snapshot| snapshot.status),
        Some("running"),
        "the job is still running while the platform already has its ack"
    );

    // `prx tasks kill <id>` ends it — the visibility is not decorative.
    runtime::registry::kill(work_id, true).await;
    for _ in 0..200u32 {
        if jobs::snapshot(job_uuid).is_some_and(|snapshot| snapshot.status == "cancelled") {
            provider.release_first.notify_one();
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    provider.release_first.notify_one();
    panic!("killing the registry row did not end the webhook job");
}

#[tokio::test]
async fn whatsapp_webhook_acks_before_the_turn_finishes() {
    local_platform_fixture(|state, provider_impl| async move {
        let body = whatsapp_payload("ack-evidence");
        let headers = whatsapp_headers(&body);

        let response = tokio::time::timeout(
            PLATFORM_ACK_TEST_BUDGET,
            handle_whatsapp_message(State(state), headers, Bytes::from(body)),
        )
        .await
        .expect("test: the webhook must answer while the turn is still running")
        .into_response();

        assert_acks_before_the_turn_finishes(provider_impl, "gateway:whatsapp:", response).await;
    })
    .await;
}

#[tokio::test]
async fn linq_webhook_acks_before_the_turn_finishes() {
    local_platform_fixture(|state, provider_impl| async move {
        let body = linq_payload("ack-evidence");
        let headers = linq_headers(&body);

        let response = tokio::time::timeout(
            PLATFORM_ACK_TEST_BUDGET,
            handle_linq_webhook(State(state), headers, Bytes::from(body)),
        )
        .await
        .expect("test: the webhook must answer while the turn is still running")
        .into_response();

        assert_acks_before_the_turn_finishes(provider_impl, "gateway:linq:", response).await;
    })
    .await;
}

#[tokio::test]
async fn nextcloud_talk_webhook_acks_before_the_turn_finishes() {
    local_platform_fixture(|state, provider_impl| async move {
        let body = nextcloud_payload("ack-evidence");
        let headers = nextcloud_headers(&body);

        let response = tokio::time::timeout(
            PLATFORM_ACK_TEST_BUDGET,
            handle_nextcloud_talk_webhook(State(state), headers, Bytes::from(body)),
        )
        .await
        .expect("test: the webhook must answer while the turn is still running")
        .into_response();

        assert_acks_before_the_turn_finishes(provider_impl, "gateway:nextcloud_talk:", response).await;
    })
    .await;
}

/// A platform retry of a delivery already accepted must not start a second
/// turn. Meta alone will retry the same payload for up to seven days.
async fn assert_redelivery_runs_once(
    provider: Arc<BlockFirstProvider>,
    first: axum::response::Response,
    second: axum::response::Response,
) {
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(second.status(), StatusCode::OK, "a retry must still be acked");
    let first_body = response_json(first).await;
    let second_body = response_json(second).await;
    assert_eq!(first_body["status"], "accepted");
    assert_eq!(second_body["status"], "duplicate");
    assert_eq!(second_body["idempotent"], true);

    tokio::time::timeout(PLATFORM_ACK_TEST_BUDGET, provider.first_started.notified())
        .await
        .expect("test: the first delivery must have started a turn");
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        1,
        "the redelivery must not have started a second turn"
    );

    if let Some(work_id) = runtime::registry::WorkId::parse(first_body["work_id"].as_str().unwrap_or_default()) {
        runtime::registry::kill(work_id, true).await;
    }
    provider.release_first.notify_one();
}

#[tokio::test]
async fn whatsapp_redelivery_does_not_run_the_turn_twice() {
    local_platform_fixture(|state, provider_impl| async move {
        let body = whatsapp_payload("dedup-evidence");
        let headers = whatsapp_headers(&body);

        // Bounded so the inline-await shape fails here instead of hanging.
        let first = tokio::time::timeout(
            PLATFORM_ACK_TEST_BUDGET,
            handle_whatsapp_message(State(state.clone()), headers.clone(), Bytes::from(body.clone())),
        )
        .await
        .expect("test: the first delivery must be acked while its turn runs")
        .into_response();
        let second = tokio::time::timeout(
            PLATFORM_ACK_TEST_BUDGET,
            handle_whatsapp_message(State(state), headers, Bytes::from(body)),
        )
        .await
        .expect("test: the redelivery must be acked without waiting on the first turn")
        .into_response();

        assert_redelivery_runs_once(provider_impl, first, second).await;
    })
    .await;
}

#[tokio::test]
async fn linq_redelivery_does_not_run_the_turn_twice() {
    local_platform_fixture(|state, provider_impl| async move {
        let body = linq_payload("dedup-evidence");
        let headers = linq_headers(&body);

        // Bounded so the inline-await shape fails here instead of hanging.
        let first = tokio::time::timeout(
            PLATFORM_ACK_TEST_BUDGET,
            handle_linq_webhook(State(state.clone()), headers.clone(), Bytes::from(body.clone())),
        )
        .await
        .expect("test: the first delivery must be acked while its turn runs")
        .into_response();
        let second = tokio::time::timeout(
            PLATFORM_ACK_TEST_BUDGET,
            handle_linq_webhook(State(state), headers, Bytes::from(body)),
        )
        .await
        .expect("test: the redelivery must be acked without waiting on the first turn")
        .into_response();

        assert_redelivery_runs_once(provider_impl, first, second).await;
    })
    .await;
}

#[tokio::test]
async fn nextcloud_talk_redelivery_does_not_run_the_turn_twice() {
    local_platform_fixture(|state, provider_impl| async move {
        let body = nextcloud_payload("dedup-evidence");
        let headers = nextcloud_headers(&body);

        // Bounded so the inline-await shape fails here instead of hanging.
        let first = tokio::time::timeout(
            PLATFORM_ACK_TEST_BUDGET,
            handle_nextcloud_talk_webhook(State(state.clone()), headers.clone(), Bytes::from(body.clone())),
        )
        .await
        .expect("test: the first delivery must be acked while its turn runs")
        .into_response();
        let second = tokio::time::timeout(
            PLATFORM_ACK_TEST_BUDGET,
            handle_nextcloud_talk_webhook(State(state), headers, Bytes::from(body)),
        )
        .await
        .expect("test: the redelivery must be acked without waiting on the first turn")
        .into_response();

        assert_redelivery_runs_once(provider_impl, first, second).await;
    })
    .await;
}

#[tokio::test]
async fn platform_webhook_rejects_bad_signature_before_claiming_the_delivery() {
    let provider_impl = Arc::new(MockProvider::default());
    let provider: Arc<dyn Provider> = provider_impl.clone();
    let state = platform_webhook_state(provider);
    let body = whatsapp_payload("signature-evidence");

    let mut headers = HeaderMap::new();
    headers.insert("X-Hub-Signature-256", HeaderValue::from_static("sha256=deadbeef"));
    let rejected = handle_whatsapp_message(State(state.clone()), headers, Bytes::from(body.clone()))
        .await
        .into_response();
    assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 0);

    // A forged delivery must not have consumed the ledger entry for the real
    // one: the same payload, correctly signed, still runs.
    let accepted = handle_whatsapp_message(State(state), whatsapp_headers(&body), Bytes::from(body))
        .await
        .into_response();
    assert_eq!(accepted.status(), StatusCode::OK);
    let accepted_body = response_json(accepted).await;
    assert_eq!(accepted_body["status"], "accepted");
    if let Some(work_id) = runtime::registry::WorkId::parse(accepted_body["work_id"].as_str().unwrap_or_default()) {
        runtime::registry::kill(work_id, true).await;
    }
}

// D4 C4: gateway fabric durable session_key migrates to the recipient-aware
// canonical while the legacy key is carried for read-merge. Since D-025 this
// holds for platform channel messages; a `/webhook` turn's key is
// credential-qualified and read-merges nothing
// (`webhook_session_key_is_credential_qualified_and_merges_no_legacy_key`).
#[test]
fn d4_gateway_fabric_durable_key_is_canonical_with_legacy_read_merge() {
    // platform channel ingress (WhatsApp / Linq / Nextcloud Talk)
    let msg = ChannelMessage {
        id: "msg-42".into(),
        sender: "client-42".into(),
        reply_target: "prx".into(),
        channel: "whatsapp".into(),
        ..Default::default()
    };
    let webhook = GatewayFabricContext::channel_message(&msg);
    assert_eq!(webhook.session_key, "gateway:whatsapp:client-42"); // legacy form
    let canonical = RuntimeEnvelope::gateway(
        "ws",
        webhook.session_key.clone(),
        webhook.channel.clone(),
        webhook.sender.clone(),
        webhook.recipient.clone(),
        MemoryVisibility::Session,
    )
    .canonical_session_key();
    // canonical is recipient-aware: gateway:{channel}:{sender}:{recipient}
    assert_eq!(canonical, "gateway:whatsapp:client-42:prx");
    assert_ne!(canonical, webhook.session_key);

    // The migrated envelope writes/reads the canonical durable key and carries
    // the legacy key for read-merge.
    let envelope = RuntimeEnvelope::gateway(
        "ws",
        canonical.clone(),
        webhook.channel.clone(),
        webhook.sender.clone(),
        webhook.recipient.clone(),
        MemoryVisibility::Session,
    )
    .with_legacy_session_key(webhook.session_key);
    assert_eq!(
        envelope.message_scope().session_key.as_deref(),
        Some(canonical.as_str())
    );
    let principal = envelope.memory_principal();
    assert_eq!(principal.session_key.as_deref(), Some(canonical.as_str()));
    assert_eq!(
        principal.legacy_session_key.as_deref(),
        Some("gateway:whatsapp:client-42")
    );
    assert_eq!(
        principal.session_key_candidates(),
        vec![
            "gateway:whatsapp:client-42:prx".to_string(),
            "gateway:whatsapp:client-42".to_string()
        ]
    );
}

/// D-025: a `/webhook` turn's session key is credential-qualified, two
/// credentials never share one, and the pre-upgrade unqualified key is not
/// carried for read-merge.
#[test]
fn webhook_session_key_is_credential_qualified_and_merges_no_legacy_key() {
    let alpha = GatewayFabricContext::generic_webhook(Some("client-42"), "bearer:alpha", None);
    let bravo = GatewayFabricContext::generic_webhook(Some("client-42"), "bearer:bravo", None);
    assert_eq!(alpha.session_key, "gateway:webhook:bearer:alpha:client-42");
    assert_ne!(alpha.session_key, bravo.session_key);
    assert_eq!(
        GatewayFabricContext::generic_webhook(None, "public", None).session_key,
        "gateway:webhook:public:webhook-client"
    );
    assert_ne!(
        alpha.webhook_owner_id("ws"),
        bravo.webhook_owner_id("ws"),
        "owner and session key are qualified by the same credential"
    );
}

#[test]
fn whatsapp_signature_valid() {
    let app_secret = generate_test_secret();
    let body = b"test body content";

    let signature_header = compute_whatsapp_signature_header(&app_secret, body);

    assert!(verify_whatsapp_signature(&app_secret, body, &signature_header));
}
