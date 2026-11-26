mod model_binding;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use super::*;

/// The engine switch and backend a test configures.
#[derive(Default)]
struct Section {
    enabled: bool,
    backend: crate::config::schema::decision_engine::DecisionBackendConfig,
}
use crate::config::schema::decision_engine::CredentialRefSetting;
use crate::cost::decision_attempt::error::{PolicyRule, ValidationRule};
use crate::decision_engine::prediction::mock::{MockDecisionBackend, MockOutcome, MockStep};
use crate::decision_engine::prediction::protocol::{
    DecisionQuestion, DecisionResponse, GlobalQuestion, QuestionId, RequestLimits, RequestParts, StructuredValue,
};
use crate::providers::catalog::{CuratedModel, ModelOperation};

const LOOPBACK: &str = "http://127.0.0.1:8900/v1";
const REMOTE: &str = "https://decisions.example.com/v1";
const MODEL: &str = "decider-2026-09";

fn key(temp: &tempfile::TempDir) -> Arc<DecisionKey> {
    DecisionKey::load_or_create(temp.path()).expect("test: key")
}

fn request_with(
    key: &DecisionKey,
    state: serde_json::Value,
    expires_in: Duration,
    approval_pending: bool,
) -> DecisionRequest {
    let mut questions = BTreeMap::new();
    questions.insert(
        QuestionId::global(GlobalQuestion::EvidenceSufficient),
        DecisionQuestion::Noul {
            instructions: StructuredValue::try_from(serde_json::Value::from("Is the evidence sufficient?"))
                .expect("test: text"),
            criteria: None,
        },
    );
    DecisionRequest::seal(
        RequestParts {
            state: StructuredValue::try_from(state).expect("test: state"),
            questions,
            model: Some(MODEL.to_string()),
            state_fingerprint: "state".to_string(),
            candidate_set_hash: "candidates".to_string(),
            policy_version: Arc::from("policy"),
            catalog_generation: "catalog".to_string(),
            omitted_questions: Vec::new(),
            approval_pending,
        },
        key,
        Instant::now() + expires_in,
        &RequestLimits::default(),
    )
    .expect("test: seal")
}

fn request(key: &DecisionKey) -> DecisionRequest {
    request_with(
        key,
        serde_json::json!({"goal": "list files"}),
        Duration::from_secs(10),
        false,
    )
}

/// A valid answer that names `backend`, so a test can tell who served.
fn answer_as(request: &DecisionRequest, backend: &str) -> DecisionResponse {
    let mut response = BuiltinDecisionBackend::answer(request).expect("test: answer");
    response.backend = backend.to_string();
    response.resolved_model = MODEL.to_string();
    response
}

/// A scripted non-builtin backend with its own identity; it records the
/// bytes it was handed.
struct Scripted {
    mock: Arc<MockDecisionBackend>,
    identity: BackendIdentity,
    received: Received,
}

struct IndependentAccounted {
    identity: BackendIdentity,
}

struct DeadlineAccounting {
    minimum_attempt_deadline: Instant,
    owner_deadline: Instant,
}

struct DeadlineReceipt {
    owner_deadline: Instant,
}

impl crate::cost::decision_attempt::sealed::AccountingOwner for DeadlineAccounting {}
impl crate::cost::decision_attempt::sealed::AttemptReceipt for DeadlineReceipt {}

#[async_trait]
impl crate::cost::decision_attempt::DecisionAttemptAccounting for DeadlineAccounting {
    fn remaining_budget(&self) -> (u64, u64) {
        (u64::MAX, u64::MAX)
    }

    async fn begin(
        &self,
        attempt: crate::cost::decision_attempt::AttemptBinding<'_>,
    ) -> Result<Box<dyn crate::cost::decision_attempt::DecisionAttemptReceipt>, DecisionError> {
        if attempt.deadline() < self.minimum_attempt_deadline {
            return Err(DecisionError::BudgetExceeded);
        }
        Ok(Box::new(DeadlineReceipt {
            owner_deadline: self.owner_deadline,
        }))
    }
}

#[async_trait]
impl crate::cost::decision_attempt::DecisionAttemptReceipt for DeadlineReceipt {
    fn deadline(&self) -> Instant {
        self.owner_deadline
    }

    async fn settle(
        self: Box<Self>,
        _usage: crate::cost::decision_attempt::DecisionUsage,
    ) -> Result<(), DecisionError> {
        Ok(())
    }
}

#[async_trait]
impl DecisionBackend for IndependentAccounted {
    fn identity(&self) -> &BackendIdentity {
        &self.identity
    }

    fn locality(&self) -> BackendLocality {
        BackendLocality::Loopback
    }

    fn version_policy(&self) -> ModelVersionPolicy {
        ModelVersionPolicy::Unpinned
    }

    fn bounds_transport_independently(&self) -> bool {
        true
    }

    async fn evaluate(
        &self,
        _request: &DecisionRequest,
        _body: &GuardedBody,
        _deadline: Instant,
    ) -> Result<DecisionResponse, DecisionError> {
        Err(DecisionError::BudgetExceeded)
    }

    async fn evaluate_with_accounting(
        &self,
        request: &DecisionRequest,
        body: &GuardedBody,
        deadline: Instant,
        accounting: Option<&dyn crate::cost::decision_attempt::DecisionAttemptAccounting>,
    ) -> Result<DecisionResponse, DecisionError> {
        const TRANSPORT_BINDING: [u8; 32] = [7; 32];
        let receipt = accounting
            .ok_or(DecisionError::BudgetExceeded)?
            .begin(crate::cost::decision_attempt::AttemptBinding {
                backend: &self.identity,
                provider: "test",
                model: MODEL,
                transport_binding: &TRANSPORT_BINDING,
                guarded_body: body.bytes(),
                request_id: request.request_id(),
                deadline,
                remaining_attempts: 1,
            })
            .await?;
        let response = answer_as(request, "independent-accounted");
        receipt.settle(response.usage).await?;
        Ok(response)
    }
}

type Received = Arc<parking_lot::Mutex<Vec<Vec<u8>>>>;

#[async_trait]
impl DecisionBackend for Scripted {
    fn identity(&self) -> &BackendIdentity {
        &self.identity
    }

    fn locality(&self) -> BackendLocality {
        BackendLocality::Loopback
    }

    fn version_policy(&self) -> ModelVersionPolicy {
        ModelVersionPolicy::Unpinned
    }

    async fn evaluate(
        &self,
        request: &DecisionRequest,
        body: &GuardedBody,
        deadline: Instant,
    ) -> Result<DecisionResponse, DecisionError> {
        self.received.lock().push(body.bytes().to_vec());
        self.mock.evaluate(request, deadline).await
    }
}

fn recorded(
    key: &Arc<DecisionKey>,
    name: &str,
    script: Vec<MockStep>,
) -> (Box<dyn DecisionBackend>, Arc<MockDecisionBackend>, Received) {
    let mock = Arc::new(MockDecisionBackend::new(Arc::clone(key), script));
    let received = Received::default();
    let backend = Scripted {
        mock: Arc::clone(&mock),
        identity: BackendIdentity {
            kind: BackendKind::CompatibleHttp,
            provider: Some(ProviderId::of(&format!("custom:http://127.0.0.1:1/{name}"))),
            configured_model: Arc::from(MODEL),
        },
        received: Arc::clone(&received),
    };
    (Box::new(backend), mock, received)
}

