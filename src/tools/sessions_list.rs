//! sessions_list — list active and completed sub-agent sessions.
//!
//! Wraps the shared active_runs registry from SessionsSpawnTool,
//! exposing a dedicated tool that aligns with OpenClaw's `sessions_list`.

use super::sessions_read_model::{self, RecoveredTaskRun, RecoveredTaskStatus};
use super::sessions_spawn::{SubAgentRun, SubAgentStatus};
use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::memory::Memory;
use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Tool to list active and recently completed sub-agent sessions.
pub struct SessionsListTool {
    active_runs: Arc<RwLock<Vec<SubAgentRun>>>,
    memory: Option<Arc<dyn Memory>>,
    workspace_id: String,
}

impl SessionsListTool {
    pub fn new(active_runs: Arc<RwLock<Vec<SubAgentRun>>>) -> Self {
        Self {
            active_runs,
            memory: None,
            workspace_id: String::new(),
        }
    }

    pub fn with_shared_memory(mut self, memory: Arc<dyn Memory>, workspace_id: impl Into<String>) -> Self {
        self.memory = Some(memory);
        self.workspace_id = workspace_id.into();
        self
    }

    fn workspace_id(&self) -> &str {
        if self.workspace_id.is_empty() {
            "/tmp"
        } else {
            &self.workspace_id
        }
    }
}

#[async_trait]
impl Tool for SessionsListTool {
    fn name(&self) -> &str {
        "sessions_list"
    }

    fn description(&self) -> &str {
        "List active and recently completed sub-agent sessions. \
         Shows run_id, source/manageability, origin, agent_index_hint, status, age, usage, and task for each session spawned via sessions_spawn. \
         Use this to check what sub-agents are running or have recently finished."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "status": {
                    "type": "string",
                    "enum": ["all", "running", "completed", "failed"],
                    "default": "all",
                    "description": "Filter by session status. Defaults to 'all'."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 100,
                    "description": "Maximum number of sessions to return. Defaults to 20."
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let status_filter = args.get("status").and_then(|v| v.as_str()).unwrap_or("all");
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;

        let runs = self.active_runs.read().await;
        let filtered: Vec<&SubAgentRun> = runs
            .iter()
            .filter(|r| match status_filter {
                "running" => matches!(r.status, SubAgentStatus::Running),
                "completed" => matches!(r.status, SubAgentStatus::Completed(_)),
                "failed" => matches!(r.status, SubAgentStatus::Failed(_)),
                _ => true,
            })
            .take(limit)
            .collect();

        let active_ids = filtered.iter().map(|run| run.id.as_str()).collect::<HashSet<_>>();
        let remaining = limit.saturating_sub(filtered.len());
        let recovered = if remaining > 0 {
            sessions_read_model::recover_task_runs(self.memory.as_ref(), self.workspace_id(), &args, limit)
                .await?
                .into_iter()
                .filter(|run| !active_ids.contains(run.run_id.as_str()))
                .filter(|run| recovered_matches_filter(run, status_filter))
                .take(remaining)
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        if filtered.is_empty() && recovered.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: format!("No sessions found (filter: {status_filter})."),
                error: None,
            });
        }

        let rendered: Vec<String> = filtered
            .iter()
            .enumerate()
            .map(|(idx, r)| {
                let status = match &r.status {
                    SubAgentStatus::Running => "🔄 running".to_string(),
                    SubAgentStatus::AwaitingInput { prompt } => {
                        format!("❓ awaiting approval: {prompt}")
                    }
                    SubAgentStatus::Completed(msg) => {
                        let preview = msg.chars().take(60).collect::<String>();
                        let ellipsis = if msg.len() > 60 { "…" } else { "" };
                        format!("✅ completed: {preview}{ellipsis}")
                    }
                    SubAgentStatus::Failed(e) => format!("❌ failed: {e}"),
                };
                let age = (Utc::now() - r.started_at).num_seconds();
                let origin = if r.parent_run_id.is_some() { "model" } else { "user" };
                let agent_index_hint = idx.saturating_add(1);
                let usage = format_run_usage(&r.token_usage_records);
                let liveness = format_run_liveness(r);
                let batch = r
                    .batch_id
                    .as_deref()
                    .map(|batch_id| format!(", batch={batch_id}"))
                    .unwrap_or_default();
                format!(
                    "• `{}` [agent_index_hint=#{agent_index_hint}, source=runtime, manageable=true, origin={origin}, usage={usage}, {age}s ago{liveness}{batch}] {status}\n  task: {}",
                    r.id, r.task
                )
            })
            .collect();

