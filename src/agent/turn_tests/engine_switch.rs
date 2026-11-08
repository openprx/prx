#![cfg(test)]
//! Surface ownership with the decision engine off. Card 03 adds enabled-engine cells.
use super::*;
use crate::agent::turn_tests::admission::AdmittedTurn;
use crate::tools::{DispatchOrigin, ToolExecutionCommand, ToolExecutionStatus};

fn channel_message() -> crate::channels::traits::ChannelMessage {
    crate::channels::traits::ChannelMessage {
        id: "prx-channel-surface".to_owned(),
        sender: "prx_user".to_owned(),
        reply_target: "prx_channel".to_owned(),
        content: "run the project tool".to_owned(),
        channel: "wacli".to_owned(),
        timestamp: 0,
        chat_kind: crate::channels::traits::ChatKind::Dm,
        chat_title: None,
        sender_display: None,
        thread_ts: None,
        mentioned_uuids: Vec::new(),
        mentioned: false,
        is_group_hint: false,
        sender_is_bot: false,
    }
}

/// Engine disabled: a channel turn does not publish a channel-excluded tool,
/// and dispatch of that name is Denied with no backend call.
#[tokio::test]
async fn tool_surface_is_built_without_decision_service() {
    let workspace = tempfile::tempdir().expect("channel workspace");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_owned();
    assert!(
        !config.decision_engine.enabled,
        "this cell runs with the decision engine off"
    );
    config.tool_tiering.channel_exclude.push("project_tool".into());
    let dispatches = Arc::new(Mutex::new(Vec::new()));
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(RecordingTool {
        name: "project_tool".to_owned(),
        execution_order: Arc::clone(&dispatches),
    }) as Box<dyn Tool>]);
    let admitted = AdmittedTurn::with_channel(config, channel_message(), registry).await;
    let decision = admitted.decision();
    let surface = crate::tools::surface_owner::build(decision.service.turn_surface_parts(decision.proof, &[]));
    assert!(!surface.contains("project_tool"), "channel-excluded tool was published");
    let adapter = admitted.adapter();
    let service = adapter
        .tool_execution_service
        .clone()
        .expect("fixture execution service");
    let mut context = adapter.tool_execution_context.clone();
    context.dispatch_origin = DispatchOrigin::ModelTurn;
    context.turn_surface = Some(Arc::new(surface));
    let outcome = service
        .execute(
            ToolExecutionCommand::new("project_tool", serde_json::json!({})),
            context,
            None,
        )
        .await;
    assert_eq!(outcome.status, ToolExecutionStatus::Denied);
    assert!(dispatches.lock().expect("dispatch log").is_empty());
}
