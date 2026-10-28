#![cfg(test)]

use super::*;

// ── ProjectContext defaults ──────────────────────────────────

#[test]
fn project_context_default_is_empty() {
    let ctx = ProjectContext::default();
    assert!(ctx.user_name.is_empty(), "expected an empty value");
    assert!(ctx.timezone.is_empty(), "expected an empty value");
    assert!(ctx.agent_name.is_empty(), "expected an empty value");
    assert!(ctx.communication_style.is_empty(), "expected an empty value");
}

// ── scaffold_workspace: basic file creation ─────────────────

#[test]
fn scaffold_creates_all_md_files() {
    let tmp = TempDir::new().unwrap();
    let ctx = ProjectContext::default();
    scaffold_workspace(tmp.path(), &ctx).unwrap();

    let expected = [
        "IDENTITY.md",
        "AGENTS.md",
        "HEARTBEAT.md",
        "SOUL.md",
        "USER.md",
        "TOOLS.md",
        "BOOTSTRAP.md",
        "MEMORY.md",
        "THINKING.md",
    ];
    for f in &expected {
        assert!(tmp.path().join(f).exists(), "missing file: {f}");
    }
    let tools = fs::read_to_string(tmp.path().join("TOOLS.md")).unwrap();
    assert!(tools.contains("runtime `ToolSpec` snapshot"));
    assert!(!tools.contains("## Built-in Tools"));
}

#[test]
fn scaffold_creates_all_subdirectories() {
    let tmp = TempDir::new().unwrap();
    let ctx = ProjectContext::default();
    scaffold_workspace(tmp.path(), &ctx).unwrap();

    for dir in &["sessions", "memory", "state", "cron", "skills"] {
        assert!(tmp.path().join(dir).is_dir(), "missing subdirectory: {dir}");
    }
}

// ── scaffold_workspace: personalization ─────────────────────

#[tokio::test]
async fn scaffold_bakes_user_name_into_files() {
    let tmp = TempDir::new().unwrap();
    let ctx = ProjectContext {
        user_name: "Alice".into(),
        ..Default::default()
    };
    scaffold_workspace(tmp.path(), &ctx).unwrap();

    let user_md = tokio::fs::read_to_string(tmp.path().join("USER.md")).await.unwrap();
    assert!(user_md.contains("**Name:** Alice"), "USER.md should contain user name");

    let bootstrap = tokio::fs::read_to_string(tmp.path().join("BOOTSTRAP.md"))
        .await
        .unwrap();
    assert!(bootstrap.contains("**Alice**"), "BOOTSTRAP.md should contain user name");
}

#[tokio::test]
async fn scaffold_bakes_timezone_into_files() {
    let tmp = TempDir::new().unwrap();
    let ctx = ProjectContext {
        timezone: "US/Pacific".into(),
        ..Default::default()
    };
    scaffold_workspace(tmp.path(), &ctx).unwrap();

    let user_md = tokio::fs::read_to_string(tmp.path().join("USER.md")).await.unwrap();
    assert!(
        user_md.contains("**Timezone:** US/Pacific"),
        "USER.md should contain timezone"
    );

    let bootstrap = tokio::fs::read_to_string(tmp.path().join("BOOTSTRAP.md"))
        .await
        .unwrap();
    assert!(bootstrap.contains("US/Pacific"), "BOOTSTRAP.md should contain timezone");
}

#[tokio::test]
async fn scaffold_bakes_agent_name_into_files() {
    let tmp = TempDir::new().unwrap();
    let ctx = ProjectContext {
        agent_name: "Crabby".into(),
        ..Default::default()
    };
    scaffold_workspace(tmp.path(), &ctx).unwrap();

    let identity = tokio::fs::read_to_string(tmp.path().join("IDENTITY.md")).await.unwrap();
    assert!(
        identity.contains("**Name:** Crabby"),
        "IDENTITY.md should contain agent name"
    );

    let soul = tokio::fs::read_to_string(tmp.path().join("SOUL.md")).await.unwrap();
    assert!(soul.contains("You are **Crabby**"), "SOUL.md should contain agent name");

    let agents = tokio::fs::read_to_string(tmp.path().join("AGENTS.md")).await.unwrap();
    assert!(
        agents.contains("Crabby Personal Assistant"),
        "AGENTS.md should contain agent name"
    );

    let heartbeat = tokio::fs::read_to_string(tmp.path().join("HEARTBEAT.md"))
        .await
        .unwrap();
    assert!(heartbeat.contains("Crabby"), "HEARTBEAT.md should contain agent name");

    let bootstrap = tokio::fs::read_to_string(tmp.path().join("BOOTSTRAP.md"))
        .await
        .unwrap();
    assert!(
        bootstrap.contains("Introduce yourself as Crabby"),
        "BOOTSTRAP.md should contain agent name"
    );
}