        let mut lines = group_lines_by_batch(&filtered, &rendered);
        let mut recovered_lines = recovered.iter().map(format_recovered_run).collect::<Vec<_>>();
        // Runs, not lines. `group_lines_by_batch` inserts one header line per
        // batch, so counting the rendered lines reported more sessions than
        // exist — and the model reading this number acts on it, deciding a
        // fan-out is still short of its expected width or that runs it never
        // started are alive. The runs are exactly what was filtered plus what
        // was recovered; presentation cannot change how many there are.
        //
        // MUTATION GUARD: count `lines.len()` again and
        // `the_shown_count_is_runs_not_rendered_lines` fails.
        let shown = filtered.len().saturating_add(recovered.len());
        lines.append(&mut recovered_lines);

        Ok(ToolResult {
            success: true,
            output: format!(
                "Sessions ({shown} shown, filter: {}):\n\n{}",
                status_filter,
                lines.join("\n\n")
            ),
            error: None,
        })
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Extended
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::Automation]
    }
}

/// Lay the rendered run lines out so the members of one `spawn_batch` fan-out
/// read as one unit.
///
/// A batch's members are otherwise scattered through the list by spawn order
/// and indistinguishable from unrelated runs, which makes the single question
/// an operator actually has — "is that fan-out done, and did any of it fail?" —
/// unanswerable without reading every line. Grouping is presentation only: the
/// same runs appear, exactly once each, with their `agent_index_hint` computed
/// from the unsorted list so a hint still means the same run it did before.
///
/// A batch header is emitted at the position of its *first* member, so a
/// batchless list is byte-for-byte what it always was.
fn group_lines_by_batch(runs: &[&SubAgentRun], rendered: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(rendered.len());
    let mut seen: HashSet<&str> = HashSet::new();
    for (run, line) in runs.iter().zip(rendered) {
        let Some(batch_id) = run.batch_id.as_deref() else {
            out.push(line.clone());
            continue;
        };
        if !seen.insert(batch_id) {
            // Already emitted with its batch, below its header.
            continue;
        }
        let members: Vec<(&&SubAgentRun, &String)> = runs
            .iter()
            .zip(rendered)
            .filter(|(candidate, _)| candidate.batch_id.as_deref() == Some(batch_id))
            .collect();
        out.push(format!(
            "▣ batch `{batch_id}` — {}",
            summarize_batch(members.iter().map(|(run, _)| **run))
        ));
        for (_, member_line) in members {
            out.push(member_line.clone());
        }
    }
    out
}

/// One-line tally of a batch, so its header answers "is it done?" on its own.
fn summarize_batch<'a>(members: impl Iterator<Item = &'a SubAgentRun>) -> String {
    let (mut running, mut completed, mut failed, mut total) = (0_usize, 0_usize, 0_usize, 0_usize);
    for member in members {
        total = total.saturating_add(1);
        match member.status {
            SubAgentStatus::Running | SubAgentStatus::AwaitingInput { .. } => running = running.saturating_add(1),
            SubAgentStatus::Completed(_) => completed = completed.saturating_add(1),
            SubAgentStatus::Failed(_) => failed = failed.saturating_add(1),
        }
    }
    format!("{total} run(s): {running} running, {completed} completed, {failed} failed")
}

/// Liveness of a run that has not reported a terminal status yet.
///
/// The point of surfacing it here is that "running" alone cannot be acted on:
/// a run that has been silent since it started and one that emitted an event a
/// second ago render identically without this. The numbers come from the run's
/// own progress beat, i.e. from `crate::agent::idle`'s single definition of
/// progress — this is a *report*, never a second policy, and nothing in this
/// module terminates anything on the strength of it.
///
/// Empty for terminal runs, where silence is expected and meaningless.
fn format_run_liveness(run: &SubAgentRun) -> String {
    if matches!(run.status, SubAgentStatus::Completed(_) | SubAgentStatus::Failed(_)) {
        return String::new();
    }
    format!(
        ", last progress {}s ago after {} event(s)",
        run.idle_for().as_secs(),
        run.progress.events()
    )
}

fn recovered_matches_filter(run: &RecoveredTaskRun, status_filter: &str) -> bool {
    match status_filter {
        "running" => matches!(run.status, RecoveredTaskStatus::Running),
        "completed" => matches!(run.status, RecoveredTaskStatus::Completed),
        "failed" => matches!(run.status, RecoveredTaskStatus::Failed),
        _ => true,
    }
}