#[tokio::test]
async fn unproven_budget_gate_excludes_configured_slot_before_wire_or_cache() {
    let key = crate::security::runtime_key::DecisionKey::from_worker_channel(
        &ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &[7; 32]),
        "test-workspace",
        "test-run",
        "test-generation",
    )
    .expect("test: key");
    let (backend, mock, received) = recorded(&key, "prx-budget-model", Vec::new());
    let factory = DecisionBackendFactory::from_slots(
        vec![backend],
        Arc::clone(&key),
        OutboundSecrets::default(),
        SlotLimits::default(),
    )
    .with_unproven_budget_gate();
    let status = factory.status();
    assert!(status.order.is_empty(), "expected an empty value");
    assert!(
        status
            .excluded
            .iter()
            .any(|(_, _, reason)| *reason == Exclusion::BudgetUnproven)
    );
    let result = factory
        .decide(&request(&key), &CausalLineage::default())
        .await
        .expect("local builtin fallback");
    assert_eq!(result.served_by, ServedBy::Builtin);
    assert_eq!(result.served_locality, BackendLocality::Builtin);
    assert_eq!(result.served_endpoint, None);
    assert_eq!(result.reason, Some(FallbackReason::BudgetUnproven));
    assert!(result.attempts.is_empty(), "expected an empty value");
    assert_eq!(mock.calls(), 0);
    assert!(received.lock().is_empty());
}

#[tokio::test]
async fn unproven_budget_gate_ignores_a_primed_decision_cache() {
    let key = crate::security::runtime_key::DecisionKey::from_worker_channel(
        &ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &[7; 32]),
        "test-workspace",
        "test-run",
        "test-generation",
    )
    .expect("test: key");
    let initial = request(&key);
    let (backend, mock) = scripted(
        &key,
        "prx-budget-model",
        vec![MockStep::now(MockOutcome::Respond(answer_as(
            &initial,
            "prx-budget-model",
        )))],
    );
    let factory = DecisionBackendFactory::from_slots(
        vec![backend],
        Arc::clone(&key),
        OutboundSecrets::default(),
        SlotLimits::default(),
    );
    let first = factory
        .decide(&initial, &CausalLineage::default())
        .await
        .expect("priming response");
    assert_eq!(first.served_by, ServedBy::Primary);
    assert_eq!(factory.status().cached, 1);
    let gated = factory.with_unproven_budget_gate();
    let repeated = request(&key);
    let result = gated
        .decide(&repeated, &CausalLineage::default())
        .await
        .expect("builtin bypasses primed cache");
    assert_eq!(result.served_by, ServedBy::Builtin);
    assert_eq!(result.reason, Some(FallbackReason::BudgetUnproven));
    assert_eq!(mock.calls(), 1, "the gated decision did not send");
}

fn scripted(
    key: &Arc<DecisionKey>,
    name: &str,
    script: Vec<MockStep>,
) -> (Box<dyn DecisionBackend>, Arc<MockDecisionBackend>) {
    let (backend, mock, _) = recorded(key, name, script);
    (backend, mock)
}

/// Transports that count builds and build a scripted backend answering
/// `script` (or refuse, when there is none).
struct CountingTransports {
    builds: AtomicUsize,
    script: parking_lot::Mutex<Option<Vec<MockStep>>>,
}

impl CountingTransports {
    fn refusing() -> Self {
        Self {
            builds: AtomicUsize::new(0),
            script: parking_lot::Mutex::new(None),
        }
    }

    fn answering(script: Vec<MockStep>) -> Self {
        Self {
            builds: AtomicUsize::new(0),
            script: parking_lot::Mutex::new(Some(script)),
        }
    }

    fn builds(&self) -> usize {
        self.builds.load(Ordering::SeqCst)
    }
}

impl DecisionTransports for CountingTransports {
    fn build(&self, spec: &BackendSpec<'_>) -> Result<Box<dyn DecisionBackend>, DecisionError> {
        self.builds.fetch_add(1, Ordering::SeqCst);
        let script = self.script.lock().take().ok_or(DecisionError::Unavailable)?;
        Ok(Box::new(Scripted {
            mock: Arc::new(MockDecisionBackend::new(Arc::clone(&spec.key), script)),
            identity: spec.identity.clone(),
            received: Received::default(),
        }))
    }
}

fn config(temp: &tempfile::TempDir, change: impl FnOnce(&mut Section)) -> Config {
    let mut config = Config::default();
    config.workspace_dir = temp.path().to_path_buf();
    config.config_path = temp.path().join("config.toml");
    config.api_key = Some("sk-ROOT-FACTORY-SENTINEL".to_string());
    let mut section = Section {
        enabled: true,
        ..Section::default()
    };
    change(&mut section);
    config.decision_engine.enabled = section.enabled;
    config.decision_engine.backend = Some(section.backend);
    config
}

fn catalog(config: &Config, provider: &str) -> CatalogView {
    CatalogView::from_config(config).with_curated([CuratedModel {
        operations: Some(vec![ModelOperation::DecisionEndpoint]),
        ..CuratedModel::new(ProviderId::of(provider), Some(MODEL))
    }])
}

fn build(
    config: &Config,
    catalog: &CatalogView,
    key: Option<Arc<DecisionKey>>,
    transports: &CountingTransports,
) -> Result<DecisionBackendFactory, FactoryBuildError> {
    DecisionBackendFactory::from_config(
        config,
        FactoryDeps {
            catalog,
            key,
            transports,
            config_generation: 7,
            events: Arc::new(crate::observability::causal_event::testing::RecordingSink::default()),
        },
    )
}

fn compatible(endpoint: &'static str, remote: bool) -> impl FnOnce(&mut Section) {
    move |section| {
        section.backend.kind = BackendSelection::Fixed(BackendKind::CompatibleHttp);
        section.backend.endpoint = endpoint.to_string();
        section.backend.model = MODEL.to_string();
        section.backend.remote_enabled = remote;
    }
}

