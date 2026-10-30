//! Real reservation/Tally/transport boundary. No prepared-context constructor
//! or adoption/profile proof is fabricated by this owned-receipt fixture.
use super::*;
use crate::causal_tree::state::TurnBudgets;
use crate::config::Config;
use crate::providers::attempt_accounting::{AttemptAccounting, PhysicalKind, PhysicalRequest};
use crate::providers::compatible::{AuthStyle, OpenAiCompatibleProvider};
use crate::providers::credential_probe::{CredentialProbe, PROBE_MODEL};
use crate::runtime::decision_accounting::{AccountingScope, PhysicalEstimate};
use crate::skills::lifecycle::{ManagedProviderModel, ModelCallError, ProposerModel, TallyCounters};

const PROVIDER: &str = "fixture-owned-provider";

struct OwnerCheckedAccounting {
    actual: Arc<AccountingScope>,
    expected_scope: String,
    key: Arc<DecisionKey>,
}

impl sealed::AccountingOwner for OwnerCheckedAccounting {}

#[async_trait]
impl AttemptAccounting for OwnerCheckedAccounting {
    fn deadline(&self) -> Instant {
        self.actual.deadline()
    }

    async fn begin(&self, request: PhysicalRequest<'_>) -> Result<Box<dyn AttemptReceipt>> {
        ensure!(request.kind() == PhysicalKind::Chat, "fixture only supports chat");
        let binding = self.key.hmac(
            KeyDomain::AttemptAccounting,
            &[
                b"lesson-owner-boundary-fixture",
                request.endpoint().as_bytes(),
                request.body(),
            ],
        );
        let actual_body: serde_json::Value = serde_json::from_slice(request.body())?;
        let actual_model = actual_body
            .get("model")
            .and_then(serde_json::Value::as_str)
            .context("physical fixture body has no actual model")?;
        ensure!(actual_model == PROBE_MODEL, "fixture physical model changed");
        let receipt = self
            .actual
            .begin_physical(PhysicalEstimate {
                purpose: "evolution_proposer",
                provider: PROVIDER,
                model: actual_model,
                body: request.body(),
                transport_binding: &binding,
                request_id: "lesson-owner-boundary-fixture",
                deadline: self.deadline(),
                remaining_attempts: 1,
            })
            .await?;
        // Pure construction after a genuine OwnedReceipt reservation. The
        // production Tally must see that reservation before owner refusal.
        Ok(Box::new(OwnerCheckedReceipt {
            inner: receipt.into_provider_receipt(),
            expected_scope: self.expected_scope.clone(),
        }))
    }
}

struct OwnerCheckedReceipt {
    inner: Box<dyn AttemptReceipt>,
    expected_scope: String,
}

impl sealed::Receipt for OwnerCheckedReceipt {}

#[async_trait]
impl AttemptReceipt for OwnerCheckedReceipt {
    fn deadline(&self) -> Instant {
        self.inner.deadline()
    }

    fn physical_identity(&self) -> Option<PhysicalAttemptIdentity<'_>> {
        self.inner.physical_identity()
    }

    async fn prepare_handoff(&mut self, body: &[u8]) -> Result<()> {
        let physical = self
            .inner
            .physical_identity()
            .context("fixture has no actual identity")?;
        // The exact same owner predicate used by production LessonReceipt.
        ensure_physical_owner(physical, &self.expected_scope)?;
        self.inner.prepare_handoff(body).await
    }

    fn retain_response_chunk(&self, bytes: usize) -> Result<()> {
        self.inner.retain_response_chunk(bytes)
    }

    async fn settle(self: Box<Self>, usage: crate::llm::route_decision::TokenUsage) -> Result<()> {
        self.inner.settle(usage).await
    }
}

fn model(base_url: &str, accounting: OwnerCheckedAccounting) -> ManagedProviderModel {
    ManagedProviderModel {
        provider: Arc::new(OpenAiCompatibleProvider::new_optional_credential(
            PROVIDER,
            base_url,
            None,
            AuthStyle::Bearer,
        )),
        binding: ModelBinding {
            provider_label: PROVIDER.into(),
            model: PROBE_MODEL.into(),
            temperature_milli: 100,
        },
        accounting: Arc::new(accounting),
    }
}

