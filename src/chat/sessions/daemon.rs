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
//! - **In flight means visible and killable.** Running off the input loop is
//!   what keeps the TUI responsive, but a request whose daemon accepted the
//!   connection and then said nothing would otherwise be a task nobody can see
//!   or end — the exact shape of stall this runtime refuses to paper over with
//!   a clock. Every request is therefore registered in [`InFlightRegistry`]
//!   while it runs, listed by `/sessions`, and endable by address with
//!   `/kill --daemon d<N>`.

use crate::config::Config;
use crate::runtime::tasks_cli::{self, KillReport, MessageReport, TasksEndpoint, TasksListing, WorkItem};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

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

/// One daemon request handed to a task and not yet answered.
///
/// The row exists for exactly as long as the task does: [`register_in_flight`]
/// creates it, and the [`InFlightGuard`] the task carries removes it on drop,
/// whichever way the task ends.
pub struct InFlightDaemonRequest {
    id: u64,
    label: String,
    base_url: String,
    started: Instant,
    cancel: CancellationToken,
}

/// Rows plus the id counter that hands out their `d<N>` addresses.
///
/// Ids are per registry and monotonic: an address an operator read out of
/// `/sessions` can never later denote a different request.
#[derive(Default)]
pub struct InFlightState {
    next_id: u64,
    rows: Vec<InFlightDaemonRequest>,
}

/// Shared registry of the daemon requests this chat is waiting on.
///
/// `parking_lot::Mutex` (synchronous, never held across `.await`), matching the
/// shell and PTY registries next door: every critical section here is a push, a
/// retain, or a scan over short owned rows.
pub type InFlightRegistry = Arc<Mutex<InFlightState>>;

/// A fresh, empty registry. One per [`ChatSessionsHandle`], so a test owns its
/// own and concurrent tests cannot see each other's rows.
///
/// [`ChatSessionsHandle`]: super::runtime::ChatSessionsHandle
#[must_use]
pub fn new_in_flight_registry() -> InFlightRegistry {
    Arc::new(Mutex::new(InFlightState::default()))
}

/// Removes its row when dropped. The spawned request task owns one, so a
/// finished, failed, or cancelled request stops being listed without anyone
/// having to remember to deregister it.
pub struct InFlightGuard {
    registry: InFlightRegistry,
    id: u64,
}

impl InFlightGuard {
    /// The `d<N>` address this request is listed and killed under.
    #[must_use]
    pub fn address(&self) -> String {
        format!("d{}", self.id)
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.registry.lock().rows.retain(|row| row.id != self.id);
    }
}

/// Publish one request as in flight, returning the guard that retires it.
///
/// `cancel` is the token the request task selects on; cancelling it is what
/// makes `/kill --daemon d<N>` able to end a request the daemon is never going
/// to answer. Nothing here imposes a deadline — the request runs until it
/// answers or until an operator ends it.
#[must_use]
pub fn register_in_flight(
    registry: &InFlightRegistry,
    request: &DaemonRequest,
    base_url: &str,
    cancel: CancellationToken,
) -> InFlightGuard {
    let mut state = registry.lock();
    state.next_id += 1;
    let id = state.next_id;
    state.rows.push(InFlightDaemonRequest {
        id,
        label: request_label(request),
        base_url: base_url.to_string(),
        started: Instant::now(),
        cancel,
    });
    drop(state);
    InFlightGuard {
        registry: Arc::clone(registry),
        id,
    }
}

/// Render the in-flight requests for `/sessions`, or `None` when there are none.
#[must_use]
pub fn in_flight_report(registry: &InFlightRegistry) -> Option<String> {
    let now = Instant::now();
    let rows: Vec<String> = registry
        .lock()
        .rows
        .iter()
        .map(|row| {
            format!(
                "  d{} {} {} {}",
                row.id,
                row.label,
                row.base_url,
                tasks_cli::format_elapsed(now.saturating_duration_since(row.started).as_secs())
            )
        })
        .collect();
    if rows.is_empty() {
        return None;
    }
    Some(format!(
        "Daemon requests in flight ({}):\n{}\nA daemon that accepted the connection and never \
         answered leaves its request here; /kill --daemon d<N> ends one without waiting on it.",
        rows.len(),
        rows.join("\n")
    ))
}

