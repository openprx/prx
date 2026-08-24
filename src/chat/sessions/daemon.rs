//! Daemon-scoped session commands: chat as a control client for the daemon.
//!
//! `/sessions`, `/steer`, and `/kill` address the child sessions this chat
//! process owns. Their `--daemon` scope addresses work held by a *different*
//! process, and there is nothing shared to read: the work registry lives inside
//! the process that owns the work, so a second `prx` sees an empty one of its
//! own. Chat therefore talks to that process's gateway over the same control
//! API — and through the very same client — that `prx tasks` uses.
//!
//! Three properties of this arrangement are deliberate:
//!
//! - **Operator plane, not session plane.** The gateway authenticates a local
//!   operator by bearer token; it does not scope work to the chat user's
//!   memory principal. Chat consequently sees *every* run the daemon holds,
//!   including runs a channel started, and not because the two identity spaces
//!   were unified — they are not. Cross-entry visibility here is
//!   operator-level.
//! - **Never on the input loop.** The control API has no request timeout by
//!   design (a running task must not be abandoned because a clock expired), so
//!   awaiting it inline would let an unresponsive daemon freeze the TUI. The
//!   caller runs every request in its own task; this module only builds the
//!   endpoint, performs the call, and renders the outcome.
//! - **Failure degrades, it does not hide.** When the daemon cannot be
//!   reached, a listing still reports what this chat itself is running, with
//!   the reason the daemon part is missing stated first.

use crate::config::Config;
use crate::runtime::tasks_cli::{self, KillReport, MessageReport, TasksEndpoint, TasksListing, WorkItem};

/// One daemon-scoped request, parsed from a `--daemon` session command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonRequest {
    /// `/sessions --daemon`
    List,
    /// `/kill --daemon <address>`
    Kill { address: String },
    /// `/steer --daemon <address> <message>`
    Steer { address: String, message: String },
}

/// Where chat reaches the daemon, and how it authenticates.
///
/// `[chat.daemon]` wins when it is set, so `/sessions --daemon` and chat's
/// outbound `message_send` address the *same* daemon rather than each deciding
/// for itself. With it unset the address falls back to the `[gateway]` block
/// this config carries, which is right whenever chat and the daemon share a
/// config dir. The credential falls back to `[gateway] paired_tokens`, but only
/// to an entry that is still plaintext: pairing stores SHA-256 hashes, and the
/// gateway hashes whatever bearer it is given before comparing, so sending a
/// stored hash would hash it a second time and never match. Skipping those
/// yields no credential and a plain 401 the operator can act on, instead of a
/// token that looks configured and silently cannot authenticate.
#[must_use]
pub fn endpoint(config: &Config) -> TasksEndpoint {
    TasksEndpoint::resolve(config, configured_url(config), operator_token(config))
}

fn configured_url(config: &Config) -> Option<String> {
    let url = config.chat.daemon.url.trim();
    (!url.is_empty()).then(|| url.to_string())
}

fn operator_token(config: &Config) -> Option<String> {
    let configured = config.chat.daemon.token.trim();
    if !configured.is_empty() {
        return Some(configured.to_string());
    }
    config
        .gateway
        .paired_tokens
        .iter()
        .map(|token| token.trim())
        .find(|token| !token.is_empty() && !crate::security::pairing::is_token_hash(token))
        .map(ToString::to_string)
}

/// The line shown the moment a daemon command is accepted.
///
/// The answer arrives later, from another task, so without this the operator
/// would see nothing at all between pressing Enter and the daemon replying.
#[must_use]
pub fn pending_notice(request: &DaemonRequest, base_url: &str) -> String {
    match request {
        DaemonRequest::List => format!("Listing daemon work at {base_url}…"),
        DaemonRequest::Kill { address } => format!("Asking the daemon at {base_url} to kill {address}…"),
        DaemonRequest::Steer { address, .. } => format!("Sending a message to daemon task {address} at {base_url}…"),
    }
}