#[tokio::test]
async fn wrong_actual_owner_refuses_before_wire_with_real_spent_liability_and_tally() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let mut config = Config {
        workspace_dir: workspace.path().to_path_buf(),
        config_path: workspace.path().join("config.toml"),
        default_provider: Some(PROVIDER.into()),
        default_model: Some(PROBE_MODEL.into()),
        ..Config::default()
    };
    config.auth.codex_auth_json_auto_import = false;
    config.autonomy.max_cost_per_day_cents = 1000;
    config.cost.prices.insert(
        format!("{PROVIDER}/{PROBE_MODEL}"),
        crate::config::schema::ModelPricing {
            input: 2.0,
            output: 4.0,
            cache_write: 0.0,
            cache_read: 0.0,
        },
    );
    let key = DecisionKey::load_or_create(workspace.path())?;
    crate::runtime::decision_accounting::initialize_tokenizers();
    let cleanup = Arc::new(CleanupTracker::default());
    let config = Arc::new(config);
    let budgets = TurnBudgets {
        tokens: 65_536,
        micro_usd: 1_000_000,
        latency_ms: 15_000,
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    let actual = Arc::new(AccountingScope::new(
        Arc::clone(&config),
        Arc::clone(&key),
        Arc::clone(&cleanup),
        [11; 32],
        budgets,
        deadline,
    ));
    let expected = Arc::new(AccountingScope::new(
        Arc::clone(&config),
        Arc::clone(&key),
        Arc::clone(&cleanup),
        [22; 32],
        budgets,
        deadline,
    ));
    assert_ne!(actual.scope_id(), expected.scope_id());
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let base_url = format!("http://{}/v1", listener.local_addr()?);
    let wrong = model(
        &base_url,
        OwnerCheckedAccounting {
            actual: Arc::clone(&actual),
            expected_scope: expected.scope_id(),
            key: Arc::clone(&key),
        },
    );
    let tally = Arc::new(TallyCounters::default());
    let result = wrong
        .complete("Fixture only.", "Reject a wrong physical owner.", Arc::clone(&tally))
        .await;
    assert!(matches!(result, Err(ModelCallError::Failed { ref usage, .. })
        if usage.physical_requests == 1 && usage.settled_requests == 0));
    assert_eq!(tally.snapshot().physical_requests, 1);
    assert_eq!(tally.snapshot().settled_requests, 0);
    assert_eq!(
        listener
            .accept()
            .err()
            .context("wrong owner reached native transport")?
            .kind(),
        std::io::ErrorKind::WouldBlock
    );
    let tracker = crate::cost::tracker::CostTracker::for_workspace(config.cost.clone(), workspace.path())?;
    let held = tracker.physical_scope_usage(&actual.scope_id())?;
    assert_eq!(held.calls, 1);
    assert_eq!(held.settled_calls, 0);
    assert!(held.unresolved && held.tokens > 0 && held.micro_usd > 0);
    assert!(actual.remaining_budgets().tokens < budgets.tokens);
    assert_eq!(tracker.physical_scope_usage(&expected.scope_id())?.calls, 0);
    let ledger_path = workspace.path().join("state/costs.reservations.jsonl");
    let refused_bytes = std::fs::read(&ledger_path)?;
    // The new read-only aggregate must not turn this genuine admitted,
    // undispatched-to-wire liability into complete zero-cost coverage.
    let before_snapshot = std::fs::read(&ledger_path)?;
    assert!(
        matches!(
            crate::cost::tracker::physical_scopes_read_only(workspace.path(), &[actual.scope_id()]),
            Err(_) | Ok(None)
        ),
        "missing or unresolved ledger coverage must remain Unknown, never complete zero"
    );
    assert_eq!(std::fs::read(&ledger_path)?, before_snapshot);
    let rows: Vec<serde_json::Value> = std::str::from_utf8(&refused_bytes)?
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|row| {
        row.get("event").and_then(serde_json::Value::as_str) == Some("estimated_reserved_v1")
            && row
                .pointer("/request/reservation/turn_id")
                .and_then(serde_json::Value::as_str)
                == Some(actual.scope_id().as_str())
    }));
    assert!(
        rows.iter()
            .any(|row| row.get("event").and_then(serde_json::Value::as_str) == Some("dispatched"))
    );

    // Same-owner control uses the same predicate, production Tally, real native
    // provider and real settlement; it does not create a lesson context.
    let probe = CredentialProbe::start().await;
    let correct = model(
        &probe.base_url(),
        OwnerCheckedAccounting {
            actual: Arc::clone(&expected),
            expected_scope: expected.scope_id(),
            key,
        },
    );
    let reply = correct
        .complete(
            "Fixture only.",
            "Allow the actual physical owner.",
            Arc::new(TallyCounters::default()),
        )
        .await?;
    assert_eq!(reply.usage.physical_requests, 1);
    assert_eq!(reply.usage.settled_requests, 1);
    assert_eq!(probe.requests().len(), 1);
    let settled = tracker.physical_scope_usage(&expected.scope_id())?;
    assert_eq!(settled.calls, 1);
    assert_eq!(settled.settled_calls, 1);
    assert!(!settled.unresolved);
    let still_held = tracker.physical_scope_usage(&actual.scope_id())?;
    assert_eq!(still_held.attempt_ids, held.attempt_ids);
    assert_eq!(still_held.tokens, held.tokens);
    assert_eq!(still_held.micro_usd, held.micro_usd);
    assert!(still_held.unresolved && still_held.settled_calls == 0);
    assert!(std::fs::read(&ledger_path)?.starts_with(&refused_bytes));
    cleanup
        .drain(tokio::time::Instant::now() + Duration::from_secs(5))
        .await?;
    Ok(())
}