/// The order table (SP-CDP-A 5.6 under D-040): pure, builtin last.
#[test]
fn the_backend_order_follows_the_table() {
    let order = |change: &dyn Fn(&mut Section), health: HealthView| {
        let mut section = Section::default();
        section.enabled = true;
        change(&mut section);
        DecisionBackendFactory::backend_order(&section.backend, section.enabled, &health)
            .into_iter()
            .map(|slot| (slot.kind, slot.locality, slot.exclusion))
            .collect::<Vec<_>>()
    };
    let builtin = (BackendKind::Builtin, BackendLocality::Builtin, None);
    let set = |kind: BackendSelection, endpoint: &'static str, remote: bool| {
        move |section: &mut Section| {
            section.backend.kind = kind;
            section.backend.endpoint = endpoint.to_string();
            section.backend.remote_enabled = remote;
        }
    };
    let healthy = HealthView::default();
    let compat = BackendSelection::Fixed(BackendKind::CompatibleHttp);
    assert_eq!(
        order(&set(BackendSelection::Fixed(BackendKind::Builtin), "", true), healthy),
        [builtin]
    );
    assert_eq!(
        order(&set(BackendSelection::Auto, "", true), healthy),
        [
            (
                BackendKind::CompatibleHttp,
                BackendLocality::Loopback,
                Some(Exclusion::NotConfigured)
            ),
            builtin
        ]
    );
    for kind in [BackendSelection::Auto, compat] {
        assert_eq!(
            order(&set(kind, LOOPBACK, false), healthy),
            [(BackendKind::CompatibleHttp, BackendLocality::Loopback, None), builtin],
            "loopback needs no remote switch"
        );
        assert_eq!(
            order(&set(kind, REMOTE, false), healthy),
            [
                (
                    BackendKind::CompatibleHttp,
                    BackendLocality::Remote,
                    Some(Exclusion::RemoteDisabled)
                ),
                builtin
            ]
        );
        assert_eq!(
            order(&set(kind, REMOTE, true), healthy),
            [(BackendKind::CompatibleHttp, BackendLocality::Remote, None), builtin]
        );
        assert_eq!(
            order(
                &set(kind, LOOPBACK, false),
                HealthView {
                    circuit_open: true,
                    key_unavailable: false,
                    unhealthy: false,
                }
            ),
            [
                (
                    BackendKind::CompatibleHttp,
                    BackendLocality::Loopback,
                    Some(Exclusion::CircuitOpen)
                ),
                builtin
            ]
        );
    }
    assert_eq!(
        order(
            &set(compat, LOOPBACK, false),
            HealthView {
                circuit_open: false,
                key_unavailable: true,
                unhealthy: false,
            }
        ),
        [
            (
                BackendKind::CompatibleHttp,
                BackendLocality::Loopback,
                Some(Exclusion::KeyUnavailable)
            ),
            builtin
        ],
        "`prx causal backends` shows an unavailable key"
    );
    for hosted in [BackendKind::TypeSafe, BackendKind::OpenRouter] {
        assert_eq!(
            order(&set(BackendSelection::Fixed(hosted), LOOPBACK, false), healthy),
            [
                (hosted, BackendLocality::Remote, Some(Exclusion::RemoteDisabled)),
                builtin
            ],
            "COORD-6: a hosted backend is remote even on a loopback override"
        );
        assert_eq!(
            order(&set(BackendSelection::Fixed(hosted), "", true), healthy),
            [(hosted, BackendLocality::Remote, None), builtin]
        );
    }
    let disabled = |section: &mut Section| {
        section.enabled = false;
        section.backend.kind = BackendSelection::Fixed(BackendKind::CompatibleHttp);
        section.backend.endpoint = LOOPBACK.to_string();
    };
    assert_eq!(
        order(&disabled, healthy),
        [
            (
                BackendKind::CompatibleHttp,
                BackendLocality::Loopback,
                Some(Exclusion::EngineDisabled)
            ),
            builtin
        ]
    );
}

/// `disabled_builds_builtin_only_order_and_no_transport` and V2.09
/// (`default_config_performs_no_decision_network_io`): with the plane
/// explicitly disabled, the default (engine switch off, R55), or
/// `auto` without an endpoint, no transport is built and the builtin serves.
#[tokio::test]
async fn disabled_builds_builtin_only_order_and_no_transport() {
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let key = key(&temp);
    let transports = CountingTransports::refusing();
    let mut default = Config::default();
    default.workspace_dir = temp.path().to_path_buf();
    let mut disabled = config(&temp, compatible(LOOPBACK, false));
    disabled.decision_engine.enabled = false;
    let auto_without_endpoint = config(&temp, |_| {});
    for (config, reason) in [
        // R55: the engine switch (default off) disables the factory.
        (&default, Some(FallbackReason::Disabled)),
        (&disabled, Some(FallbackReason::Disabled)),
        (&auto_without_endpoint, None),
    ] {
        let factory = build(
            config,
            &catalog(config, &format!("custom:{LOOPBACK}")),
            Some(Arc::clone(&key)),
            &transports,
        )
        .expect("test: build");
        let result = factory
            .decide(&request(&key), &CausalLineage::default())
            .await
            .expect("test: decide");
        assert_eq!(result.served_by, ServedBy::Builtin);
        assert_eq!(result.reason, reason);
        assert!(result.attempts.is_empty(), "expected an empty value");
        assert_eq!(result.served, result.builtin_shadow);
        assert!(factory.status().order.is_empty(), "expected an empty value");
    }
    assert_eq!(transports.builds(), 0, "no transport is ever constructed");
    let disabled_status = build(
        &disabled,
        &catalog(&disabled, &format!("custom:{LOOPBACK}")),
        Some(key),
        &transports,
    )
    .expect("test: build")
    .status();
    assert_eq!(
        disabled_status.excluded,
        [(
            BackendKind::CompatibleHttp,
            BackendLocality::Loopback,
            Exclusion::EngineDisabled
        )]
    );
}

/// V2.04 with D-074 and D-079 L-5: a hosted backend needs an
/// auth-profile credential, which this release cannot configure, so it
/// is refused at every build, with remote off or on and whatever root
/// key is configured. No transport is built. (A remote backend left out
/// while remote is off is shown with a compatible endpoint.)
#[tokio::test]
async fn hosted_backend_refused_when_remote_disabled_even_with_key() {
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let key = key(&temp);
    let transports = CountingTransports::refusing();
    for remote in [false, true] {
        let hosted = config(&temp, |section| {
            section.backend.kind = BackendSelection::Fixed(BackendKind::TypeSafe);
            section.backend.model = MODEL.to_string();
            section.backend.remote_enabled = remote;
        });
        let error = build(
            &hosted,
            &catalog(&hosted, "typesafe"),
            Some(Arc::clone(&key)),
            &transports,
        )
        .expect_err("test: refused");
        assert!(
            matches!(
                error,
                FactoryBuildError::Selection(ResolutionError::DecisionCredentialNotProfile { .. })
            ),
            "{error}"
        );
        assert!(!error.to_string().contains("sk-ROOT-FACTORY-SENTINEL"), "{error}");
    }
    assert_eq!(transports.builds(), 0);
}

/// Strict selection at build (D-040 OQ-A2): an undeclared model fails
/// the build, and so the generation; a declared loopback model is built
/// and serves as the primary; a remote endpoint without the remote
/// switch is left out and never built.
#[tokio::test]
async fn a_configured_backend_is_built_only_when_selectable_and_allowed() {
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let key = key(&temp);
    let loopback = config(&temp, compatible(LOOPBACK, false));
    let undeclared = CatalogView::from_config(&loopback);
    let refusing = CountingTransports::refusing();
    assert!(matches!(
        build(&loopback, &undeclared, Some(Arc::clone(&key)), &refusing),
        Err(FactoryBuildError::Selection(
            ResolutionError::OperationUnverified { .. }
        ))
    ));
    let remote_off = config(&temp, compatible(REMOTE, false));
    let factory = build(
        &remote_off,
        &catalog(&remote_off, &format!("custom:{REMOTE}")),
        Some(Arc::clone(&key)),
        &refusing,
    )
    .expect("test: build");
    assert_eq!(
        factory.status().excluded,
        [(
            BackendKind::CompatibleHttp,
            BackendLocality::Remote,
            Exclusion::RemoteDisabled
        )]
    );
    assert_eq!(refusing.builds(), 0);

    let request = request(&key);
    let transports = CountingTransports::answering(vec![MockStep::now(MockOutcome::Respond(answer_as(
        &request,
        "local-decider",
    )))]);
    let factory = build(
        &loopback,
        &catalog(&loopback, &format!("custom:{LOOPBACK}")),
        Some(Arc::clone(&key)),
        &transports,
    )
    .expect("test: build");
    assert_eq!(transports.builds(), 1);
    let status = factory.status();
    assert_eq!(status.order.len(), 1);
    assert_eq!(status.order[0].identity.kind, BackendKind::CompatibleHttp);
    assert_eq!(
        status.order[0].identity.provider,
        Some(ProviderId::of(&format!("custom:{LOOPBACK}")))
    );
    let result = factory
        .decide(&request, &CausalLineage::default())
        .await
        .expect("test: decide");
    assert_eq!(result.served_by, ServedBy::Primary);
    assert_eq!(result.served_locality, BackendLocality::Loopback);
    assert_eq!(result.served_endpoint.as_deref(), Some(LOOPBACK));
    let mut private_path_result = result.clone();
    private_path_result.served_endpoint = Some(Arc::from("http://127.0.0.1/prx_private_route"));
    assert!(!format!("{private_path_result:?}").contains("prx_private_route"));
    assert_eq!(result.attempts.len(), 1);
    assert_eq!(result.attempts[0].outcome, AttemptOutcome::Served);
    assert_eq!(result.fallback_order.last(), Some(&BuiltinDecisionBackend::identity()));
}