/// Stop waiting on one of *this chat's* requests, addressed as `d<N>`.
///
/// Returns `None` when the address is not a `d<N>` at all, which is the signal
/// to forward it to the daemon: every other address space (a run id, the
/// daemon's own `w42`) belongs to the daemon, and chat forwards those verbatim.
#[must_use]
pub fn cancel_in_flight(registry: &InFlightRegistry, address: &str) -> Option<String> {
    let id = parse_in_flight_address(address)?;
    let state = registry.lock();
    let Some(row) = state.rows.iter().find(|row| row.id == id) else {
        return Some(format!("No daemon request d{id} is in flight from this chat."));
    };
    row.cancel.cancel();
    Some(format!(
        "Stopped waiting on daemon request d{id} ({} at {}). Only this chat stopped waiting — the \
         daemon was never asked to undo anything, so /sessions --daemon still tells the truth \
         about it.",
        row.label, row.base_url
    ))
}

fn parse_in_flight_address(address: &str) -> Option<u64> {
    let trimmed = address.trim();
    let digits = trimmed.strip_prefix('d').or_else(|| trimmed.strip_prefix('D'))?;
    digits.parse::<u64>().ok().filter(|id| *id > 0)
}

/// What a request ended from this chat reports back to the transcript.
#[must_use]
pub fn cancelled_notice(request: &DaemonRequest, base_url: &str) -> String {
    format!(
        "Daemon request `{}` to {base_url} was ended from this chat before the daemon answered.",
        request_label(request)
    )
}