fn format_recovered_run(run: &RecoveredTaskRun) -> String {
    let status = match run.status {
        RecoveredTaskStatus::Running => format!("🔄 running (memory: {})", run.last_event_type),
        RecoveredTaskStatus::Completed => {
            let detail = run.status_detail.as_deref().unwrap_or("completed");
            let preview = detail.chars().take(60).collect::<String>();
            let ellipsis = if detail.len() > 60 { "…" } else { "" };
            format!("✅ completed (memory): {preview}{ellipsis}")
        }
        RecoveredTaskStatus::Failed => {
            let detail = run.status_detail.as_deref().unwrap_or(run.last_event_type.as_str());
            format!("❌ failed (memory): {detail}")
        }
    };
    let task = run.task.as_deref().unwrap_or("(task unavailable)");
    let owner = run
        .owner_id
        .as_deref()
        .map(|owner| format!("\n  owner: {owner}"))
        .unwrap_or_default();
    format!(
        "• `{}` [source=memory, manageable=false, usage=unknown, memory at {}] {status}\n  task: {task}{owner}\n  note: recovered from memory only; not killable/steerable in the current runtime registry",
        run.run_id, run.last_event_at
    )
}

pub(crate) fn format_run_usage(records: &[crate::llm::route_decision::MeteredTokenUsageRecord]) -> String {
    let mut total_tokens = 0u64;
    let mut estimated_tokens = 0u64;
    let mut known_cost_usd = 0.0f64;
    let mut unknown_cost_requests = 0u64;
    for record in records {
        total_tokens = total_tokens.saturating_add(record.total_tokens);
        if record.source == crate::llm::route_decision::TokenUsageSource::Estimated {
            estimated_tokens = estimated_tokens.saturating_add(record.total_tokens);
        }
        if let Some(cost) = record.cost_usd.filter(|cost| cost.is_finite() && *cost >= 0.0) {
            known_cost_usd += cost;
        } else {
            unknown_cost_requests = unknown_cost_requests.saturating_add(1);
        }
    }
    if total_tokens == 0 {
        return "unknown".to_string();
    }
    let prefix = if estimated_tokens > 0 { "~" } else { "" };
    let mut out = format!("{prefix}{} tok", format_token_count_compact(total_tokens));
    if unknown_cost_requests > 0 {
        out.push_str(" | cost unknown");
    } else {
        out.push_str(" | ");
        out.push_str(&format_cost_usd(known_cost_usd));
    }
    out
}

