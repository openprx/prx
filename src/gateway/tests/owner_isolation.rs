#![cfg(test)]

use super::*;

/// R-MCPB F-2: the webhook run owner came from the request body's
/// `reply_target` alone, so a client holding any paired token could name
/// another client's `reply_target` and list (and so kill or steer) that
/// client's sub-agent runs. The owner is now bound to the authenticated
/// credential, with `reply_target` only a sub-namespace under it.
///
/// MUTATION GUARD: drop the credential from the webhook owner and the
/// token-b assertion goes red.
#[tokio::test]
async fn a_webhook_token_cannot_reach_another_tokens_runs_by_naming_its_reply_target() {
    let workspace = tempfile::TempDir::new().expect("test: temp workspace");
    let state = two_token_webhook_state(workspace.path());

    let spawned = webhook_as(
        &state,
        "token-alpha-0001",
        "Use sessions_spawn for OWNER-VIS-SPAWN",
        "client-a",
    )
    .await;
    assert!(spawned.contains("run_id"), "test: token a's spawn ran: {spawned}");

    let by_b = webhook_as(
        &state,
        "token-bravo-0002",
        "Use sessions_spawn for OWNER-VIS-LIST",
        "client-a",
    )
    .await;
    assert!(by_b.contains("TOOL-RESULT"), "test: token b's list ran: {by_b}");
    assert!(
        !by_b.contains(OWNER_VIS_SECRET_TASK),
        "another token must not see token a's run by naming its reply_target: {by_b}"
    );

    let by_a = webhook_as(
        &state,
        "token-alpha-0001",
        "Use sessions_spawn for OWNER-VIS-LIST",
        "client-a",
    )
    .await;
    assert!(
        by_a.contains(OWNER_VIS_SECRET_TASK),
        "the token that started the run still sees it: {by_a}"
    );
}

/// D-025: the webhook conversation session key was
/// `gateway:webhook:{reply_target}`, so a client with another paired token
/// that sent the same `reply_target` was handed the first client's
/// conversation as recalled history. The key is now credential-qualified,
/// `gateway:webhook:{credential}:{reply_target}`, and the old unqualified
/// key is not read-merged (it cannot be attributed to a credential).
///
/// MUTATION GUARD: drop the credential from the webhook session key, or
/// read-merge the old key again, and the token-b assertion goes red.
#[tokio::test]
async fn a_webhook_token_is_not_given_another_tokens_history_by_naming_its_reply_target() {
    const ALPHA_MARKER: &str = "alpha-private-conversation-marker-7f3e";
    let workspace = tempfile::TempDir::new().expect("test: temp workspace");
    let provider = Arc::new(WebhookToolScriptProvider::default());
    let state = two_token_webhook_state_with(workspace.path(), provider.clone());

    let first = webhook_as(&state, "token-alpha-0001", ALPHA_MARKER, "client-a").await;
    assert!(first.contains("response"), "test: token a's turn ran: {first}");
    let again = webhook_as(&state, "token-alpha-0001", "second alpha turn", "client-a").await;
    assert!(again.contains("response"), "test: token a's second turn ran: {again}");
    let alpha_second_turn = provider.seen.lock().last().cloned().unwrap_or_default();
    assert!(
        alpha_second_turn.contains(ALPHA_MARKER),
        "non-vacuity: token a's own next turn recalls its earlier message: {alpha_second_turn}"
    );

    provider.seen.lock().clear();
    let by_b = webhook_as(&state, "token-bravo-0002", "hello from bravo", "client-a").await;
    assert!(by_b.contains("response"), "test: token b's turn ran: {by_b}");
    let seen_by_b = provider.seen.lock().join("\n---\n");
    assert!(
        seen_by_b.contains("hello from bravo"),
        "test: token b's turn reached the model: {seen_by_b}"
    );
    assert!(
        !seen_by_b.contains(ALPHA_MARKER),
        "token b must see no history of token a's conversation: {seen_by_b}"
    );
}

/// D-026 (R-MCPBfg N-2): a webhook has no approval adapter, so its
/// approval-required `memory_store` call is withheld before execution. The
/// proposed content must also remain absent from shared prompt material.
///
/// MUTATION GUARD: mirror every successful store again and this goes red.
#[tokio::test]
async fn a_webhook_memory_store_never_reaches_another_surfaces_system_prompt() {
    let workspace = tempfile::TempDir::new().expect("test: temp workspace");
    let provider = Arc::new(WebhookToolScriptProvider::default());
    let state = two_token_webhook_state_with(workspace.path(), provider.clone());

    let withheld = webhook_as(
        &state,
        "token-alpha-0001",
        "Use memory_store for SHARED-PROMPT-STORE",
        "client-a",
    )
    .await;
    assert!(
        withheld.contains("I can't complete this request under the current policy."),
        "test: the webhook must report the withheld write: {withheld}"
    );
    assert!(
        provider.seen.lock().is_empty(),
        "the approval-required webhook write must be withheld before provider execution"
    );
    assert!(
        state
            .mem
            .get("webhook-note")
            .await
            .expect("test: memory lookup")
            .is_none(),
        "the unapproved webhook write must not reach memory storage"
    );

    let mirror = std::fs::read_to_string(workspace.path().join("MEMORY.md")).unwrap_or_default();
    assert!(!mirror.contains(SHARED_PROMPT_INJECTION), "{mirror}");
    let identity = crate::agent::prompt::build_identity_prompt(workspace.path());
    assert!(
        !identity.contains(SHARED_PROMPT_INJECTION),
        "a webhook caller's text must not reach the shared system prompt: {identity}"
    );
}