/// D-077: a backend whose health probe failed is left out of every
/// decision until a probe passes; the order shows it as unhealthy.
#[tokio::test]
async fn an_unhealthy_backend_is_left_out_until_a_probe_passes() {
    struct Sick {
        identity: BackendIdentity,
        healthy: Arc<std::sync::atomic::AtomicBool>,
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl DecisionBackend for Sick {
        fn identity(&self) -> &BackendIdentity {
            &self.identity
        }
        fn locality(&self) -> BackendLocality {
            BackendLocality::Loopback
        }
        fn version_policy(&self) -> ModelVersionPolicy {
            ModelVersionPolicy::Unpinned
        }
        async fn evaluate(
            &self,
            _: &DecisionRequest,
            _: &GuardedBody,
            _: Instant,
        ) -> Result<DecisionResponse, DecisionError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(DecisionError::Unavailable)
        }
        async fn health(&self, _: &DecisionRequest, _: &GuardedBody, _: Instant) -> Result<(), DecisionError> {
            if self.healthy.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(DecisionError::Unavailable)
            }
        }
    }
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let key = key(&temp);
    let healthy = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let sick = Sick {
        identity: BackendIdentity {
            configured_model: Arc::from(MODEL),
            ..BuiltinDecisionBackend::identity()
        },
        healthy: Arc::clone(&healthy),
        calls: Arc::clone(&calls),
    };
    let factory = DecisionBackendFactory::from_slots(
        vec![Box::new(sick)],
        Arc::clone(&key),
        OutboundSecrets::default(),
        SlotLimits::default(),
    );
    factory.check_health().await;
    assert!(!factory.status().order[0].healthy);
    let result = factory
        .decide(&request(&key), &CausalLineage::default())
        .await
        .expect("test: decide");
    assert_eq!(
        result.attempts[0].outcome,
        AttemptOutcome::Skipped(DecisionError::Unavailable)
    );
    assert_eq!(result.served_by, ServedBy::Builtin);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "an unhealthy backend is not called");
    healthy.store(true, Ordering::SeqCst);
    factory.check_health().await;
    assert!(factory.status().order[0].healthy);
    factory
        .decide(&request(&key), &CausalLineage::default())
        .await
        .expect("test: decide");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let mut section = Section::default();
    section.enabled = true;
    section.backend.kind = BackendSelection::Fixed(BackendKind::CompatibleHttp);
    section.backend.endpoint = LOOPBACK.to_string();
    let order = DecisionBackendFactory::backend_order(
        &section.backend,
        section.enabled,
        &HealthView {
            circuit_open: false,
            key_unavailable: false,
            unhealthy: true,
        },
    );
    assert_eq!(order[0].exclusion, Some(Exclusion::Unhealthy));
}

/// D-057 L-1: configuration can never select the scripted test backend.
#[test]
fn configuration_can_never_select_the_mock() {
    assert!(BackendSelection::try_from("mock".to_string()).is_err());
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let config = config(&temp, |section| {
        section.backend.kind = BackendSelection::Fixed(BackendKind::Mock)
    });
    let transports = CountingTransports::refusing();
    assert!(matches!(
        build(
            &config,
            &CatalogView::from_config(&config),
            Some(key(&temp)),
            &transports
        ),
        Err(FactoryBuildError::MockNotConfigurable)
    ));
    assert_eq!(transports.builds(), 0);
}

/// D-057 N-5: an unavailable decision key leaves only the builtin; the
/// configured backend is left out and never built.
#[tokio::test]
async fn an_unavailable_key_falls_back_to_the_builtin() {
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let loopback = config(&temp, compatible(LOOPBACK, false));
    let transports = CountingTransports::refusing();
    let factory = build(
        &loopback,
        &catalog(&loopback, &format!("custom:{LOOPBACK}")),
        None,
        &transports,
    )
    .expect("test: build");
    assert_eq!(
        factory.status().excluded,
        [(
            BackendKind::CompatibleHttp,
            BackendLocality::Loopback,
            Exclusion::KeyUnavailable
        )]
    );
    assert_eq!(transports.builds(), 0);
    let other = tempfile::TempDir::new().expect("test: temp dir");
    let result = factory
        .decide(&request(&key(&other)), &CausalLineage::default())
        .await
        .expect("test: decide");
    assert_eq!(result.served_by, ServedBy::Builtin);
}

/// D-079 L-5: a disabled plane still validates its configured backend,
/// so enabling it later cannot fail the generation; nothing is built.
#[test]
fn a_disabled_plane_still_validates_its_backend() {
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let mut disabled = config(&temp, compatible(LOOPBACK, false));
    disabled.decision_engine.enabled = false;
    let transports = CountingTransports::refusing();
    assert!(matches!(
        build(&disabled, &CatalogView::from_config(&disabled), None, &transports),
        Err(FactoryBuildError::Selection(
            ResolutionError::OperationUnverified { .. }
        ))
    ));
    assert_eq!(transports.builds(), 0);
}

/// Every path under `dir`, sorted.
fn listing(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in std::fs::read_dir(&next).expect("test: read dir").filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path.clone());
            }
            found.push(path);
        }
    }
    found.sort();
    found
}

/// D-082 N-9: a disabled plane never opens the credential store. A store
/// another process holds neither delays nor fails the build or the
/// configuration check, and no file appears; the configuration-only
/// rules still hold, and enabled, the same configuration resolves its
/// credential.
#[test]
fn a_disabled_plane_never_opens_the_credential_store() {
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let mut disabled = config(&temp, compatible(LOOPBACK, false));
    disabled.decision_engine.enabled = false;
    disabled
        .decision_engine
        .backend
        .get_or_insert_with(Default::default)
        .credential_ref = CredentialRefSetting::Profile("cred-decision-held".to_string());
    // Declared in the configuration, which the configuration check reads.
    disabled.providers.models.push(
        serde_json::from_value(serde_json::json!({
            "provider": format!("custom:{LOOPBACK}"),
            "model": MODEL,
            "operations": ["decision_endpoint"],
        }))
        .expect("test: declaration"),
    );
    let lock = temp.path().join("auth-profiles.lock");
    std::fs::write(&lock, "pid=1\n").expect("test: held lock");
    let held_key = key(&temp);
    let before = listing(temp.path());
    let transports = CountingTransports::refusing();
    let declared = catalog(&disabled, &format!("custom:{LOOPBACK}"));
    let started = Instant::now();
    build(&disabled, &declared, None, &transports).expect("test: a disabled plane builds");
    assert_eq!(
        crate::config::manage::validate::check_decision_plane(None, &disabled),
        Ok(None)
    );
    let recovery_only = disabled.clone();
    assert!(!recovery_only.decision_engine.enabled);
    build(&recovery_only, &declared, Some(held_key), &transports)
        .expect("test: engine-off recovery cannot resolve a stored loopback credential");
    assert_eq!(
        crate::config::manage::validate::check_decision_plane(None, &recovery_only),
        Ok(None)
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the held store was never waited for"
    );
    assert_eq!(listing(temp.path()), before, "no file was created");
    assert_eq!(transports.builds(), 0);
    // The configuration-only rules still hold while disabled: an
    // undeclared model is refused.
    let mut undeclared = disabled.clone();
    undeclared.providers.models.clear();
    assert!(matches!(
        build(&undeclared, &CatalogView::from_config(&undeclared), None, &transports),
        Err(FactoryBuildError::Selection(
            ResolutionError::OperationUnverified { .. }
        ))
    ));
    // Enabled, the credential is resolved: a free store without it
    // refuses the reference.
    std::fs::remove_file(&lock).expect("test: release");
    let mut enabled = disabled;
    enabled.decision_engine.enabled = true;
    assert!(matches!(
        build(&enabled, &declared, Some(key(&temp)), &transports),
        Err(FactoryBuildError::Selection(ResolutionError::CredentialMissing { .. }))
    ));
}