fn request_label(request: &DaemonRequest) -> String {
    match request {
        DaemonRequest::List => "list".to_string(),
        DaemonRequest::Kill { address } => format!("kill {address}"),
        DaemonRequest::Steer { address, .. } => format!("steer {address}"),
    }
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

/// Render a steer report, claiming exactly what the daemon claimed.
///
/// The daemon answers `outcome: "queued"`, and queued is not read: handing a
/// message to a run's bounded steering queue proves the queue had room and the
/// receiver still exists, never that the run polls it. A task-mode sub-agent
/// without tools registers a steering sender and never reads the receiver, so
/// its messages sit there forever. The headline used to read "Delivered to
/// daemon task ...", which told a chat operator the instruction had landed and
/// left them waiting on a target that was never going to act on it.
///
/// So the tag is printed verbatim under its own label, and the daemon's note —
/// the sentence that spells out what `queued` does not promise — is appended
/// when the daemon sent one. Nothing is substituted when it did not: a gloss
/// written here would be a client-side guess at a server-side guarantee.
///
/// MUTATION GUARD: put the "Delivered to daemon task" headline back and
/// `a_steer_report_repeats_the_daemons_outcome_instead_of_claiming_delivery`
/// fails.
#[must_use]
pub fn render_message(report: &MessageReport) -> String {
    let run_id = report.run_id.as_deref().unwrap_or("-");
    let mut out = format!(
        "Message to daemon task {} [{}] {} (run:{run_id}) — outcome: {}",
        report.id, report.kind, report.name, report.outcome
    );
    if let Some(note) = report.note.as_deref() {
        out.push('\n');
        out.push_str(note);
    }
    out
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

    /// A request that has been handed to a task must be listed while it runs:
    /// a daemon that accepted the connection and then said nothing is exactly
    /// the case where the only recourse is seeing it and ending it.
    #[test]
    fn an_in_flight_request_is_listed_while_it_runs() {
        let registry = new_in_flight_registry();
        assert!(in_flight_report(&registry).is_none(), "nothing in flight yet");

        let request = DaemonRequest::Kill {
            address: "w42".to_string(),
        };
        let guard = register_in_flight(&registry, &request, "http://127.0.0.1:9", CancellationToken::new());
        assert_eq!(guard.address(), "d1");

        let report = in_flight_report(&registry).expect("test: the request must be listed");
        assert!(report.contains("Daemon requests in flight (1)"), "{report}");
        assert!(report.contains("d1 kill w42 http://127.0.0.1:9"), "{report}");
        assert!(
            report.contains("/kill --daemon d<N>"),
            "the way out must be stated: {report}"
        );

        drop(guard);
        assert!(
            in_flight_report(&registry).is_none(),
            "a finished request stops being listed"
        );
    }

    /// Ending one is a cancellation of the waiting task, not a deadline: the
    /// row goes away because its task ended, and the daemon is not pretended
    /// to have been told anything.
    #[test]
    fn a_stuck_request_can_be_ended_by_address() {
        let registry = new_in_flight_registry();
        let cancel = CancellationToken::new();
        let _guard = register_in_flight(&registry, &DaemonRequest::List, "http://box:9000", cancel.clone());

        let text = cancel_in_flight(&registry, "d1").expect("test: d1 is this chat's address space");

        assert!(cancel.is_cancelled(), "ending must cancel the request task");
        assert!(text.contains("Stopped waiting on daemon request d1"), "{text}");
        assert!(text.contains("never asked to undo anything"), "{text}");
    }

    /// `d<N>` is the only address chat claims. Everything else — a run id, the
    /// daemon's own `w42` — belongs to the daemon and must be forwarded, or
    /// `/kill --daemon` would silently stop killing daemon work.
    #[test]
    fn only_the_d_address_space_is_handled_locally() {
        let registry = new_in_flight_registry();
        let _guard = register_in_flight(
            &registry,
            &DaemonRequest::List,
            "http://box:9000",
            CancellationToken::new(),
        );

        assert!(cancel_in_flight(&registry, "w42").is_none());
        assert!(cancel_in_flight(&registry, "42").is_none());
        assert!(cancel_in_flight(&registry, "3d1a7c4e-0000-4000-8000-000000000000").is_none());

        let unknown = cancel_in_flight(&registry, "d7").expect("test: d7 is still this chat's space");
        assert!(unknown.contains("No daemon request d7 is in flight"), "{unknown}");
    }

    fn item(id: &str, name: &str, run_id: Option<&str>, parent: Option<&str>) -> WorkItem {
        WorkItem {
            id: id.to_string(),
            kind: "sub_agent".to_string(),
            name: name.to_string(),
            state: "running".to_string(),
            parent: parent.map(ToString::to_string),
            run_id: run_id.map(ToString::to_string),
            batch_id: None,
            elapsed_secs: 75,
            pid: None,
            pgid: None,
            steerable: false,
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

    /// The steer report names the target, the run id, and the daemon's tag —
    /// and claims no delivery of its own.
    ///
    /// MUTATION GUARD: restore the "Delivered to daemon task" headline in
    /// `render_message` and the first assertion fails.
    #[test]
    fn a_steer_report_repeats_the_daemons_outcome_instead_of_claiming_delivery() {
        let report = MessageReport {
            requested: "w3".to_string(),
            id: "w3".to_string(),
            run_id: Some("d9671848-1111-2222-3333-444444444444".to_string()),
            kind: "sub_agent".to_string(),
            name: "sub-agent".to_string(),
            outcome: "queued".to_string(),
            note: Some("a run that does not read its queue never consumes it".to_string()),
        };
        let rendered = render_message(&report);
        assert!(
            !rendered.to_lowercase().contains("deliver"),
            "queued is not delivered, and chat must not upgrade it: {rendered}"
        );
        assert!(rendered.contains("Message to daemon task w3"), "{rendered}");
        assert!(
            rendered.contains("run:d9671848-1111-2222-3333-444444444444"),
            "{rendered}"
        );
        assert!(rendered.contains("outcome: queued"), "{rendered}");
        assert!(
            rendered.contains("a run that does not read its queue never consumes it"),
            "the daemon's own note is what tells the operator queued is not read: {rendered}"
        );
    }

    /// A daemon that sends no note gets none invented for it.
    #[test]
    fn a_steer_report_without_a_note_stays_a_single_line() {
        let report = MessageReport {
            requested: "w3".to_string(),
            id: "w3".to_string(),
            run_id: None,
            kind: "sub_agent".to_string(),
            name: "sub-agent".to_string(),
            outcome: "queued".to_string(),
            note: None,
        };
        assert_eq!(
            render_message(&report),
            "Message to daemon task w3 [sub_agent] sub-agent (run:-) — outcome: queued"
        );
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
                // What the real endpoint answers: accepted onto the queue, and
                // no claim about anyone reading it.
                outcome: "queued".to_string(),
                note: Some("the message is on the target's steering queue".to_string()),
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
            assert!(rendered.contains("Message to daemon task w3"), "{rendered}");
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