/// Perform one daemon request and render the outcome for the chat transcript.
///
/// `local_fallback` is this chat's own session listing, captured by the caller
/// before the task started; it is shown only when a listing could not be
/// fetched, so a dead daemon costs visibility of the daemon's work and nothing
/// else.
pub async fn run(endpoint: &TasksEndpoint, request: DaemonRequest, local_fallback: Option<&str>) -> String {
    match request {
        DaemonRequest::List => match tasks_cli::fetch_tasks(endpoint).await {
            Ok(listing) => render_listing(&listing),
            Err(error) => render_listing_failure(&error, local_fallback),
        },
        DaemonRequest::Kill { address } => {
            // Cascade, matching `prx tasks kill`: nothing else cleans up the
            // tools and processes a killed item started.
            match tasks_cli::request_kill(endpoint, &address, true).await {
                Ok(report) => render_kill(&report),
                Err(error) => render_failure("Daemon kill failed", &error),
            }
        }
        DaemonRequest::Steer { address, message } => {
            match tasks_cli::request_message(endpoint, &address, &message).await {
                Ok(report) => render_message(&report),
                Err(error) => render_failure("Daemon steer failed", &error),
            }
        }
    }
}

fn render_item(item: &WorkItem) -> String {
    let parent = item
        .parent
        .as_deref()
        .map_or_else(String::new, |parent| format!(" (parent {parent})"));
    // The run id is printed in full, not elided: it is the address the
    // operator has to type back into `/steer --daemon`, and the only one that
    // means anything outside the daemon process.
    let run = item
        .run_id
        .as_deref()
        .map_or_else(String::new, |run_id| format!(" run:{run_id}"));
    format!(
        "  {} {} {} {} {}{}{}",
        item.id,
        item.kind,
        item.state,
        tasks_cli::format_elapsed(item.elapsed_secs),
        item.name,
        parent,
        run
    )
}

/// Render a work listing the way `/sessions` renders local ones.
#[must_use]
pub fn render_listing(listing: &TasksListing) -> String {
    if listing.running.is_empty() && listing.unreaped.is_empty() {
        return "No daemon work items.".to_string();
    }
    let mut out = String::new();
    if listing.running.is_empty() {
        out.push_str("No daemon work items are currently running.");
    } else {
        out.push_str(&format!("Daemon sessions ({}):\n", listing.running.len()));
        for item in &listing.running {
            out.push_str(&render_item(item));
            out.push('\n');
        }
    }
    if !listing.unreaped.is_empty() {
        out.push_str(&format!(
            "\nSpawned but not reaped ({}) — children that outlived their owner:\n",
            listing.unreaped.len()
        ));
        for item in &listing.unreaped {
            out.push_str(&render_item(item));
            out.push('\n');
        }
    }
    out.trim_end().to_string()
}

/// Render a kill report, including partial outcomes.
#[must_use]
pub fn render_kill(report: &KillReport) -> String {
    let mut out = format!(
        "Killed {} in the daemon (cascade: {}) — {} target(s):\n",
        report.requested,
        if report.cascade { "on" } else { "off" },
        report.targets.len()
    );
    for target in &report.targets {
        out.push_str(&format!(
            "  {} {} {} {}\n",
            target.id, target.kind, target.outcome, target.name
        ));
    }
    // A target with no termination handle is a partial result, not a success —
    // the same distinction `prx tasks kill` turns into a non-zero exit code.
    if report.targets.iter().any(|target| target.outcome == "not_killable") {
        out.push_str("\nSome targets had no termination handle and could not be killed.");
    } else if report.targets.iter().any(|target| target.outcome == "requested") {
        out.push_str(
            "\nSome targets had not gone away yet when this report was produced. Nothing was \
             abandoned — run /sessions --daemon to see the current state.",
        );
    }
    out.trim_end().to_string()
}

/// Render a steer delivery report.
#[must_use]
pub fn render_message(report: &MessageReport) -> String {
    let run_id = report.run_id.as_deref().unwrap_or("-");
    format!(
        "Delivered to daemon task {} [{}] {} (run:{run_id}) — {}",
        report.id, report.kind, report.name, report.outcome
    )
}

/// Render a failed kill/steer: the reason, nothing invented on top of it.
#[must_use]
pub fn render_failure(what: &str, error: &anyhow::Error) -> String {
    format!("{what}: {error:#}")
}