fn format_token_count_compact(tokens: u64) -> String {
    if tokens >= 10_000_000 {
        format!("{}M", tokens / 1_000_000)
    } else if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn format_cost_usd(cost: f64) -> String {
    if !cost.is_finite() || cost <= 0.0 {
        "$0.0000".to_string()
    } else if cost >= 1.0 {
        format!("${cost:.2}")
    } else {
        format!("${cost:.4}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::route_decision::TokenUsageSource;
    use crate::memory::{MemoryEventInput, MemoryPrincipal, MemoryVisibility, SqliteMemory};
    use crate::tools::sessions_spawn::{SubAgentRun, SubAgentStatus};
    use chrono::Utc;

    fn make_run(id: &str, status: SubAgentStatus, task: &str) -> SubAgentRun {
        SubAgentRun {
            progress: crate::agent::idle::child_beat(),
            batch_id: None,
            id: id.to_string(),
            task: task.to_string(),
            owner_id: None,
            topic_id: None,
            source_message_event_id: None,
            started_at: Utc::now(),
            finished_at: None,
            status,
            recipient: None,
            channel_name: None,
            abort_handle: None,
            process_control: None,
            history: Arc::new(RwLock::new(Vec::new())),
            steer_tx: None,
            parent_run_id: None,
            session_scope_key: "test-session".to_string(),
            spawn_depth: 0,
            token_usage_records: Vec::new(),
        }
    }

    #[test]
    fn name_and_description() {
        let runs = Arc::new(RwLock::new(Vec::new()));
        let tool = SessionsListTool::new(runs);
        assert_eq!(tool.name(), "sessions_list");
        assert!(!tool.description().is_empty());
    }

    #[tokio::test]
    async fn empty_returns_success() {
        let runs = Arc::new(RwLock::new(Vec::new()));
        let tool = SessionsListTool::new(runs);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("No sessions"));
    }

    #[tokio::test]
    async fn lists_runs() {
        let runs = Arc::new(RwLock::new(vec![
            make_run("aaa", SubAgentStatus::Running, "task A"),
            make_run("bbb", SubAgentStatus::Completed("done".into()), "task B"),
        ]));
        let tool = SessionsListTool::new(runs);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("aaa"));
        assert!(result.output.contains("bbb"));
        assert!(result.output.contains("task A"));
    }

    /// A live run must be listable as more than "running": how long it has been
    /// silent is the difference between a healthy sub-agent and a dead one, and
    /// it is what a join has to read.
    #[tokio::test]
    async fn a_live_run_reports_how_long_it_has_been_silent() {
        let run = make_run("live", SubAgentStatus::Running, "long task");
        run.progress.record(crate::agent::idle::ProgressKind::ToolEnd);
        let runs = Arc::new(RwLock::new(vec![run]));
        let tool = SessionsListTool::new(runs);

        let result = tool.execute(json!({})).await.unwrap();

        assert!(
            result.output.contains("last progress 0s ago after 1 event(s)"),
            "{}",
            result.output
        );
    }

    /// ...and a finished run must not, because silence after a terminal status
    /// is expected and reporting it would read as a fault.
    #[tokio::test]
    async fn a_finished_run_reports_no_liveness() {
        let runs = Arc::new(RwLock::new(vec![make_run(
            "done",
            SubAgentStatus::Completed("output".into()),
            "short task",
        )]));
        let tool = SessionsListTool::new(runs);

        let result = tool.execute(json!({})).await.unwrap();

        assert!(!result.output.contains("last progress"), "{}", result.output);
    }

    /// A fan-out started by one `spawn_batch` must read as one unit: a header
    /// that answers "is it done?" and its members underneath it, even though
    /// the runs are interleaved with unrelated ones in the registry.
    #[tokio::test]
    async fn batch_members_are_grouped_under_one_header() {
        let mut first = make_run("m1", SubAgentStatus::Running, "member one");
        first.batch_id = Some("batch-42".to_string());
        let loner = make_run("solo", SubAgentStatus::Running, "unrelated");
        let mut second = make_run("m2", SubAgentStatus::Completed("done".into()), "member two");
        second.batch_id = Some("batch-42".to_string());

        let runs = Arc::new(RwLock::new(vec![first, loner, second]));
        let tool = SessionsListTool::new(runs);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);

        let output = result.output;
        assert!(
            output.contains("▣ batch `batch-42` — 2 run(s): 1 running, 1 completed, 0 failed"),
            "{output}"
        );
        let header = output.find("batch-42` \u{2014}").expect("the batch header");
        let m1 = output.find("`m1`").expect("member one is listed");
        let m2 = output.find("`m2`").expect("member two is listed");
        let solo = output.find("`solo`").expect("the unrelated run is still listed");
        // The header takes the place of the batch's first member, and the rest
        // of the batch is pulled up under it; everything else keeps its order.
        assert!(header < m1 && m1 < m2, "members must follow their header: {output}");
        assert!(
            m2 < solo,
            "an unrelated run must not be swallowed by the batch: {output}"
        );
        assert!(output.contains("batch=batch-42"), "{output}");
        let solo_line = output
            .lines()
            .find(|line| line.contains("`solo`"))
            .expect("the unrelated run is listed");
        assert!(
            !solo_line.contains("batch="),
            "a run outside any batch must not be labelled with one: {solo_line}"
        );
    }

    /// The headline count is the number of runs, not the number of lines the
    /// renderer happened to emit.
    ///
    /// Batch grouping adds a header line per fan-out. Counting those inflated
    /// the number a model reads as "how many sessions are there", and it is a
    /// number models act on: three runs reported as four invites a search for a
    /// run that does not exist, or a conclusion that a fan-out is wider than it
    /// is. Two batches here, so the inflation is two — large enough that an
    /// off-by-one elsewhere could not produce it by accident.
    #[tokio::test]
    async fn the_shown_count_is_runs_not_rendered_lines() {
        let mut first = make_run("m1", SubAgentStatus::Running, "member one");
        first.batch_id = Some("batch-a".to_string());
        let mut second = make_run("m2", SubAgentStatus::Running, "member two");
        second.batch_id = Some("batch-a".to_string());
        let mut other = make_run("m3", SubAgentStatus::Running, "member three");
        other.batch_id = Some("batch-b".to_string());
        let loner = make_run("solo", SubAgentStatus::Running, "unrelated");

        let runs = Arc::new(RwLock::new(vec![first, loner, second, other]));
        let tool = SessionsListTool::new(runs);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);

        let output = result.output;
        assert!(
            output.starts_with("Sessions (4 shown"),
            "four runs are listed, so the count must say four however they are laid out: {output}"
        );
        // And the layout really did add lines, so the assertion above is not
        // passing by there being nothing to inflate it.
        assert_eq!(
            output.matches("▣ batch").count(),
            2,
            "the two batch headers must be present: {output}"
        );
    }

    /// A list with no batches must render exactly as it always did.
    #[tokio::test]
    async fn a_list_without_batches_gains_no_grouping() {
        let runs = Arc::new(RwLock::new(vec![
            make_run("aaa", SubAgentStatus::Running, "task A"),
            make_run("bbb", SubAgentStatus::Running, "task B"),
        ]));
        let tool = SessionsListTool::new(runs);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(!result.output.contains("▣ batch"), "{}", result.output);
        assert!(!result.output.contains("batch="), "{}", result.output);
    }

    #[tokio::test]
    async fn lists_origin_and_agent_index_hint() {
        let mut run = make_run("model-run", SubAgentStatus::Running, "model task");
        run.parent_run_id = Some("turn-root".to_string());
        let runs = Arc::new(RwLock::new(vec![run]));
        let tool = SessionsListTool::new(runs);

        let result = tool.execute(json!({})).await.unwrap();

        assert!(result.success);
        assert!(
            result.output.contains("agent_index_hint=#1") && result.output.contains("origin=model"),
            "{}",
            result.output
        );
        assert!(
            result.output.contains("source=runtime, manageable=true"),
            "{}",
            result.output
        );
    }

    #[tokio::test]
    async fn lists_runtime_usage_when_reported() {
        let mut run = make_run("usage-run", SubAgentStatus::Completed("done".into()), "usage task");
        run.token_usage_records
            .push(crate::llm::route_decision::MeteredTokenUsageRecord {
                settlement_id: None,
                provider: "test-provider".to_string(),
                model: "test-model".to_string(),
                prompt_tokens: 1000,
                completion_tokens: 500,
                total_tokens: 1500,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                source: TokenUsageSource::Reported,
                cost_usd: Some(0.0042),
            });
        let runs = Arc::new(RwLock::new(vec![run]));
        let tool = SessionsListTool::new(runs);

        let result = tool.execute(json!({"status": "completed"})).await.unwrap();

        assert!(result.success);
        assert!(result.output.contains("usage=1.5k tok | $0.0042"), "{}", result.output);
    }

    #[tokio::test]
    async fn filter_running_only() {
        let runs = Arc::new(RwLock::new(vec![
            make_run("run1", SubAgentStatus::Running, "task1"),
            make_run("run2", SubAgentStatus::Completed("done".into()), "task2"),
        ]));
        let tool = SessionsListTool::new(runs);
        let result = tool.execute(json!({"status": "running"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("run1"));
        assert!(!result.output.contains("run2"));
    }

    #[tokio::test]
    async fn lists_memory_backed_runs_when_runtime_registry_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        memory
            .append_memory_event(MemoryEventInput {
                event_id: None,
                workspace_id: "/tmp".to_string(),
                event_type: "task.spawned".to_string(),
                subject_table: "tasks".to_string(),
                subject_id: "mem-run-1".to_string(),
                session_key: Some("test-session".to_string()),
                run_id: None,
                parent_run_id: None,
                agent_id: None,
                persona_id: None,
                visibility: MemoryVisibility::Workspace,
                payload_json: Some(
                    json!({
                        "task": "recover me",
                        "owner_id": "owner-a",
                        "topic_id": "topic-a"
                    })
                    .to_string(),
                ),
            })
            .await
            .unwrap();
        memory
            .append_memory_event(MemoryEventInput {
                event_id: None,
                workspace_id: "/tmp".to_string(),
                event_type: "task.completed".to_string(),
                subject_table: "tasks".to_string(),
                subject_id: "mem-run-1".to_string(),
                session_key: Some("test-session".to_string()),
                run_id: None,
                parent_run_id: None,
                agent_id: None,
                persona_id: None,
                visibility: MemoryVisibility::Workspace,
                payload_json: Some(json!({"result_preview": "done"}).to_string()),
            })
            .await
            .unwrap();

        let tool = SessionsListTool::new(Arc::new(RwLock::new(Vec::new()))).with_shared_memory(memory.clone(), "/tmp");
        let result = tool.execute(json!({"status": "completed"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("mem-run-1"));
        assert!(result.output.contains("recover me"));
        assert!(result.output.contains("owner-a"));
        assert!(
            result.output.contains("source=memory, manageable=false"),
            "{}",
            result.output
        );
        assert!(
            result
                .output
                .contains("not killable/steerable in the current runtime registry"),
            "{}",
            result.output
        );

        let visible = memory
            .list_memory_events_since(
                &MemoryPrincipal {
                    workspace_id: "/tmp".to_string(),
                    agent_id: None,
                    persona_id: None,
                    session_key: None,
                    channel: None,
                    sender: None,
                    owner_id: None,
                    legacy_session_key: None,
                },
                0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(visible.len(), 2);
    }
}