/// V2.07 (and D-079 L-13): breakers are per backend. The first
/// backend's failures open its breaker only; the second keeps serving;
/// after the recovery interval one probe goes to the first again, and it
/// serves.
#[tokio::test]
async fn circuit_opens_per_backend_not_globally() {
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let key = key(&temp);
    let request = request_with(
        &key,
        serde_json::json!({"goal": "list files"}),
        Duration::from_secs(10),
        true,
    );
    let (first, first_mock) = scripted(
        &key,
        "a",
        vec![
            MockStep::now(MockOutcome::Fail(DecisionError::Transport)),
            MockStep::now(MockOutcome::Fail(DecisionError::Transport)),
            MockStep::now(MockOutcome::Respond(answer_as(&request, "a"))),
        ],
    );
    let (second, second_mock) = scripted(
        &key,
        "b",
        vec![MockStep::now(MockOutcome::Respond(answer_as(&request, "b"))); 3],
    );
    let limits = SlotLimits {
        failure_threshold: 2,
        cooldown: Duration::from_millis(100),
        ..SlotLimits::default()
    };
    let factory = DecisionBackendFactory::from_slots(
        vec![first, second],
        Arc::clone(&key),
        OutboundSecrets::default(),
        limits,
    );
    for _ in 0..2 {
        assert_eq!(
            factory
                .decide(&request, &CausalLineage::default())
                .await
                .expect("test: decide")
                .served_by,
            ServedBy::Fallback
        );
    }
    assert!(matches!(factory.status().order[0].breaker, BreakerState::Open { .. }));
    assert_eq!(
        factory.status().order[1].breaker,
        BreakerState::Closed { consecutive: 0 }
    );
    let result = factory
        .decide(&request, &CausalLineage::default())
        .await
        .expect("test: decide");
    assert_eq!(
        result.attempts[0].outcome,
        AttemptOutcome::Skipped(DecisionError::CircuitOpen)
    );
    assert_eq!(result.served_by, ServedBy::Fallback);
    assert_eq!(first_mock.calls(), 2, "an open breaker is not called");
    tokio::time::sleep(Duration::from_millis(120)).await;
    let result = factory
        .decide(&request, &CausalLineage::default())
        .await
        .expect("test: decide");
    assert_eq!(result.served_by, ServedBy::Primary, "the recovery probe served");
    assert_eq!(first_mock.calls(), 3);
    assert_eq!(second_mock.calls(), 3);
    assert_eq!(
        factory.status().order[0].breaker,
        BreakerState::Closed { consecutive: 0 }
    );
}

/// V2.08 (D-079 L-12, deterministic): a fallback is attempted only
/// while the remaining deadline allows one. The first backend hangs for
/// its whole slot timeout (180 ms) inside a 200 ms deadline, leaving less
/// than the second backend's minimum attempt (45 ms); the builtin serves.
#[tokio::test]
async fn fallback_only_if_remaining_deadline_allows() {
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let key = key(&temp);
    let request = request_with(
        &key,
        serde_json::json!({"goal": "list files"}),
        Duration::from_millis(200),
        false,
    );
    let (first, _) = scripted(&key, "a", vec![MockStep::now(MockOutcome::Hang)]);
    let (second, second_mock) = scripted(
        &key,
        "b",
        vec![MockStep::now(MockOutcome::Respond(answer_as(&request, "b")))],
    );
    let limits = SlotLimits {
        timeout: Duration::from_millis(180),
        ..SlotLimits::default()
    };
    let factory = DecisionBackendFactory::from_slots(
        vec![first, second],
        Arc::clone(&key),
        OutboundSecrets::default(),
        limits,
    );
    let started = Instant::now();
    let result = factory
        .decide(&request, &CausalLineage::default())
        .await
        .expect("test: decide");
    assert!(started.elapsed() < Duration::from_secs(2), "one total deadline");
    assert_eq!(
        result.attempts[0].outcome,
        AttemptOutcome::Failed(DecisionError::Timeout)
    );
    assert_eq!(result.reason, Some(FallbackReason::DeadlineExhausted));
    assert_eq!(result.served_by, ServedBy::Builtin);
    assert_eq!(second_mock.calls(), 0);
}

#[tokio::test]
async fn independently_bounded_transport_does_not_charge_accounting_to_its_io_timeout() {
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let key = key(&temp);
    let request = request_with(
        &key,
        serde_json::json!({"goal": "list files"}),
        Duration::from_secs(60),
        false,
    );
    let owner_deadline = request.manifest().expires_at();
    let accounting = DeadlineAccounting {
        minimum_attempt_deadline: Instant::now() + Duration::from_secs(30),
        owner_deadline,
    };
    let backend = IndependentAccounted {
        identity: BackendIdentity {
            kind: BackendKind::CompatibleHttp,
            provider: Some(ProviderId::of("custom:http://127.0.0.1:1/accounted")),
            configured_model: Arc::from(MODEL),
        },
    };
    let factory = DecisionBackendFactory::from_slots(
        vec![Box::new(backend)],
        Arc::clone(&key),
        OutboundSecrets::default(),
        SlotLimits {
            timeout: Duration::from_millis(50),
            ..SlotLimits::default()
        },
    );

    let result = factory
        .decide_with_accounting(&request, &CausalLineage::default(), Some(&accounting))
        .await
        .expect("accounted configured decision");

    assert_eq!(result.served_by, ServedBy::Primary);
    assert!(matches!(result.attempts[0].outcome, AttemptOutcome::Served));
}

/// D-079 L-6: a timeout forced by the turn's deadline is not the
/// backend's failure: its breaker does not count it. The 300 ms slot
/// timeout outlasts the 100 ms turn deadline, which bounds the attempt
/// (the minimum attempt, 75 ms, still lets it start).
#[tokio::test]
async fn a_turn_deadline_timeout_is_not_counted_against_the_backend() {
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let key = key(&temp);
    let request = request_with(
        &key,
        serde_json::json!({"goal": "list files"}),
        Duration::from_millis(100),
        false,
    );
    let (first, _) = scripted(&key, "a", vec![MockStep::now(MockOutcome::Hang)]);
    let limits = SlotLimits {
        timeout: Duration::from_millis(300),
        failure_threshold: 1,
        ..SlotLimits::default()
    };
    let factory = DecisionBackendFactory::from_slots(vec![first], Arc::clone(&key), OutboundSecrets::default(), limits);
    let result = factory
        .decide(&request, &CausalLineage::default())
        .await
        .expect("test: decide");
    assert_eq!(
        result.attempts[0].outcome,
        AttemptOutcome::Failed(DecisionError::Timeout)
    );
    assert_eq!(result.reason, Some(FallbackReason::DeadlineExhausted));
    assert_eq!(
        factory.status().order[0].breaker,
        BreakerState::Closed { consecutive: 0 }
    );
}