#[tokio::test]
async fn scaffold_bakes_communication_style() {
    let tmp = TempDir::new().unwrap();
    let ctx = ProjectContext {
        communication_style: "Be technical and detailed.".into(),
        ..Default::default()
    };
    scaffold_workspace(tmp.path(), &ctx).unwrap();

    let soul = tokio::fs::read_to_string(tmp.path().join("SOUL.md")).await.unwrap();
    assert!(
        soul.contains("Be technical and detailed."),
        "SOUL.md should contain communication style"
    );

    let user_md = tokio::fs::read_to_string(tmp.path().join("USER.md")).await.unwrap();
    assert!(
        user_md.contains("Be technical and detailed."),
        "USER.md should contain communication style"
    );

    let bootstrap = tokio::fs::read_to_string(tmp.path().join("BOOTSTRAP.md"))
        .await
        .unwrap();
    assert!(
        bootstrap.contains("Be technical and detailed."),
        "BOOTSTRAP.md should contain communication style"
    );
}

// ── scaffold_workspace: defaults when context is empty ──────

#[tokio::test]
async fn scaffold_uses_defaults_for_empty_context() {
    let tmp = TempDir::new().unwrap();
    let ctx = ProjectContext::default(); // all empty
    scaffold_workspace(tmp.path(), &ctx).unwrap();

    let identity = tokio::fs::read_to_string(tmp.path().join("IDENTITY.md")).await.unwrap();
    assert!(
        identity.contains("**Name:** OpenPRX"),
        "should default agent name to OpenPRX"
    );

    let user_md = tokio::fs::read_to_string(tmp.path().join("USER.md")).await.unwrap();
    assert!(user_md.contains("**Name:** User"), "should default user name to User");
    assert!(user_md.contains("**Timezone:** UTC"), "should default timezone to UTC");

    let soul = tokio::fs::read_to_string(tmp.path().join("SOUL.md")).await.unwrap();
    assert!(
        soul.contains("Be warm, natural, and clear."),
        "should default communication style"
    );
}

// ── scaffold_workspace: skip existing files ─────────────────

#[tokio::test]
async fn scaffold_does_not_overwrite_existing_files() {
    let tmp = TempDir::new().unwrap();
    let ctx = ProjectContext {
        user_name: "Bob".into(),
        ..Default::default()
    };

    // Pre-create SOUL.md with custom content
    let soul_path = tmp.path().join("SOUL.md");
    fs::write(&soul_path, "# My Custom Soul\nDo not overwrite me.").unwrap();

    scaffold_workspace(tmp.path(), &ctx).unwrap();

    // SOUL.md should be untouched
    let soul = tokio::fs::read_to_string(&soul_path).await.unwrap();
    assert!(
        soul.contains("Do not overwrite me"),
        "existing files should not be overwritten"
    );
    assert!(
        !soul.contains("You're not a chatbot"),
        "should not contain scaffold content"
    );

    // But USER.md should be created fresh
    let user_md = tokio::fs::read_to_string(tmp.path().join("USER.md")).await.unwrap();
    assert!(user_md.contains("**Name:** Bob"));
}

// ── scaffold_workspace: idempotent ──────────────────────────

#[tokio::test]
async fn scaffold_is_idempotent() {
    let tmp = TempDir::new().unwrap();
    let ctx = ProjectContext {
        user_name: "Eve".into(),
        agent_name: "Claw".into(),
        ..Default::default()
    };

    scaffold_workspace(tmp.path(), &ctx).unwrap();
    let soul_v1 = tokio::fs::read_to_string(tmp.path().join("SOUL.md")).await.unwrap();

    // Run again — should not change anything
    scaffold_workspace(tmp.path(), &ctx).unwrap();
    let soul_v2 = tokio::fs::read_to_string(tmp.path().join("SOUL.md")).await.unwrap();

    assert_eq!(soul_v1, soul_v2, "scaffold should be idempotent");
}

// ── scaffold_workspace: all files are non-empty ─────────────

#[tokio::test]
async fn scaffold_files_are_non_empty() {
    let tmp = TempDir::new().unwrap();
    let ctx = ProjectContext::default();
    scaffold_workspace(tmp.path(), &ctx).unwrap();

    for f in &[
        "IDENTITY.md",
        "AGENTS.md",
        "HEARTBEAT.md",
        "SOUL.md",
        "USER.md",
        "TOOLS.md",
        "BOOTSTRAP.md",
        "MEMORY.md",
        "THINKING.md",
    ] {
        let content = tokio::fs::read_to_string(tmp.path().join(f)).await.unwrap();
        assert!(!content.trim().is_empty(), "{f} should not be empty");
    }
}

// ── scaffold_workspace: AGENTS.md references on-demand memory

#[tokio::test]
async fn agents_md_references_on_demand_memory() {
    let tmp = TempDir::new().unwrap();
    let ctx = ProjectContext::default();
    scaffold_workspace(tmp.path(), &ctx).unwrap();

    let agents = tokio::fs::read_to_string(tmp.path().join("AGENTS.md")).await.unwrap();
    assert!(
        agents.contains("memory_recall"),
        "AGENTS.md should reference memory_recall for on-demand access"
    );
    assert!(
        agents.contains("on-demand"),
        "AGENTS.md should mention daily notes are on-demand"
    );
}