/// Render a failed listing, degrading to this chat's own sessions.
///
/// The daemon half is what became unavailable; the local half is still true,
/// so it is still shown. Stating the reason first keeps the two apart — an
/// operator must never read the local list as if it were the daemon's.
#[must_use]
pub fn render_listing_failure(error: &anyhow::Error, local_fallback: Option<&str>) -> String {
    let head = format!("Daemon sessions unavailable: {error:#}");
    match local_fallback {
        Some(local) if !local.trim().is_empty() => {
            format!("{head}\n\nShowing this chat's own sessions instead:\n{local}")
        }
        _ => head,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn item(id: &str, name: &str, run_id: Option<&str>, parent: Option<&str>) -> WorkItem {
        WorkItem {
            id: id.to_string(),
            kind: "sub_agent".to_string(),
            name: name.to_string(),
            state: "running".to_string(),
            parent: parent.map(ToString::to_string),
            run_id: run_id.map(ToString::to_string),
            elapsed_secs: 75,
            pid: None,
            pgid: None,
        }
    }

    #[test]
    fn the_paired_token_is_used_so_a_shared_config_needs_no_flags() {
        let mut config = Config::default();
        config.gateway.paired_tokens = vec!["  ".to_string(), "tok-abc".to_string()];
        assert_eq!(operator_token(&config).as_deref(), Some("tok-abc"));
        config.gateway.paired_tokens.clear();
        assert!(operator_token(&config).is_none());
    }

    #[test]
    fn a_stored_token_hash_is_not_offered_as_a_bearer() {
        // What pairing actually persists is the hash, not the token. The
        // gateway hashes the bearer it receives before comparing, so handing
        // back a stored hash authenticates as nothing; the operator is better
        // served by no credential and a 401 than by one that looks configured.
        let mut config = Config::default();
        let hash = "a".repeat(64);
        assert!(crate::security::pairing::is_token_hash(&hash));
        config.gateway.paired_tokens = vec![hash];
        assert!(operator_token(&config).is_none());

        config.gateway.paired_tokens.push("tok-plain".to_string());
        assert_eq!(operator_token(&config).as_deref(), Some("tok-plain"));
    }

    #[test]
    fn an_explicit_chat_daemon_block_wins_over_the_local_gateway_block() {
        let mut config = Config::default();
        config.gateway.host = "127.0.0.1".to_string();
        config.gateway.port = 18931;
        config.gateway.paired_tokens = vec!["tok-gateway".to_string()];
        config.chat.daemon.url = " http://box:9000/ ".to_string();
        config.chat.daemon.token = " tok-chat ".to_string();
        // Same source of truth as chat's outbound `message_send`, so the two
        // cannot end up pointed at different daemons.
        assert_eq!(endpoint(&config).base_url(), "http://box:9000");
        assert_eq!(operator_token(&config).as_deref(), Some("tok-chat"));
    }

    #[test]
    fn the_endpoint_follows_the_configured_gateway() {
        let mut config = Config::default();
        config.gateway.host = "0.0.0.0".to_string();
        config.gateway.port = 18931;
        assert_eq!(endpoint(&config).base_url(), "http://127.0.0.1:18931");
        assert!(configured_url(&config).is_none());
    }

    #[test]
    fn a_listing_prints_the_full_run_id_because_that_is_what_gets_typed_back() {
        let listing = TasksListing {
            running: vec![item(
                "w3",
                "sub-agent",
                Some("d9671848-1111-2222-3333-444444444444"),
                Some("w2"),
            )],
            unreaped: Vec::new(),
            total: 1,
        };
        let rendered = render_listing(&listing);
        assert!(rendered.contains("Daemon sessions (1):"), "{rendered}");
        assert!(
            rendered.contains("run:d9671848-1111-2222-3333-444444444444"),
            "{rendered}"
        );
        assert!(rendered.contains("(parent w2)"), "{rendered}");
        assert!(rendered.contains("1m15s"), "{rendered}");
    }

    #[test]
    fn an_empty_listing_says_so_rather_than_rendering_nothing() {
        let listing = TasksListing {
            running: Vec::new(),
            unreaped: Vec::new(),
            total: 0,
        };
        assert_eq!(render_listing(&listing), "No daemon work items.");
    }

    #[test]
    fn unreaped_children_are_reported_even_when_nothing_is_running() {
        let listing = TasksListing {
            running: Vec::new(),
            unreaped: vec![item("w9", "orphan", None, None)],
            total: 1,
        };
        let rendered = render_listing(&listing);
        assert!(
            rendered.contains("No daemon work items are currently running."),
            "{rendered}"
        );
        assert!(rendered.contains("Spawned but not reaped (1)"), "{rendered}");
        assert!(rendered.contains("w9"), "{rendered}");
    }

    #[test]
    fn an_unreachable_daemon_still_shows_the_local_sessions() {
        let error = anyhow::anyhow!("cannot reach a running PRX process at http://127.0.0.1:1");
        let rendered = render_listing_failure(&error, Some("Background sessions:\n  #1 agent chat running 3s job"));
        assert!(rendered.starts_with("Daemon sessions unavailable: "), "{rendered}");
        assert!(rendered.contains("cannot reach a running PRX process"), "{rendered}");
        assert!(
            rendered.contains("Showing this chat's own sessions instead:"),
            "{rendered}"
        );
        assert!(rendered.contains("#1 agent chat running 3s job"), "{rendered}");
    }

    #[test]
    fn an_unreachable_daemon_with_no_local_sessions_still_states_the_reason() {
        let error = anyhow::anyhow!("connection refused");
        let rendered = render_listing_failure(&error, None);
        assert_eq!(rendered, "Daemon sessions unavailable: connection refused");
        assert_eq!(
            render_listing_failure(&error, Some("   ")),
            "Daemon sessions unavailable: connection refused"
        );
    }

    #[test]
    fn a_failure_reports_the_whole_context_chain() {
        let error = anyhow::anyhow!("connection refused").context("runtime task message failed");
        let rendered = render_failure("Daemon steer failed", &error);
        assert!(rendered.contains("runtime task message failed"), "{rendered}");
        assert!(rendered.contains("connection refused"), "{rendered}");
    }

    #[test]
    fn a_delivery_names_the_target_and_the_run_id() {
        let report = MessageReport {
            requested: "w3".to_string(),
            id: "w3".to_string(),
            run_id: Some("d9671848-1111-2222-3333-444444444444".to_string()),
            kind: "sub_agent".to_string(),
            name: "sub-agent".to_string(),
            outcome: "delivered".to_string(),
        };
        let rendered = render_message(&report);
        assert!(rendered.contains("Delivered to daemon task w3"), "{rendered}");
        assert!(
            rendered.contains("run:d9671848-1111-2222-3333-444444444444"),
            "{rendered}"
        );
        assert!(rendered.contains("delivered"), "{rendered}");
    }

    #[test]
    fn a_kill_that_could_not_terminate_a_target_says_so() {
        let report = KillReport {
            requested: "w3".to_string(),
            cascade: true,
            targets: vec![tasks_cli::KillTarget {
                id: "w3".to_string(),
                kind: "sub_agent".to_string(),
                name: "sub-agent".to_string(),
                outcome: "not_killable".to_string(),
            }],
        };
        let rendered = render_kill(&report);
        assert!(rendered.contains("cascade: on"), "{rendered}");
        assert!(rendered.contains("no termination handle"), "{rendered}");
    }

    #[test]
    fn a_pending_kill_is_reported_as_pending_not_as_done() {
        let report = KillReport {
            requested: "d9671848-1111-2222-3333-444444444444".to_string(),
            cascade: true,
            targets: vec![tasks_cli::KillTarget {
                id: "w3".to_string(),
                kind: "sub_agent".to_string(),
                name: "sub-agent".to_string(),
                outcome: "requested".to_string(),
            }],
        };
        let rendered = render_kill(&report);
        assert!(rendered.contains("had not gone away yet"), "{rendered}");
        assert!(rendered.contains("/sessions --daemon"), "{rendered}");
    }

    #[test]
    fn the_pending_notice_names_the_endpoint_and_the_target() {
        assert_eq!(
            pending_notice(&DaemonRequest::List, "http://127.0.0.1:16830"),
            "Listing daemon work at http://127.0.0.1:16830…"
        );
        assert!(
            pending_notice(
                &DaemonRequest::Kill {
                    address: "w3".to_string()
                },
                "http://h:1"
            )
            .contains("kill w3"),
        );
        assert!(
            pending_notice(
                &DaemonRequest::Steer {
                    address: "w3".to_string(),
                    message: "also check Y".to_string(),
                },
                "http://h:1"
            )
            .contains("daemon task w3"),
        );
    }

    /// A stand-in for the daemon's control API, speaking the same wire types
    /// `tasks_cli` decodes. It exists so the client half — URL construction,
    /// bearer auth, address encoding, decoding, rendering — is exercised
    /// against a real socket rather than mocked out; the daemon half is
    /// covered on a live gateway, and by a4's tests inside it.
    async fn serve_stub_daemon(expected_token: Option<&'static str>) -> (String, tokio::task::JoinHandle<()>) {
        use axum::extract::Path;
        use axum::http::{HeaderMap, StatusCode};
        use axum::routing::{get, post};

        fn authorized(headers: &HeaderMap, expected: Option<&'static str>) -> bool {
            let Some(expected) = expected else {
                return true;
            };
            headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "))
                .is_some_and(|token| token == expected)
        }

        let list = move |headers: HeaderMap| async move {
            if !authorized(&headers, expected_token) {
                return (
                    StatusCode::UNAUTHORIZED,
                    axum::Json(serde_json::json!({"error": "Unauthorized"})),
                )
                    .into_response();
            }
            let listing = TasksListing {
                running: vec![item(
                    "w3",
                    "sub-agent",
                    Some("d9671848-1111-2222-3333-444444444444"),
                    Some("w2"),
                )],
                unreaped: Vec::new(),
                total: 1,
            };
            axum::Json(listing).into_response()
        };
        let message = move |Path(id): Path<String>, headers: HeaderMap, body: String| async move {
            if !authorized(&headers, expected_token) {
                return (
                    StatusCode::UNAUTHORIZED,
                    axum::Json(serde_json::json!({"error": "Unauthorized"})),
                )
                    .into_response();
            }
            // Unknown addresses fail loudly, exactly as the real handler does.
            if id == "w999999" {
                return (
                    StatusCode::NOT_FOUND,
                    axum::Json(serde_json::json!({"error": format!("no running work item with id '{id}'")})),
                )
                    .into_response();
            }
            let sent = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|value| {
                    value
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .map(ToString::to_string)
                })
                .unwrap_or_default();
            axum::Json(MessageReport {
                requested: id,
                id: "w3".to_string(),
                run_id: Some("d9671848-1111-2222-3333-444444444444".to_string()),
                kind: "sub_agent".to_string(),
                name: sent,
                outcome: "delivered".to_string(),
            })
            .into_response()
        };
        let kill = move |Path(id): Path<String>, headers: HeaderMap| async move {
            if !authorized(&headers, expected_token) {
                return (
                    StatusCode::UNAUTHORIZED,
                    axum::Json(serde_json::json!({"error": "Unauthorized"})),
                )
                    .into_response();
            }
            axum::Json(KillReport {
                requested: id,
                cascade: true,
                targets: vec![tasks_cli::KillTarget {
                    id: "w3".to_string(),
                    kind: "sub_agent".to_string(),
                    name: "sub-agent".to_string(),
                    outcome: "killed".to_string(),
                }],
            })
            .into_response()
        };

        use axum::response::IntoResponse as _;
        let app = axum::Router::new()
            .route("/api/runtime/tasks", get(list))
            .route("/api/runtime/tasks/{id}/message", post(message))
            .route("/api/runtime/tasks/{id}/kill", post(kill));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test: bind stub daemon");
        let addr = listener.local_addr().expect("test: stub daemon address");
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), handle)
    }

    /// Evidence 4, over a real socket: both id spaces reach the same target.
    #[tokio::test]
    async fn both_address_spaces_reach_the_daemon_over_the_wire() {
        let (base_url, server) = serve_stub_daemon(None).await;
        let endpoint = TasksEndpoint::resolve(&Config::default(), Some(base_url), None);
        for address in ["w3", "d9671848-1111-2222-3333-444444444444"] {
            let rendered = run(
                &endpoint,
                DaemonRequest::Steer {
                    address: address.to_string(),
                    message: "also check Y".to_string(),
                },
                None,
            )
            .await;
            assert!(rendered.contains("Delivered to daemon task w3"), "{rendered}");
            // The stub echoes the delivered message back as the target name,
            // proving the body survived the round trip rather than being lost.
            assert!(rendered.contains("also check Y"), "{rendered}");
        }
        server.abort();
    }

    #[tokio::test]
    async fn a_listing_and_a_kill_round_trip_over_the_wire() {
        let (base_url, server) = serve_stub_daemon(None).await;
        let endpoint = TasksEndpoint::resolve(&Config::default(), Some(base_url), None);
        let listed = run(&endpoint, DaemonRequest::List, Some("local")).await;
        assert!(listed.contains("Daemon sessions (1):"), "{listed}");
        assert!(listed.contains("run:d9671848-1111-2222-3333-444444444444"), "{listed}");
        let killed = run(
            &endpoint,
            DaemonRequest::Kill {
                address: "d9671848-1111-2222-3333-444444444444".to_string(),
            },
            None,
        )
        .await;
        assert!(killed.contains("cascade: on"), "{killed}");
        assert!(killed.contains("w3 sub_agent killed"), "{killed}");
        server.abort();
    }

    #[tokio::test]
    async fn an_address_the_daemon_cannot_resolve_is_reported_not_swallowed() {
        let (base_url, server) = serve_stub_daemon(None).await;
        let endpoint = TasksEndpoint::resolve(&Config::default(), Some(base_url), None);
        let rendered = run(
            &endpoint,
            DaemonRequest::Steer {
                address: "w999999".to_string(),
                message: "go".to_string(),
            },
            None,
        )
        .await;
        assert!(rendered.starts_with("Daemon steer failed: "), "{rendered}");
        assert!(
            rendered.contains("no running work item with id 'w999999'"),
            "{rendered}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn the_configured_pairing_token_is_what_authenticates_chat() {
        let (base_url, server) = serve_stub_daemon(Some("tok-abc")).await;
        let mut config = Config::default();
        config.gateway.paired_tokens = vec!["tok-abc".to_string()];
        let paired = TasksEndpoint::resolve(&config, Some(base_url.clone()), operator_token(&config));
        assert!(
            run(&paired, DaemonRequest::List, None)
                .await
                .contains("Daemon sessions (1):")
        );

        let unpaired = TasksEndpoint::resolve(&Config::default(), Some(base_url), None);
        let rendered = run(
            &unpaired,
            DaemonRequest::List,
            Some("Background sessions:\n  #1 agent chat running 3s job"),
        )
        .await;
        assert!(rendered.starts_with("Daemon sessions unavailable: "), "{rendered}");
        assert!(rendered.contains("Unauthorized"), "{rendered}");
        assert!(rendered.contains("#1 agent chat running 3s job"), "{rendered}");
        server.abort();
    }

    /// Evidence 2, at the unit level: a request against a closed port returns a
    /// rendered degradation instead of hanging or panicking.
    #[tokio::test]
    async fn an_unreachable_daemon_degrades_to_the_local_listing() {
        let mut config = Config::default();
        config.gateway.host = "127.0.0.1".to_string();
        // Port 1 on loopback is not listening; the connect is refused at once.
        config.gateway.port = 1;
        let endpoint = endpoint(&config);
        let rendered = run(
            &endpoint,
            DaemonRequest::List,
            Some("Background sessions:\n  #1 agent chat running 3s job"),
        )
        .await;
        assert!(rendered.starts_with("Daemon sessions unavailable: "), "{rendered}");
        assert!(rendered.contains("#1 agent chat running 3s job"), "{rendered}");
    }

    #[tokio::test]
    async fn an_unreachable_daemon_fails_a_steer_loudly() {
        let mut config = Config::default();
        config.gateway.host = "127.0.0.1".to_string();
        config.gateway.port = 1;
        let endpoint = endpoint(&config);
        let rendered = run(
            &endpoint,
            DaemonRequest::Steer {
                address: "w3".to_string(),
                message: "also check Y".to_string(),
            },
            None,
        )
        .await;
        assert!(rendered.starts_with("Daemon steer failed: "), "{rendered}");
        assert!(rendered.contains("127.0.0.1:1"), "{rendered}");
    }
}