/// R-MCPBfg N-4: the webhook owner and session key embedded the plain
/// SHA-256 of the credential — for a paired token exactly the at-rest
/// verifier in `gateway.paired_tokens`, and for an operator-chosen
/// `X-Webhook-Token` or signing secret an offline-guessable digest — in
/// message events, run tags and logs. They now carry a domain-separated
/// digest (`prx-webhook-owner-v1`) that matches no other stored hash.
///
/// MUTATION GUARD: derive the owner credential from `hash_webhook_secret`
/// again and this goes red.
#[tokio::test]
async fn webhook_owner_and_session_key_carry_no_plain_credential_digest() {
    let workspace = tempfile::TempDir::new().expect("test: temp workspace");
    let state = two_token_webhook_state(workspace.path());
    let plain = hash_webhook_secret("token-alpha-0001");

    let answered = webhook_as(&state, "token-alpha-0001", "hello digest", "client-a").await;
    assert!(answered.contains("response"), "test: the turn ran: {answered}");

    let events = all_workspace_message_events(&state, workspace.path()).await;
    let webhook_events = events
        .iter()
        .filter(|event| {
            event
                .session_key
                .as_deref()
                .is_some_and(|key| key.starts_with("gateway:webhook:bearer:"))
        })
        .collect::<Vec<_>>();
    assert!(
        !webhook_events.is_empty(),
        "test: the turn recorded credential-keyed events: {events:?}"
    );
    for event in &events {
        for value in [event.session_key.as_deref(), event.owner_id.as_deref()]
            .into_iter()
            .flatten()
        {
            assert!(
                !value.contains(&plain),
                "a stored key or owner carries the plain token digest: {value}"
            );
        }
    }
}

/// Answers every webhook turn with a reply that echoes the probe secret.
struct SecretReplyProvider;

#[async_trait]
impl Provider for SecretReplyProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok(format!(
            "Here is the key {} you asked for",
            crate::agent::terminal::persistence_probe::PROBE_SECRET
        ))
    }
}

/// Fails every webhook turn with an error that echoes the probe secret.
#[derive(Default)]
struct SecretErrorProvider {
    calls: AtomicUsize,
}

#[async_trait]
impl Provider for SecretErrorProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!(
            "upstream rejected key {}",
            crate::agent::terminal::persistence_probe::PROBE_SECRET
        )
    }
}

/// D-053 / REDACT-TURN probe: drives `handle_webhook` through
/// `run_gateway_chat_with_multimodal` to its `Completed` `finalize_turn`
/// commit with a reply carrying a secret; nothing under the workspace (the
/// SQLite memory, its WAL, the cost ledger) may persist it. Must fail before
/// the REDACT-TURN commits.
#[tokio::test]
async fn gateway_webhook_persists_no_secret_from_a_reply() {
    let workspace = tempfile::TempDir::new().expect("test: temp workspace");
    let state = two_token_webhook_state_with(workspace.path(), Arc::new(SecretReplyProvider));

    let answered = webhook_as(&state, "token-alpha-0001", "hello probe", "client-a").await;
    assert!(
        answered.contains("Here is the key"),
        "test: the turn ran and its reply was delivered: {answered}"
    );
    let events = all_workspace_message_events(&state, workspace.path()).await;
    assert!(!events.is_empty(), "test: the turn recorded message events");

    crate::agent::terminal::persistence_probe::assert_nothing_under_persists_the_probe_secret(workspace.path());
}

/// D-053 / REDACT-TURN probe: drives `handle_webhook` through
/// `run_gateway_chat_with_multimodal` to its `Failed` `finalize_turn` commit
/// with a provider error carrying a secret; nothing under the workspace may
/// persist it. Must fail before the REDACT-TURN commits.
#[tokio::test]
async fn gateway_webhook_persists_no_secret_from_a_provider_error() {
    let workspace = tempfile::TempDir::new().expect("test: temp workspace");
    let provider_impl = Arc::new(SecretErrorProvider::default());
    let provider: Arc<dyn Provider> = provider_impl.clone();
    let state = two_token_webhook_state_with(workspace.path(), provider);
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer token-alpha-0001"),
    );
    let body = serde_json::json!({"message": "hello probe", "reply_target": "client-a"}).to_string();

    let response = handle_webhook(
        State(state.clone()),
        test_connect_info(),
        test_uri(),
        headers,
        Bytes::from(body),
    )
    .await
    .into_response();
    assert_eq!(
        response.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "test: the provider failure must end the turn"
    );
    assert!(
        provider_impl.calls.load(Ordering::SeqCst) >= 1,
        "test: the turn must have reached the provider"
    );
    let events = all_workspace_message_events(&state, workspace.path()).await;
    assert!(!events.is_empty(), "test: the failed turn recorded message events");

    crate::agent::terminal::persistence_probe::assert_nothing_under_persists_the_probe_secret(workspace.path());
}