/// D-079 M-3: a backend is handed exactly the bytes the outbound guard
/// checked, which are the System One encoding of the request.
#[tokio::test]
async fn a_backend_sends_exactly_the_guarded_bytes() {
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let key = key(&temp);
    let request = request(&key);
    let (first, _, received) = recorded(
        &key,
        "a",
        vec![MockStep::now(MockOutcome::Respond(answer_as(&request, "a")))],
    );
    let factory = DecisionBackendFactory::from_slots(
        vec![first],
        Arc::clone(&key),
        OutboundSecrets::default(),
        SlotLimits::default(),
    );
    assert_eq!(
        factory
            .decide(&request, &CausalLineage::default())
            .await
            .expect("test: decide")
            .served_by,
        ServedBy::Primary
    );
    let expected = SystemOneCodec::new(&request, &key)
        .and_then(|codec| codec.encode_request())
        .expect("test: encode");
    assert_eq!(received.lock().as_slice(), [expected]);
}

/// CDP 17: an invalid response is void; the next backend serves, and the
/// request (state and candidates) is unchanged.
#[tokio::test]
async fn invalid_response_falls_to_the_next_backend_and_never_changes_candidates() {
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let key = key(&temp);
    let request = request(&key);
    let state_before = request.state().as_json().clone();
    let mut invalid = answer_as(&request, "a");
    invalid.answers.clear();
    let (first, _) = scripted(&key, "a", vec![MockStep::now(MockOutcome::Respond(invalid))]);
    let (second, _) = scripted(
        &key,
        "b",
        vec![MockStep::now(MockOutcome::Respond(answer_as(&request, "b")))],
    );
    let factory = DecisionBackendFactory::from_slots(
        vec![first, second],
        Arc::clone(&key),
        OutboundSecrets::default(),
        SlotLimits::default(),
    );
    let result = factory
        .decide(&request, &CausalLineage::default())
        .await
        .expect("test: decide");
    assert_eq!(
        result.attempts[0].outcome,
        AttemptOutcome::Failed(DecisionError::InvalidResponse(ValidationRule::AnswerMissing))
    );
    assert_eq!(result.served_by, ServedBy::Fallback);
    assert_eq!(result.served.claimed_backend(), "b");
    let question = QuestionId::global(GlobalQuestion::EvidenceSufficient);
    assert_eq!(result.served.answer(&question), result.builtin_shadow.answer(&question));
    assert_eq!(request.state().as_json(), &state_before);
}

