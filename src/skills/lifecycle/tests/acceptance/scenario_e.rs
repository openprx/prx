//! Scenario E: verify through real entrypoints, up to the boundary.
//!
//! What runs here is the single-shot agent entry (`prx agent -m`, i.e.
//! `crate::agent::run`) and the `prx skills lifecycle` CLI handler, both against
//! a loopback model on 127.0.0.1. The evidence the spec asks for is captured and
//! checked: the selected revision, the actual hydration, the tool arguments and
//! results, the terminal receipt and a harmless verifiable side effect.
//!
//! `blocked-external`, not covered and not faked:
//! * the learning claim (spec 11.1, invariant 11) needs an authorized actual
//!   managed-model run, and a paid hosted call or a local managed model
//!   endpoint is outside the current boundary. Everything here uses a fixture
//!   model and a scripted proposer, so nothing in this file shows that PRX
//!   learns;
//! * real fullscreen `prx chat`, Gateway, configured channels, session-worker
//!   and Delegate surfaces, which need the real binary, credentials, real IM or
//!   the current worker/browser blockers.

use super::flows::{capture_failed_tasks, propose, staged};
use super::support::{ScriptedProposer, World, consolidate, notes_suite, nth, replace_patch, skill, skill_text};
use crate::SkillLifecycleCommands;
use crate::config::skills_lifecycle::SkillsLifecycleModeConfig;
use crate::skills::lifecycle::{Adherence, ExperienceOutcome, ProposalStatus};
use sha2::{Digest, Sha256};
use std::sync::Arc;

#[tokio::test]
async fn the_agent_entry_leaves_the_selection_hydration_tool_terminal_and_side_effect_evidence() {
    let world = World::new().await;
    let text = skill_text(&[("hotfix", "hotfix-v1"), ("report", "report-v1")]);
    let revision = world.onboard(&text);
    let suite = notes_suite(&world.owner, "changelog-v2", "hotfix-v1");
    let mut config = world.production_config(SkillsLifecycleModeConfig::Auto, &[(&skill(), &suite)]);
    config.autonomy.level = crate::security::AutonomyLevel::Full;
    config.tool_tiering.always_include = ["skill_read", "file_read", "file_write"].map(str::to_string).to_vec();

    let answer = world
        .run_task(&config, "TASK[report] Write the release notes report for build 5.")
        .await
        .expect("test: report run");
    assert_eq!(answer, "FINAL:report-v1");

    // The harmless side effect, through the real tool.
    let written = std::fs::read_to_string(world.ws.join("report.txt")).expect("test: report.txt was written");
    assert_eq!(written, "REPORT:report-v1");

    // Tool arguments and results as the model saw them on the wire.
    {
        let bodies = world.loopback.bodies.lock();
        let last = bodies.last().expect("test: a request");
        assert!(
            last.contains("\\\"name\\\":\\\"release-notes\\\""),
            "skill_read was called with the skill name"
        );
        assert!(last.contains("tool_result name=\\\"skill_read"), "its result came back");
        assert!(
            last.contains("RULE report => report-v1"),
            "the pinned revision's text was served"
        );
        assert!(
            last.contains("tool_result name=\\\"file_write"),
            "the write result came back"
        );
        assert!(
            last.contains("report.txt") && last.contains("REPORT:report-v1"),
            "its arguments were recorded"
        );
    }

    // The runtime receipts of that run.
    let runs = world.production_runs(1).await;
    let run = nth(&runs, 0);
    assert_eq!(run.hydration_receipts.len(), 1);
    let hydration = nth(&run.hydration_receipts, 0);
    assert_eq!(hydration.revision_id, revision);
    assert_eq!(hydration.generation, 1);
    assert_eq!(hydration.resource_path, "SKILL.md");
    assert_eq!(
        hydration.resource_sha256,
        hex::encode(Sha256::digest(text.as_bytes())),
        "the hash of the bytes served"
    );
    let terminal = run.terminal.as_ref().expect("test: terminal joined");
    assert_eq!(terminal.outcome, ExperienceOutcome::Success);
    assert_ne!(terminal.terminal_receipt_id, "");
    assert_eq!(
        run.correlation.run_id.as_deref(),
        Some(terminal.terminal_receipt_id.as_str()),
        "terminal and run correlate"
    );
    assert_eq!(terminal.facts.adherence_of(&skill(), &revision), Adherence::Followed);
    assert_eq!((run.dropped_evidence, run.late_evidence), (0, 0));
    // The selection receipt is written off the turn path and follows retrieval;
    // when it exists it names the same revision.
    assert!(run.selection_receipts.iter().all(|s| s.revision_id == revision));
}

#[tokio::test]
async fn the_cli_refuses_to_publish_in_draft_only_and_leaves_the_binding_alone() {
    let world = World::new().await;
    let a = world.onboard(&skill_text(&[("hotfix", "hotfix-v1")]));
    let suite = notes_suite(&world.owner, "changelog-v2", "hotfix-v1");
    let draft = world.production_config(SkillsLifecycleModeConfig::DraftOnly, &[(&skill(), &suite)]);
    let service = world.service(&draft);
    let evidence = capture_failed_tasks(&world, &draft, "no-rule", 2).await;
    let pattern = consolidate(&world, "The skill has no rule for release notes.", &evidence);
    let proposer = ScriptedProposer::new(vec![replace_patch(
        "add the release rule",
        "RULES-END",
        "RULE release => changelog-v2\nRULES-END",
    )]);
    let proposal = staged(propose(&world, "req-e", &pattern, &proposer).await);
    assert_eq!(proposal.status, ProposalStatus::Draft);

    let run = |command: SkillLifecycleCommands| {
        let (service, config) = (Arc::clone(&service), draft.clone());
        async move { crate::skills::lifecycle_cli::execute(command, Some(service), &config).await }
    };
    let status = run(SkillLifecycleCommands::Status).await.expect("test: status");
    assert!(status.contains("mode: draft_only"), "{status}");
    assert!(status.contains("Publication: blocked"), "{status}");
    let refused = run(SkillLifecycleCommands::Publish {
        proposal_id: proposal.proposal_id.clone(),
    })
    .await
    .expect_err("test: draft-only must not publish");
    assert!(
        refused.to_string().contains("draft_only does not allow publishing"),
        "{refused}"
    );
    let binding = world
        .store
        .active_binding(&skill())
        .expect("test: binding")
        .expect("test: bound");
    assert_eq!((binding.revision_id, binding.generation), (a, 1));
    let listed = run(SkillLifecycleCommands::List { skill: None })
        .await
        .expect("test: list");
    assert!(
        listed.contains(&proposal.proposal_id) && listed.contains("draft"),
        "{listed}"
    );
}
