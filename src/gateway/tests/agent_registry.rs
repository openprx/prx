//! MA-01b1b-2: agent incarnation lifecycle through the production resolve
//! path (`post_session_message`) and the production reload participant.
use super::runtime_generation::{authz_test_state, gateway_turn_runtime_owner_over};
use super::*;
use crate::config::{
    ConfigGeneration, ConfigGenerationParticipant, ConfigReloadTrigger, DelegateAgentConfig, PreparedConfigGeneration,
};
use std::sync::mpsc;

const TOKEN: &str = "prx-agent-registry-token";
const SESSION: &str = "no-such-session";

fn agent(temperature: f64, enabled: bool) -> DelegateAgentConfig {
    DelegateAgentConfig {
        provider: "ollama".to_owned(),
        model: "prx-local-model".to_owned(),
        system_prompt: None,
        api_key: None,
        credential_ref: None,
        temperature: Some(temperature),
        max_depth: 8,
        agentic: false,
        allowed_tools: Vec::new(),
        identity_dir: None,
        memory_scope: None,
        spawn_enabled: None,
        enabled,
        autonomy: None,
    }
}

type Agents<'a> = &'a [(&'a str, f64, bool)];

fn config(dir: &std::path::Path, agents: Agents<'_>, switch: bool) -> Config {
    let mut config = Config {
        config_path: dir.join("config.toml"),
        workspace_dir: dir.join("workspace"),
        api_key: Some("test-key".into()),
        api_url: Some("http://127.0.0.1:1".into()),
        default_provider: Some("custom:http://127.0.0.1:1/v1".into()),
        ..Config::default()
    };
    config.autonomy.level = crate::security::policy::AutonomyLevel::Full;
    config.gateway.targeted_agents = switch;
    config.providers.models.push(
        toml::from_str(
            "provider = \"ollama\"\nmodel = \"prx-local-model\"\noperations = [\"chat\"]\nnative_tools = true\ncontext_window = 65536\n",
        )
        .expect("test: model catalog"),
    );
    config.agents = agents
        .iter()
        .map(|(name, temperature, enabled)| ((*name).to_owned(), agent(*temperature, *enabled)))
        .collect();
    config
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Idle,
    FailPrepare,
    FailCommit,
    Block,
}

/// A participant registered after the gateway owner, so its `commit` runs
/// after the owner's retire.
struct Gate {
    mode: Mutex<Mode>,
    entered: mpsc::Sender<()>,
    release: Mutex<mpsc::Receiver<()>>,
}

struct GatePrepared {
    gate: Arc<Gate>,
    mode: Mode,
}

impl ConfigGenerationParticipant for Arc<Gate> {
    fn name(&self) -> &'static str {
        "agent_registry_test_gate"
    }

    fn supports_rebuild_field(&self, _field: &str) -> bool {
        true
    }

    fn prepare(
        &self,
        _generation: Arc<ConfigGeneration>,
        _changed_fields: &[String],
    ) -> anyhow::Result<Box<dyn PreparedConfigGeneration>> {
        let mode = *self.mode.lock();
        anyhow::ensure!(mode != Mode::FailPrepare, "injected prepare failure");
        Ok(Box::new(GatePrepared {
            gate: Arc::clone(self),
            mode,
        }))
    }
}

impl PreparedConfigGeneration for GatePrepared {
    fn commit(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(self.mode != Mode::FailCommit, "injected commit failure");
        if self.mode == Mode::Block {
            self.gate.entered.send(()).expect("test: the test is listening");
            self.gate
                .release
                .lock()
                .recv()
                .expect("test: the test releases the commit");
        }
        Ok(())
    }

    fn rollback(&mut self) {}
}

struct Harness {
    dir: tempfile::TempDir,
    /// Directory whose workspace holds the registry database.
    db_dir: std::path::PathBuf,
    memory: Arc<SqliteMemory>,
    manager: crate::config::SharedConfig,
    state: AppState,
    gate: Arc<Gate>,
    _participant: Arc<dyn ConfigGenerationParticipant>,
    entered: mpsc::Receiver<()>,
    release: mpsc::Sender<()>,
    _owner: Arc<GatewayTurnRuntimeOwner>,
}

fn owner_id() -> String {
    super::super::api::ApiPrincipal::of_token(TOKEN).owner_id
}

fn db(dir: &std::path::Path) -> rusqlite::Connection {
    rusqlite::Connection::open(dir.join("workspace/memory/brain.db")).expect("test: open registry database")
}

/// Start the production owner over a SQLite registry. `seed` runs on the raw
/// database after the schema exists and before the owner's startup sweep.
fn harness(agents: Agents<'_>, switch: bool, seed: impl FnOnce(&rusqlite::Connection, &str)) -> Harness {
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let config = config(dir.path(), agents, switch);
    std::fs::create_dir_all(&config.workspace_dir).expect("test: workspace dir");
    let memory = Arc::new(SqliteMemory::new(&config.workspace_dir).expect("test: sqlite memory"));
    seed(&db(dir.path()), &config.workspace_dir.to_string_lossy());
    let db_dir = dir.path().to_owned();
    start(dir, db_dir, config, memory)
}

/// A second real owner over the first one's backend, in its own workspace.
fn second_owner(first: &Harness, agents: Agents<'_>) -> Harness {
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let config = config(dir.path(), agents, true);
    std::fs::create_dir_all(&config.workspace_dir).expect("test: workspace dir");
    start(dir, first.db_dir.clone(), config, Arc::clone(&first.memory))
}

fn start(dir: tempfile::TempDir, db_dir: std::path::PathBuf, config: Config, memory: Arc<SqliteMemory>) -> Harness {
    let (manager, owner) = gateway_turn_runtime_owner_over(config, memory.clone());
    let (entered_tx, entered) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let gate = Arc::new(Gate {
        mode: Mutex::new(Mode::Idle),
        entered: entered_tx,
        release: Mutex::new(release_rx),
    });
    let participant: Arc<dyn ConfigGenerationParticipant> = Arc::new(Arc::clone(&gate));
    manager.register_participant(&participant);
    let mut state = authz_test_state(Config::default());
    state.config = Arc::clone(&manager);
    state.mem = memory.clone();
    state.turn_runtime = Some(Arc::clone(&owner));
    Harness {
        dir,
        db_dir,
        memory,
        manager,
        state,
        gate,
        _participant: participant,
        entered,
        release,
        _owner: owner,
    }
}

impl Harness {
    fn reload(&self, agents: Agents<'_>) -> anyhow::Result<()> {
        self.manager
            .apply_runtime_config(config(self.dir.path(), agents, true), ConfigReloadTrigger::Test)
            .map(|_| ())
    }

    /// POST a message to a session that does not exist: a resolved target
    /// answers `404 Session not found` with no error code; a refused target
    /// answers its typed code. Returns `(status, code)`.
    async fn post(&self, agent: Option<&str>) -> (u16, Option<String>) {
        send(self.state.clone(), axum::body::Body::from(message(agent))).await
    }

    /// `(incarnation, state, config_revision)` of every row of `key`.
    fn rows(&self, key: &str) -> Vec<(String, String, String)> {
        let conn = db(&self.db_dir);
        let mut stmt = conn
            .prepare(
                "SELECT incarnation, state, config_revision FROM agent_incarnations
                 WHERE agent_key = ?1 ORDER BY rowid",
            )
            .expect("test: prepare");
        stmt.query_map([key], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .expect("test: query")
            .collect::<Result<_, _>>()
            .expect("test: rows")
    }

    fn named_rows(&self) -> i64 {
        db(&self.db_dir)
            .query_row(
                "SELECT count(*) FROM agent_incarnations WHERE agent_key <> 'm'",
                [],
                |row| row.get(0),
            )
            .expect("test: count")
    }

    fn active(&self, key: &str) -> Vec<String> {
        self.rows(key)
            .into_iter()
            .filter(|(_, state, _)| state == "active")
            .map(|(incarnation, _, _)| incarnation)
            .collect()
    }
}

fn message(agent: Option<&str>) -> String {
    agent
        .map_or_else(
            || serde_json::json!({"message": "hi"}),
            |name| serde_json::json!({"message": "hi", "agent": name}),
        )
        .to_string()
}

/// Run one `post_session_message` over `body`. The request pins its runtime
/// before the body is read, so a slow body holds it on the old generation.
async fn send(state: AppState, body: axum::body::Body) -> (u16, Option<String>) {
    let mut request = axum::extract::Request::builder()
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(body)
        .expect("test: request");
    request
        .extensions_mut()
        .insert(super::super::api::ApiPrincipal::of_token(TOKEN));
    let response = super::super::api::post_session_message(
        axum::extract::State(state),
        axum::extract::Path(SESSION.to_owned()),
        request,
    )
    .await
    .into_response();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.expect("test: body").to_bytes();
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("test: json body");
    let code = value
        .pointer("/error/code")
        .and_then(|code| code.as_str())
        .map(str::to_owned);
    (status, code)
}

const RESOLVED: (u16, Option<&str>) = (404, None);

fn expect(outcome: &(u16, Option<String>), status: u16, code: Option<&str>) {
    assert_eq!((outcome.0, outcome.1.as_deref()), (status, code), "outcome");
}

fn revision_of(temperature: f64) -> String {
    agent_lifecycle::config_revision(&agent(temperature, true)).expect("test: revision")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_recreation_uses_new_incarnation() {
    let main_revision = SqliteMemory::schema_migration_checksum("prx.ma.main.v1");
    let seed_main = {
        let main_revision = main_revision.clone();
        move |conn: &rusqlite::Connection, workspace: &str| {
            conn.execute(
                "INSERT INTO agent_incarnations VALUES (?1, ?2, 'm', '00000000-0000-4000-8000-0000000000aa', 'active', ?3)",
                rusqlite::params![workspace, owner_id(), main_revision],
            )
            .expect("test: seed Main");
        }
    };

    // 1. Switch off, unknown and disabled resolves leave the registry empty.
    let off = harness(&[("x", 0.1, true)], false, |_, _| {});
    expect(&off.post(Some("x")).await, 409, Some("agent_disabled"));
    assert_eq!(off.named_rows(), 0);
    let h = harness(
        &[("x", 0.1, true), ("sleeper", 0.1, false), ("y", 0.1, true)],
        true,
        seed_main,
    );
    expect(&h.post(Some("ghost")).await, 404, Some("agent_unknown"));
    expect(&h.post(Some("sleeper")).await, 409, Some("agent_disabled"));
    assert_eq!(h.named_rows(), 0, "refusals write no row");

    // 2. The first resolve of X mints U1.
    expect(&h.post(Some("x")).await, RESOLVED.0, RESOLVED.1);
    let first = h.active("n:x");
    assert_eq!(first.len(), 1, "the first resolve mints one incarnation");
    let u1 = first[0].clone();
    assert_eq!(h.rows("n:x")[0].2, revision_of(0.1));

    // 3. An edit plus an accepted reload keep U1 and change the revision;
    // Main's revision is untouched.
    h.reload(&[("x", 0.2, true), ("sleeper", 0.1, false), ("y", 0.1, true)])
        .expect("test: the edit reload is accepted");
    assert_eq!(h.active("n:x"), vec![u1.clone()], "an edit retains the incarnation");
    assert_eq!(
        h.rows("n:x")[0].2,
        revision_of(0.2),
        "config_revision changed with the edit"
    );
    assert_ne!(revision_of(0.1), revision_of(0.2));
    assert_eq!(h.rows("m")[0].2, main_revision, "Main's config_revision is unchanged");

    // 4. A reload a participant rejects in prepare, or one whose later
    // participant fails in commit (rollback compensation), leaves U1 active.
    let removal: Agents<'_> = &[("sleeper", 0.1, false), ("y", 0.1, true)];
    *h.gate.mode.lock() = Mode::FailPrepare;
    assert!(h.reload(removal).is_err());
    assert_eq!(h.active("n:x"), vec![u1.clone()]);
    *h.gate.mode.lock() = Mode::FailCommit;
    assert!(h.reload(removal).is_err());
    assert_eq!(
        h.active("n:x"),
        vec![u1.clone()],
        "a failed commit re-activates the retired row"
    );
    expect(&h.post(Some("x")).await, RESOLVED.0, RESOLVED.1);
    assert_eq!(h.active("n:x"), vec![u1.clone()]);

    // 5. An accepted removal sets U1 retiring.
    *h.gate.mode.lock() = Mode::Idle;
    h.reload(removal).expect("test: the removal is accepted");
    assert_eq!(
        h.rows("n:x"),
        vec![(u1.clone(), "retiring".to_owned(), revision_of(0.2))]
    );

    // 5b. A request that pinned the old runtime before the commit and selects
    // its target while the commit holds the removal of Y is refused (409) and
    // mints nothing.
    *h.gate.mode.lock() = Mode::Block;
    let (body_tx, mut body_rx) = tokio::sync::mpsc::channel::<axum::body::Bytes>(1);
    let (polled_tx, polled_rx) = tokio::sync::oneshot::channel::<()>();
    let mut polled_tx = Some(polled_tx);
    // The first poll of the body proves the handler already pinned its runtime.
    let stream = futures::stream::poll_fn(move |cx| {
        if let Some(polled) = polled_tx.take() {
            polled.send(()).ok();
        }
        body_rx
            .poll_recv(cx)
            .map(|chunk| chunk.map(Ok::<_, std::convert::Infallible>))
    });
    let held = tokio::spawn(send(h.state.clone(), axum::body::Body::from_stream(stream)));
    let wait = std::time::Duration::from_secs(20);
    tokio::time::timeout(wait, polled_rx)
        .await
        .expect("test: the handler polled the body in time")
        .expect("test: the poll signal was sent");
    let manager = Arc::clone(&h.manager);
    let pending_config = config(h.dir.path(), &[("sleeper", 0.1, false)], true);
    let reload = std::thread::spawn(move || manager.apply_runtime_config(pending_config, ConfigReloadTrigger::Test));
    h.entered.recv_timeout(wait).expect("test: the commit is blocked");
    body_tx
        .send(axum::body::Bytes::from(message(Some("y"))))
        .await
        .expect("test: the held request is waiting");
    drop(body_tx);
    expect(
        &tokio::time::timeout(wait, held)
            .await
            .expect("test: the held request finished")
            .expect("test: held request"),
        409,
        Some("agent_disabled"),
    );
    assert!(h.rows("n:y").is_empty(), "the barrier writes no row");
    h.release.send(()).expect("test: release");
    reload
        .join()
        .expect("test: reload thread")
        .expect("test: the removal of Y is accepted");
    *h.gate.mode.lock() = Mode::Idle;
    expect(&h.post(Some("y")).await, 404, Some("agent_unknown"));

    // 6. Re-add while retiring: no active row, and the target is refused.
    h.reload(&[("x", 0.2, true), ("sleeper", 0.1, false)])
        .expect("test: the re-add is accepted");
    let outcome = h.post(Some("x")).await;
    assert!(h.active("n:x").is_empty(), "no active incarnation while retiring");
    expect(&outcome, 409, Some("agent_disabled"));

    // 7. Stand-in for the purge: U1 becomes retired.
    db(h.dir.path())
        .execute(
            "UPDATE agent_incarnations SET state = 'retired' WHERE incarnation = ?1",
            [&u1],
        )
        .expect("test: retire U1");

    // 8. The next resolve mints U2, and the U1 row is unchanged.
    expect(&h.post(Some("x")).await, RESOLVED.0, RESOLVED.1);
    let u2 = h.active("n:x");
    assert_eq!(u2.len(), 1);
    assert_ne!(u2[0], u1, "re-creation uses a new incarnation");
    assert_eq!(h.rows("n:x")[0], (u1, "retired".to_owned(), revision_of(0.2)));
    assert_eq!(
        h.rows("m")[0].2,
        main_revision,
        "Main's config_revision is still unchanged"
    );
}

/// 5c. A failed startup sweep latches the failed owner's targeted resolves
/// until its own reconcile succeeds, whatever another owner on the same backend
/// does. The failure is injected on the real path with a workspace-scoped trigger.
#[tokio::test]
async fn failed_startup_sweep_latches_targeted_turns_until_reconciled() {
    let h = harness(&[("v", 0.1, true), ("w", 0.1, true)], true, |conn, workspace| {
        conn.execute(
            "INSERT INTO agent_incarnations VALUES (?1, ?2, 'n:z', '00000000-0000-4000-8000-0000000000bb', 'active', ?3)",
            rusqlite::params![workspace, owner_id(), revision_of(0.1)],
        )
        .expect("test: seed Z");
        conn.execute_batch(&format!(
            "CREATE TRIGGER t_fail BEFORE UPDATE ON agent_incarnations WHEN OLD.workspace_id = '{workspace}'
             BEGIN SELECT RAISE(ABORT, 'injected'); END"
        ))
        .expect("test: inject the sweep failure");
    });
    expect(&h.post(Some("v")).await, 409, Some("agent_disabled"));
    assert!(h.rows("n:v").is_empty(), "a latched resolve writes no row");
    assert_eq!(h.active("n:z").len(), 1, "the failed sweep left Z active");
    assert_eq!(h.post(None).await, (404, None), "Main is unaffected by the latch");

    // A second owner on the same backend reconciles successfully and serves.
    let other = second_owner(&h, &[("q", 0.1, true)]);
    other
        .reload(&[("q", 0.2, true)])
        .expect("test: the other owner reloads");
    expect(&other.post(Some("q")).await, RESOLVED.0, RESOLVED.1);
    assert_eq!(other.active("n:q").len(), 1, "the other owner mints");
    expect(&h.post(Some("v")).await, 409, Some("agent_disabled"));
    assert!(h.rows("n:v").is_empty(), "the first owner's latch still writes no row");

    db(&h.db_dir)
        .execute_batch("DROP TRIGGER t_fail")
        .expect("test: remove the failure");
    h.reload(&[("v", 0.1, true), ("w", 0.2, true)])
        .expect("test: an unrelated reload is accepted");
    assert_eq!(h.rows("n:z")[0].1, "retiring", "the finalize sweep retires Z");
    expect(&h.post(Some("v")).await, RESOLVED.0, RESOLVED.1);
    assert_eq!(h.active("n:v").len(), 1, "the latch is clear, so V mints");
}