// ── scaffold_workspace: MEMORY.md warns about token cost ────

#[tokio::test]
async fn memory_md_warns_about_token_cost() {
    let tmp = TempDir::new().unwrap();
    let ctx = ProjectContext::default();
    scaffold_workspace(tmp.path(), &ctx).unwrap();

    let memory = tokio::fs::read_to_string(tmp.path().join("MEMORY.md")).await.unwrap();
    assert!(
        memory.contains("costs tokens"),
        "MEMORY.md should warn about token cost"
    );
    assert!(
        memory.contains("auto-injected"),
        "MEMORY.md should mention it's auto-injected"
    );
}

// ── scaffold_workspace: TOOLS.md remains environment-local ───

#[tokio::test]
async fn tools_md_defers_runtime_catalog_to_tool_spec_snapshot() {
    let tmp = TempDir::new().unwrap();
    let ctx = ProjectContext::default();
    scaffold_workspace(tmp.path(), &ctx).unwrap();

    let tools = tokio::fs::read_to_string(tmp.path().join("TOOLS.md")).await.unwrap();
    assert!(tools.contains("runtime `ToolSpec` snapshot"));
    assert!(tools.contains("environment-specific"));
    assert!(!tools.contains("## Built-in Tools"));
    assert!(!tools.contains("**shell**"));
}

#[tokio::test]
async fn soul_md_includes_emoji_awareness_guidance() {
    let tmp = TempDir::new().unwrap();
    let ctx = ProjectContext::default();
    scaffold_workspace(tmp.path(), &ctx).unwrap();

    let soul = tokio::fs::read_to_string(tmp.path().join("SOUL.md")).await.unwrap();
    assert!(
        soul.contains("Use emojis naturally (0-2 max"),
        "SOUL.md should include emoji usage guidance"
    );
    assert!(
        soul.contains("Match emoji density to the user"),
        "SOUL.md should include emoji-awareness guidance"
    );
}

// ── scaffold_workspace: special characters in names ─────────

#[tokio::test]
async fn scaffold_handles_special_characters_in_names() {
    let tmp = TempDir::new().unwrap();
    let ctx = ProjectContext {
        user_name: "José María".into(),
        agent_name: "OpenPRX-v2".into(),
        timezone: "Europe/Madrid".into(),
        communication_style: "Be direct.".into(),
    };
    scaffold_workspace(tmp.path(), &ctx).unwrap();

    let user_md = tokio::fs::read_to_string(tmp.path().join("USER.md")).await.unwrap();
    assert!(user_md.contains("José María"));

    let soul = tokio::fs::read_to_string(tmp.path().join("SOUL.md")).await.unwrap();
    assert!(soul.contains("OpenPRX-v2"));
}

// ── scaffold_workspace: full personalization round-trip ─────

#[tokio::test]
async fn scaffold_full_personalization() {
    let tmp = TempDir::new().unwrap();
    let ctx = ProjectContext {
            user_name: "Argenis".into(),
            timezone: "US/Eastern".into(),
            agent_name: "Claw".into(),
            communication_style:
                "Be friendly, human, and conversational. Show warmth and empathy while staying efficient. Use natural contractions."
                    .into(),
        };
    scaffold_workspace(tmp.path(), &ctx).unwrap();

    // Verify every file got personalized
    let identity = tokio::fs::read_to_string(tmp.path().join("IDENTITY.md")).await.unwrap();
    assert!(identity.contains("**Name:** Claw"));

    let soul = tokio::fs::read_to_string(tmp.path().join("SOUL.md")).await.unwrap();
    assert!(soul.contains("You are **Claw**"));
    assert!(soul.contains("Be friendly, human, and conversational"));

    let user_md = tokio::fs::read_to_string(tmp.path().join("USER.md")).await.unwrap();
    assert!(user_md.contains("**Name:** Argenis"));
    assert!(user_md.contains("**Timezone:** US/Eastern"));
    assert!(user_md.contains("Be friendly, human, and conversational"));

    let agents = tokio::fs::read_to_string(tmp.path().join("AGENTS.md")).await.unwrap();
    assert!(agents.contains("Claw Personal Assistant"));

    let bootstrap = tokio::fs::read_to_string(tmp.path().join("BOOTSTRAP.md"))
        .await
        .unwrap();
    assert!(bootstrap.contains("**Argenis**"));
    assert!(bootstrap.contains("US/Eastern"));
    assert!(bootstrap.contains("Introduce yourself as Claw"));

    let heartbeat = tokio::fs::read_to_string(tmp.path().join("HEARTBEAT.md"))
        .await
        .unwrap();
    assert!(heartbeat.contains("Claw"));
}