/// Invariant 16.2: a request whose state carries a configured secret is
/// never handed to a non-builtin backend.
/// D-086 F-3: a refused request and a breaker opening reach the log at
/// WARN whatever the observer backend is, with the backend, class, rule
/// and duration only: never the refused value.
#[tokio::test]
async fn refusals_and_breaker_openings_are_logged_at_warn() {
    #[derive(Clone, Default)]
    struct Sink(Arc<parking_lot::Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }
    let sink = Sink::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(sink.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let key = key(&temp);
    // The callsite interest cache is global: a rebuild racing on another
    // thread can leave a callsite disabled after ours, so each run below
    // is retried, after a rebuild, until its warning is captured.
    let captured = |needle: &str| String::from_utf8_lossy(&sink.0.lock()).contains(needle);
    let mut live = Config::default();
    live.api_key = Some("sk-WARN-LOG-SENTINEL".to_string());
    let secret = request_with(
        &key,
        serde_json::json!({"goal": "use sk-WARN-LOG-SENTINEL please"}),
        Duration::from_secs(10),
        false,
    );
    for _ in 0..20 {
        tracing::callsite::rebuild_interest_cache();
        let (first, _) = scripted(&key, "a", Vec::new());
        let factory = DecisionBackendFactory::from_slots(
            vec![first],
            Arc::clone(&key),
            OutboundSecrets::of_config(&live),
            SlotLimits::default(),
        );
        factory
            .decide(&secret, &CausalLineage::default())
            .await
            .expect("test: decide");
        if captured("decision request refused before it was sent") {
            break;
        }
    }

    let plain = request(&key);
    for _ in 0..20 {
        tracing::callsite::rebuild_interest_cache();
        let (failing, _) = scripted(
            &key,
            "a",
            vec![MockStep::now(MockOutcome::Fail(DecisionError::Timeout))],
        );
        let factory = DecisionBackendFactory::from_slots(
            vec![failing],
            Arc::clone(&key),
            OutboundSecrets::default(),
            SlotLimits {
                failure_threshold: 1,
                ..SlotLimits::default()
            },
        );
        factory
            .decide(&plain, &CausalLineage::default())
            .await
            .expect("test: decide");
        if captured("decision backend circuit opened") {
            break;
        }
    }

    let logs = String::from_utf8_lossy(&sink.0.lock()).to_string();
    assert!(
        logs.contains("decision request refused before it was sent")
            && logs.contains("class=\"policy_rejection\"")
            && logs.contains("rule=\"secret_in_request\""),
        "{logs}"
    );
    assert!(
        logs.contains("decision backend circuit opened") && logs.contains("open_for_ms="),
        "{logs}"
    );
    assert!(logs.contains("WARN"), "{logs}");
    assert!(!logs.contains("WARN-LOG-SENTINEL"), "{logs}");
}

#[tokio::test]
async fn a_request_carrying_a_registered_secret_is_never_sent() {
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let key = key(&temp);
    let mut live = Config::default();
    live.api_key = Some("sk-OUTBOUND-STATE-SENTINEL".to_string());
    let request = request_with(
        &key,
        serde_json::json!({"goal": "use sk-OUTBOUND-STATE-SENTINEL please"}),
        Duration::from_secs(10),
        false,
    );
    let (first, mock) = scripted(
        &key,
        "a",
        vec![MockStep::now(MockOutcome::Respond(answer_as(&request, "a")))],
    );
    let factory = DecisionBackendFactory::from_slots(
        vec![first],
        Arc::clone(&key),
        OutboundSecrets::of_config(&live),
        SlotLimits::default(),
    );
    let result = factory
        .decide(&request, &CausalLineage::default())
        .await
        .expect("test: decide");
    assert_eq!(
        result.attempts[0].outcome,
        AttemptOutcome::Skipped(DecisionError::PolicyRejection(PolicyRule::SecretInRequest))
    );
    assert_eq!(mock.calls(), 0, "nothing was sent");
    assert_eq!(result.served_by, ServedBy::Builtin);
    assert_eq!(result.reason, Some(FallbackReason::AllFailed));
}

/// V7.06: a repeated cacheable request is served from the cache; a
/// different request, a pending approval or another generation misses.
#[tokio::test]
async fn decision_cache_hit_and_invalidation() {
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let key = key(&temp);
    let request = request(&key);
    let other = request_with(
        &key,
        serde_json::json!({"goal": "read a file"}),
        Duration::from_secs(10),
        false,
    );
    let pending = request_with(
        &key,
        serde_json::json!({"goal": "list files"}),
        Duration::from_secs(10),
        true,
    );
    let (first, mock) = scripted(
        &key,
        "a",
        vec![
            MockStep::now(MockOutcome::Respond(answer_as(&request, "a"))),
            MockStep::now(MockOutcome::Respond(answer_as(&other, "a"))),
            MockStep::now(MockOutcome::Respond(answer_as(&pending, "a"))),
            MockStep::now(MockOutcome::Respond(answer_as(&pending, "a"))),
        ],
    );
    let factory = DecisionBackendFactory::from_slots(
        vec![first],
        Arc::clone(&key),
        OutboundSecrets::default(),
        SlotLimits::default(),
    );
    assert_eq!(
        factory
            .decide(&request, &CausalLineage::default())
            .await
            .expect("test: first")
            .served_by,
        ServedBy::Primary
    );
    let again = request_with(
        &key,
        serde_json::json!({"goal": "list files"}),
        Duration::from_secs(10),
        false,
    );
    assert_ne!(again.request_id(), request.request_id());
    let cached = factory
        .decide(&again, &CausalLineage::default())
        .await
        .expect("test: again");
    assert_eq!(cached.served_by, ServedBy::Cache);
    assert_eq!(cached.served_locality, BackendLocality::Loopback);
    assert_eq!(cached.served_endpoint, None, "test slots have no configured endpoint");
    // D-079 L-9: a cache hit carries the current request's id and no
    // usage of its own.
    assert_eq!(cached.served.request_id(), again.request_id());
    assert_eq!(
        cached.served.usage(),
        &crate::cost::decision_attempt::DecisionUsage::default()
    );
    assert_eq!(cached.served.provider_request_id(), None);
    assert_eq!(mock.calls(), 1);
    assert_eq!(
        factory
            .decide(&other, &CausalLineage::default())
            .await
            .expect("test: other")
            .served_by,
        ServedBy::Primary
    );
    for _ in 0..2 {
        assert_eq!(
            factory
                .decide(&pending, &CausalLineage::default())
                .await
                .expect("test: pending")
                .served_by,
            ServedBy::Primary
        );
    }
    assert_eq!(mock.calls(), 4, "a pending approval is never cached");
    assert_eq!(factory.status().cached, 2);

    let identity = BuiltinDecisionBackend::identity();
    let base = CacheKey::new(&request, &identity, 1);
    assert_ne!(base, CacheKey::new(&request, &identity, 2), "generation");
    assert_ne!(base, CacheKey::new(&other, &identity, 1), "request fingerprint");
    let mut model = identity.clone();
    model.configured_model = Arc::from("other-model");
    assert_ne!(base, CacheKey::new(&request, &model, 1), "configured model");
    let mut kind = identity;
    kind.kind = BackendKind::CompatibleHttp;
    assert_ne!(base, CacheKey::new(&request, &kind, 1), "backend kind");
}

#[test]
fn cache_entries_expire_and_the_least_recent_is_evicted() {
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let key = key(&temp);
    let request = request(&key);
    let decision = builtin_shadow(&request).expect("test: shadow");
    let cache = DecisionCache::new();
    let now = Instant::now();
    let identity = BuiltinDecisionBackend::identity();
    let first = CacheKey::new(&request, &identity, 0);
    cache.put(first, decision.clone(), now);
    assert!(
        cache
            .get(&first, now + crate::decision_engine::prediction::cache::CACHE_TTL / 2)
            .is_some()
    );
    assert!(
        cache
            .get(&first, now + crate::decision_engine::prediction::cache::CACHE_TTL)
            .is_none(),
        "expired"
    );
    assert!(cache.is_empty(), "an expired entry is removed");
    for generation in 0..=crate::decision_engine::prediction::cache::CACHE_CAPACITY as u64 {
        cache.put(CacheKey::new(&request, &identity, generation), decision.clone(), now);
    }
    assert_eq!(cache.len(), crate::decision_engine::prediction::cache::CACHE_CAPACITY);
    assert!(
        cache.get(&CacheKey::new(&request, &identity, 0), now).is_none(),
        "the oldest was evicted"
    );
}

/// Cancel-safety: dropping a decision mid-attempt records nothing as
/// served and leaves the factory usable.
#[tokio::test]
async fn a_dropped_decision_records_nothing() {
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let key = key(&temp);
    let request = request(&key);
    let (first, mock) = scripted(
        &key,
        "a",
        vec![
            MockStep::now(MockOutcome::Hang),
            MockStep::now(MockOutcome::Respond(answer_as(&request, "a"))),
        ],
    );
    let factory = DecisionBackendFactory::from_slots(
        vec![first],
        Arc::clone(&key),
        OutboundSecrets::default(),
        SlotLimits::default(),
    );
    assert!(
        tokio::time::timeout(
            Duration::from_millis(20),
            factory.decide(&request, &CausalLineage::default())
        )
        .await
        .is_err()
    );
    assert_eq!(factory.status().cached, 0);
    assert_eq!(
        factory.status().order[0].breaker,
        BreakerState::Closed { consecutive: 0 }
    );
    let result = factory
        .decide(&request, &CausalLineage::default())
        .await
        .expect("test: decide");
    assert_eq!(result.served_by, ServedBy::Primary);
    assert_eq!(mock.calls(), 2);
}

/// D-079 M-1 (and L-13): a half-open probe that is cancelled (the
/// decision dropped) does not wedge the breaker; the next decision
/// probes the backend again and it serves.
#[tokio::test]
async fn a_cancelled_half_open_probe_does_not_wedge_the_breaker() {
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let key = key(&temp);
    let request = request(&key);
    let mut loopback = config(&temp, compatible(LOOPBACK, false));
    loopback
        .decision_engine
        .backend
        .get_or_insert_with(Default::default)
        .circuit_breaker
        .failure_threshold = 1;
    loopback
        .decision_engine
        .backend
        .get_or_insert_with(Default::default)
        .circuit_breaker
        .cooldown_secs = 1;
    let transports = CountingTransports::answering(vec![
        MockStep::now(MockOutcome::Fail(DecisionError::Transport)),
        MockStep::now(MockOutcome::Hang),
        MockStep::now(MockOutcome::Respond(answer_as(&request, "a"))),
    ]);
    let factory = build(
        &loopback,
        &catalog(&loopback, &format!("custom:{LOOPBACK}")),
        Some(Arc::clone(&key)),
        &transports,
    )
    .expect("test: build");
    assert_eq!(
        factory
            .decide(&request, &CausalLineage::default())
            .await
            .expect("test: first")
            .served_by,
        ServedBy::Builtin
    );
    assert!(matches!(factory.status().order[0].breaker, BreakerState::Open { .. }));
    tokio::time::sleep(Duration::from_millis(1050)).await;
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            factory.decide(&request, &CausalLineage::default())
        )
        .await
        .is_err(),
        "the probe was cancelled"
    );
    let result = factory
        .decide(&request, &CausalLineage::default())
        .await
        .expect("test: after the cancelled probe");
    assert_eq!(result.served_by, ServedBy::Primary, "{:?}", result.attempts);
}

fn lineage() -> CausalLineage {
    CausalLineage {
        workspace_id: Arc::from("workspace-1"),
        session_key: Some(Arc::from("SESSION-SENTINEL-8f3")),
        run_id: Some(Arc::from("run-1")),
        ..CausalLineage::default()
    }
}

/// V8.03: every event of a decision carries its lineage, and the
/// sequence says what happened: A started, its breaker opened, it
/// failed and fell back to B; B started and served as the fallback.
#[tokio::test]
async fn every_causal_event_emitted_with_lineage() {
    use crate::observability::causal_event::{CausalEventKind as Kind, CircuitState, ServedLabel};
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let key = key(&temp);
    let request = request(&key);
    let (first, _) = scripted(
        &key,
        "a",
        vec![MockStep::now(MockOutcome::Fail(DecisionError::Transport))],
    );
    let (second, _) = scripted(
        &key,
        "b",
        vec![MockStep::now(MockOutcome::Respond(answer_as(&request, "b")))],
    );
    let sink = Arc::new(crate::observability::causal_event::testing::RecordingSink::default());
    let limits = SlotLimits {
        failure_threshold: 1,
        ..SlotLimits::default()
    };
    let factory = DecisionBackendFactory::from_slots(
        vec![first, second],
        Arc::clone(&key),
        OutboundSecrets::default(),
        limits,
    )
    .with_events(Arc::clone(&sink) as Arc<dyn CausalEventSink>);
    let result = factory.decide(&request, &lineage()).await.expect("test: decide");
    assert_eq!(result.served_by, ServedBy::Fallback);
    assert_eq!(result.served_locality, BackendLocality::Loopback);
    assert_eq!(result.served_endpoint, None);
    let events = std::mem::take(&mut *sink.events.lock());
    let kinds: Vec<Kind> = events.iter().map(|event| event.kind).collect();
    assert_eq!(
        kinds,
        [
            Kind::PredictionStarted,
            Kind::BackendCircuitOpened,
            Kind::PredictionFailed,
            Kind::BackendFallback,
            Kind::PredictionStarted,
            Kind::PredictionSucceeded,
        ]
    );
    for event in &events {
        assert_eq!(&*event.lineage.workspace_id, "workspace-1");
        assert_eq!(event.lineage.session_key.as_deref(), Some("SESSION-SENTINEL-8f3"));
        assert_eq!(event.lineage.decision_id.as_deref(), Some(request.request_id()));
        assert_eq!(event.fields.backend, Some(BackendKind::CompatibleHttp));
    }
    assert_eq!(events[1].fields.circuit, Some(CircuitState::Open));
    assert_eq!(events[2].fields.error_class, Some("transport"));
    assert_eq!(events[3].fields.to_backend, Some(BackendKind::CompatibleHttp));
    assert_eq!(events[5].fields.served, Some(ServedLabel::Fallback));

    // The builtin serving is an event too.
    let sink = Arc::new(crate::observability::causal_event::testing::RecordingSink::default());
    let (only, _) = scripted(
        &key,
        "a",
        vec![MockStep::now(MockOutcome::Fail(DecisionError::Unavailable))],
    );
    let factory = DecisionBackendFactory::from_slots(
        vec![only],
        Arc::clone(&key),
        OutboundSecrets::default(),
        SlotLimits::default(),
    )
    .with_events(Arc::clone(&sink) as Arc<dyn CausalEventSink>);
    factory.decide(&request, &lineage()).await.expect("test: decide");
    let events = sink.events.lock();
    let last = events.last().expect("test: events");
    assert_eq!(last.kind, Kind::PredictionSucceeded);
    assert_eq!(last.fields.served, Some(ServedLabel::Builtin));
    assert_eq!(events[events.len() - 2].fields.to_backend, Some(BackendKind::Builtin));
}

/// SP-CDP-A 5.11 `metric_labels_are_bounded_allowlist`: after a decision
/// run with a sentinel session, user text in the state and a hostile
/// model claimed by the backend, the Prometheus text holds none of them,
/// and the decision metrics hold bounded labels only.
#[tokio::test]
async fn metric_labels_are_bounded_allowlist() {
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let key = key(&temp);
    let request = request_with(
        &key,
        serde_json::json!({"goal": "USER-TEXT-SENTINEL please list files"}),
        Duration::from_secs(10),
        false,
    );
    let mut hostile = answer_as(&request, "a");
    hostile.resolved_model = "HOSTILE-CLAIMED-MODEL".to_string();
    let (first, _) = scripted(&key, "a", vec![MockStep::now(MockOutcome::Respond(hostile))]);
    let observer = Arc::new(crate::observability::PrometheusObserver::try_new().expect("test: observer"));
    let sink = crate::observability::causal_event::ObserverCausalSink::new(
        Arc::clone(&observer) as Arc<dyn crate::observability::Observer>
    );
    let factory = DecisionBackendFactory::from_slots(
        vec![first],
        Arc::clone(&key),
        OutboundSecrets::default(),
        SlotLimits::default(),
    )
    .with_events(Arc::new(sink));
    assert_eq!(
        factory
            .decide(&request, &lineage())
            .await
            .expect("test: decide")
            .served_by,
        ServedBy::Primary
    );
    let text = observer.encode();
    assert!(
        text.contains(
            r#"prx_causal_decisions_total{backend="compatible_http",locality="loopback",served_by="primary"} 1"#
        ),
        "{text}"
    );
    for sentinel in [
        "SESSION-SENTINEL",
        "USER-TEXT-SENTINEL",
        "HOSTILE-CLAIMED-MODEL",
        "workspace-1",
        "run-1",
        request.request_id(),
    ] {
        assert!(!text.contains(sentinel), "{sentinel} reached a metric: {text}");
    }
}

/// AG-12: a decision error and the factory's status never show URL user
/// information, even for a provider label that carries it.
#[tokio::test]
async fn decision_errors_and_status_carry_no_userinfo() {
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let key = key(&temp);
    let mock = Arc::new(MockDecisionBackend::new(Arc::clone(&key), Vec::new()));
    let leaky = Scripted {
        mock,
        identity: BackendIdentity {
            kind: BackendKind::CompatibleHttp,
            provider: Some(ProviderId::of("custom:http://decider:USERINFO-SENTINEL@127.0.0.1:1/v1")),
            configured_model: Arc::from(MODEL),
        },
        received: Received::default(),
    };
    let factory = DecisionBackendFactory::from_slots(
        vec![Box::new(leaky)],
        Arc::clone(&key),
        OutboundSecrets::default(),
        SlotLimits::default(),
    );
    // `ProviderId`'s Debug redacts by itself; the raw key is what a
    // serialized status would carry, so it is checked directly.
    let shown = factory.status().order[0]
        .identity
        .provider
        .as_ref()
        .map(|provider| provider.as_str().to_string())
        .unwrap_or_default();
    assert!(!shown.contains("USERINFO-SENTINEL"), "{shown}");
    assert!(shown.contains("127.0.0.1"), "{shown}");
    let status = format!("{:?} {:?}", factory.status(), factory);
    assert!(!status.contains("USERINFO-SENTINEL"), "{status}");
    assert!(status.contains("127.0.0.1"), "the endpoint stays readable: {status}");
    let refused = config(
        &temp,
        compatible("http://decider:USERINFO-SENTINEL@127.0.0.1:1/v1", false),
    );
    let transports = CountingTransports::refusing();
    let error =
        build(&refused, &CatalogView::from_config(&refused), Some(key), &transports).expect_err("test: refused");
    assert!(!error.to_string().contains("USERINFO-SENTINEL"), "{error}");
    assert!(!format!("{error:?}").contains("USERINFO-SENTINEL"), "{error:?}");
}
