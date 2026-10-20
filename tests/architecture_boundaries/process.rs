use super::helpers::{
    assert_inventory, enclosing_function, function_body, normalize_line, production_part, read_source, relative_path,
    repository_root, rust_source_files,
};
use std::fs;
const ALLOWED_RAW_CHILD_PROCESS_SPAWNS: &[&str] = &[
    // Fresh-process fixture: exact current test executable, owned acknowledgement and reap.
    "src/runtime/ingress_recovery/tests.rs::start::.spawn()",
    "src/channels/signal_native.rs::health_check::let Ok(_child) = cmd.spawn() else {",
    "src/channels/signal_native.rs::listen::.spawn()",
    "src/chat/views.rs::run_git_diff_bounded::.spawn()",
    "src/chat/sessions/shell.rs::spawn_shell_with_origin::.spawn()",
    "src/chat/terminal_proto.rs::copy_to_tmux_buffer::.spawn()?;",
    "src/media/mod.rs::run_command_bounded::.spawn()",
    // Test-only (b1): process-mode liveness is only proved against a real
    // child. One is externally SIGKILLed so the parent sees nothing but a
    // closed pipe (the OOM-kill shape); the other emits output in instalments
    // so the drain can be observed refreshing the run's progress beat while the
    // child is still alive. A managed spawn would hide both.
    "src/tools/sessions_spawn/tests/liveness.rs::a_sigkilled_worker_is_classified_as_exited_without_result::let mut child = command.spawn().expect(\"stand-in worker spawns\");",
    // b2: a join must survive a member that is a real OS process emitting
    // nothing at all; only a real child can prove the silence is real.
    "src/tools/sessions_spawn/tests/join.rs::a_join_survives_a_real_child_process_that_never_writes_anything::let mut child = command.spawn().expect(\"stand-in worker spawns\");",
    // Test-only (b3): the wording that separates `no_result` from `failed` must
    // be derived from a genuinely SIGKILLed child through the production
    // classifier, not typed into the test — a managed spawn would supply an
    // exit status the classifier never sees in the fault this models.
    "src/tools/sessions_spawn/tests/join_verdicts.rs::silent_death_reason_from_a_real_sigkill::.spawn()",
    "src/tools/sessions_spawn/tests/liveness.rs::worker_output_refreshes_the_run_beat_while_it_is_still_arriving::let mut child = command.spawn().expect(\"stand-in worker spawns\");",
    "src/tools/sessions_spawn/tests/owned_child.rs::assert_explicit_cleanup_signals_once::let mut child = command.spawn().unwrap();",
    "src/tools/sessions_spawn/tests/owned_child.rs::injected_panic_cleanup_try_wait_error_keeps_owner_pending::let child = command.spawn().unwrap();",
    "src/tools/sessions_spawn/tests/owned_child.rs::injected_termination_wait_error_keeps_owner_pending::let child = command.spawn().unwrap();",
    "src/tools/sessions_spawn/tests/owned_child.rs::injected_wait_error_keeps_child_finalization_and_slot_pending::let child = command.spawn().unwrap();",
    "src/tools/sessions_spawn/tests/owned_child.rs::owned_child_panic_cleanup_signals_once_and_reaps_before_return::let mut child = command.spawn().unwrap();",
    "src/tools/sessions_spawn/tests/owned_child.rs::owner_keeps_child_after_requester_timeout_until_reap::let child = command.spawn().unwrap();",
    "src/tools/sessions_spawn/tests/owned_child.rs::owner_mediated_process_kill_reaps_leader_and_terminates_group::let child = command.spawn().expect(\"test child should spawn\");",
    "src/tools/sessions_spawn/tests/owned_child.rs::process_mode_waits_for_natural_completion_without_a_deadline::let mut child = command.spawn().unwrap();",
    "src/tools/sessions_spawn/process_mode.rs::run_sub_agent_process::let mut child = command.spawn()?;",
    // Test-only: the steer wire is only proved by a real pipe to a real child,
    // so these two stand in for a session-worker (one echoes stdin, one never
    // reads it) rather than mocking the very pipe under test.
    "src/tools/sessions_spawn/tests/actions.rs::steer_frames_reach_worker_stdin_and_land_in_history::let mut child = command.spawn().expect(\"stand-in worker spawns\");",
    "src/tools/sessions_spawn/tests/actions.rs::steer_pump_backpressures_when_the_worker_never_reads_stdin::let mut child = command.spawn().expect(\"stand-in worker spawns\");",
    "src/tools/sessions_spawn/tests/owned_child.rs::termination_after_leader_exit_does_not_wait_forever_on_inherited_pipe::let mut child = command.spawn().expect(\"test leader should spawn\");",
    // Test-only: the orphan-reclaim test must create a child that deliberately
    // bypasses the managed path, otherwise there is no orphan to detect.
    "src/runtime/registry.rs::unreaped_child_is_reported_as_an_orphan_then_reclaimed::let mut child = command.spawn().expect(\"test: spawn detached child\");",
    // Test-only: proving the hang detector cleans up through `killpg` requires a
    // child that no RAII path can reclaim, so the managed spawn (which kills on
    // drop) would hide the very thing under test.
    "src/agent/idle.rs::hung_turn_is_terminated_and_its_child_process_group_is_killed::let child = command.spawn().unwrap();",
    "src/runtime/shell_process.rs::spawn_managed_shell_child::let child = cmd.spawn()?;",
    // MCP stdio transports must hand the raw child pipes to rmcp. Both sites
    // create a process group and register the child in the shared runtime
    // ledger; discovery closes immediately and call sessions close on reset.
    "src/tools/mcp.rs::call_stdio::let (transport, _stderr) = TokioChildProcess::builder(cmd).stderr(Stdio::null()).spawn()?;",
    "src/tools/mcp.rs::discover_server_tools_stdio::let (transport, _stderr) = TokioChildProcess::builder(cmd).stderr(Stdio::null()).spawn()?;",
    // Test-only: proving the skill publication generation CAS is process-safe
    // needs real competing processes (the test binary re-executed), and proving
    // the lock is released when its holder dies needs a child that is SIGKILLed.
    "src/skills/lifecycle/tests/publish.rs::a_killed_lock_holder_releases_the_lock_and_publication_continues::.spawn()",
    "src/skills/lifecycle/tests/publish.rs::processes_racing_on_one_generation_have_exactly_one_winner::.spawn()",
    "src/tunnel/cloudflare.rs::start::.spawn()?;",
    "src/tunnel/custom.rs::start::.spawn()?;",
    "src/tunnel/mod.rs::kill_shared_terminates_and_clears_child::.spawn()",
    "src/tunnel/ngrok.rs::start::.spawn()?;",
    "src/tunnel/tailscale.rs::start::.spawn()?;",
];

#[test]
fn raw_child_process_spawns_are_explicitly_allowlisted() {
    let mut actual = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let source = read_source(&path);
        let lines = source.lines().collect::<Vec<_>>();
        for (index, line) in lines.iter().enumerate() {
            if !line.contains(".spawn(") {
                continue;
            }
            let function_start = (0..=index)
                .rev()
                .find(|candidate| lines[*candidate].contains("fn "))
                .unwrap_or(0);
            let prefix = lines[function_start..=index].join("\n");
            let is_process_spawn = prefix.contains("Command::new")
                || prefix.contains("process::Command")
                // The hardened git launch (GIT-ENV) hands back a command.
                || prefix.contains("git_launch::git_command(")
                || prefix.contains(".build_command()")
                || line.contains("cmd.spawn(")
                || line.contains("command.spawn(")
                // An rmcp child-process transport spawns whatever command it
                // is handed, wherever that command was built.
                || line.contains("TokioChildProcess::builder(");
            let is_thread_spawn = prefix.contains("std::thread::Builder::new") && !prefix.contains("process::Command");
            if is_process_spawn && !is_thread_spawn {
                actual.push(format!(
                    "{relative}::{}::{}",
                    enclosing_function(&lines, index),
                    normalize_line(line)
                ));
            }
        }
    }
    assert_inventory(
        "raw child-process spawn sites",
        actual,
        ALLOWED_RAW_CHILD_PROCESS_SPAWNS,
    );
}

#[test]
fn shell_entrypoints_delegate_process_execution_to_shared_adapter() {
    for (relative, entrypoint, execution_helper) in [
        ("src/tools/shell.rs", "execute_inner", "execute_inner"),
        (
            "src/cron/scheduler.rs",
            "run_job_command",
            "run_job_command_with_adapter",
        ),
        (
            "src/xin/runner.rs",
            "run_shell_with_cancellation",
            "run_shell_with_adapter",
        ),
    ] {
        let source = fs::read_to_string(repository_root().join(relative))
            .unwrap_or_else(|error| panic!("read {relative}: {error}"));
        let entrypoint_body = function_body(&source, entrypoint);
        let helper_body = function_body(&source, execution_helper);
        if entrypoint != execution_helper {
            let helper_call_marker = format!("{execution_helper}(");
            assert!(
                entrypoint_body.contains(&helper_call_marker),
                "{relative}::{entrypoint} must call the inspected helper {execution_helper}"
            );
        }
        assert!(
            helper_body.contains(".execute(") && helper_body.contains("ShellProcessRequest"),
            "{relative}::{execution_helper} must execute a ShellProcessRequest through ShellProcessAdapter"
        );
        let inspected_chain = format!("{entrypoint_body}\n{helper_body}");
        for forbidden in [
            "tokio::process::Command",
            "std::process::Command",
            "Command::new",
            ".spawn(",
            ".output(",
            ".status(",
        ] {
            assert!(
                !inspected_chain.contains(forbidden),
                "{relative}::{entrypoint}->{execution_helper} reintroduced raw process execution via {forbidden}"
            );
        }
    }
}

/// Every workspace command hook starts from the cleared launch environment
/// in its dedicated directory, and only behind the operator's opt-in
/// (HOOKS-ENV): the one runner clears the environment, builds it through the
/// shared launch builder and pins the working directory, and `emit` checks
/// the policy before running any action.
#[test]
fn workspace_hooks_launch_through_the_shared_builder_behind_the_opt_in() {
    let source = fs::read_to_string(repository_root().join("src/hooks/mod.rs"))
        .unwrap_or_else(|error| panic!("read src/hooks/mod.rs: {error}"));
    let production = production_part(&source);
    let runner = function_body(&production, "run_action");
    for required in [
        "cmd.env_clear();",
        "mcp_launch::workspace_launch_environment(",
        "cmd.current_dir(&working_dir);",
        "mcp_launch::prepare_launch_directory(",
    ] {
        assert!(runner.contains(required), "run_action must call {required}");
    }
    assert_eq!(
        production.matches("Command::new(").count(),
        1,
        "src/hooks/mod.rs builds a process command outside run_action"
    );
    // `emit` only queues; the background `deliver` runs the hooks and checks
    // the opt-in at delivery time.
    let deliver = function_body(&production, "deliver");
    assert!(
        deliver.contains("self.workspace_policy()"),
        "deliver must check the opt-in"
    );
}

/// Finding 96: hook events are queued, so every owner of a live
/// `HookManager` drains it on its exit path or loses the last events.
#[test]
fn every_live_hook_manager_owner_drains_at_shutdown() {
    for path in [
        "src/gateway/mod.rs",
        "src/channels/startup.rs",
        "src/chat/run.rs",
        "src/agent/entry.rs",
        "src/session_worker/runner/run.rs",
    ] {
        let source =
            fs::read_to_string(repository_root().join(path)).unwrap_or_else(|error| panic!("read {path}: {error}"));
        assert!(
            production_part(&source).contains("drain_for_shutdown()"),
            "{path} owns a live HookManager and must call drain_for_shutdown() on its exit path"
        );
    }
}

/// GIT-ENV F-6 (D-103): the production places that name the `git`
/// program (`"git"`, `"git.exe"`, or a path ending in one), as
/// `file::function::literal`. Only the hardened launch starts git; the rest
/// name it without starting it (a shell-command classifier, the intent
/// catalogue, the doctor's `git --version` probe, a protocol name).
const ALLOWED_GIT_PROGRAM_LITERALS: &[&str] = &[
    "src/doctor/mod.rs::check_environment::git",
    "src/doctor/mod.rs::check_unowned_shell_jobs::git",
    "src/security/git_launch.rs::command_fallback::git",
    "src/security/git_launch.rs::git_command_with_parent::git",
    "src/security/git_launch.rs::program_is_git::git",
    "src/security/git_launch.rs::read_git::git",
    "src/security/policy.rs::command_risk_level::git",
    "src/security/policy.rs::is_args_safe::git",
];

/// GIT-ENV F-6 (D-103): every production site that builds a process
/// command, as `file::function::line`. A new one, whatever the program
/// expression (a constant, a variable, a path, `sh -c`), is reviewed here:
/// git must go through `security::git_launch::git_command`.
const ALLOWED_PROCESS_COMMANDS: &[&str] = &[
    "src/channels/imessage.rs::send::let output = tokio::process::Command::new(\"osascript\")",
    "src/channels/service.rs::maybe_restart_managed_daemon_service::if let Ok(status_output) = Command::new(\"rc-service\")",
    "src/channels/service.rs::maybe_restart_managed_daemon_service::let _ = Command::new(\"launchctl\")",
    "src/channels/service.rs::maybe_restart_managed_daemon_service::let active_output = Command::new(\"systemctl\")",
    "src/channels/service.rs::maybe_restart_managed_daemon_service::let list_output = Command::new(\"launchctl\")",
    "src/channels/service.rs::maybe_restart_managed_daemon_service::let restart_output = Command::new(\"rc-service\")",
    "src/channels/service.rs::maybe_restart_managed_daemon_service::let restart_output = Command::new(\"systemctl\")",
    "src/channels/service.rs::maybe_restart_managed_daemon_service::let start_output = Command::new(\"launchctl\")",
    "src/channels/signal.rs::run_command::let output = tokio::process::Command::new(cmd)",
    "src/channels/signal_native.rs::build_command::let mut cmd = Command::new(&self.cli_path);",
    "src/channels/wacli.rs::health_check::let mut cmd = tokio::process::Command::new(self.cli_binary());",
    "src/channels/wacli.rs::send_prepared_media::let mut cmd = tokio::process::Command::new(self.cli_binary());",
    "src/channels/wacli.rs::send_text::let mut cmd = tokio::process::Command::new(self.cli_binary());",
    "src/channels/wacli.rs::set_presence::let mut cmd = tokio::process::Command::new(self.cli_binary());",
    "src/chat/external_editor.rs::run_external_editor_command::std::process::Command::new(\"cmd\")",
    "src/chat/external_editor.rs::run_external_editor_command::std::process::Command::new(\"sh\")",
    "src/chat/sessions/shell.rs::spawn_shell_with_origin::let mut cmd = tokio::process::Command::new(\"sh\");",
    "src/chat/terminal_proto.rs::copy_to_tmux_buffer::let mut child = Command::new(\"tmux\")",
    "src/doctor/mod.rs::check_command_available::match std::process::Command::new(cmd)",
    "src/doctor/mod.rs::disk_available_mb::let output = std::process::Command::new(\"df\").arg(\"-m\").arg(path).output().ok()?;",
    "src/hooks/mod.rs::run_action::let mut cmd = Command::new(&program);",
    "src/media/mod.rs::run_command_bounded::let mut child = tokio::process::Command::new(command)",
    "src/memory/lucid.rs::run_lucid_command_raw::let mut cmd = Command::new(lucid_cmd);",
    "src/nodes/server.rs::execute_shell_command::let mut cmd = Command::new(&parsed_command.program);",
    "src/runtime/docker.rs::build_command::let mut process = tokio::process::Command::new(\"docker\");",
    "src/runtime/native.rs::build_shell_command::let mut process = tokio::process::Command::new(\"sh\");",
    "src/security/git_launch.rs::git_command_with_parent::let mut command = tokio::process::Command::new(\"git\");",
    "src/security/git_launch.rs::read_git::let mut command = tokio::process::Command::new(\"git\");",
    "src/security/secrets.rs::load_or_create_key::match std::process::Command::new(\"icacls\")",
    "src/service/mod.rs::check_openprx_user::let output = Command::new(\"getent\")",
    "src/service/mod.rs::chown_recursive_to_openprx::let output = Command::new(\"chown\")",
    "src/service/mod.rs::chown_to_openprx::let output = Command::new(\"chown\")",
    "src/service/mod.rs::ensure_openprx_user::let group_output = Command::new(\"getent\")",
    "src/service/mod.rs::ensure_openprx_user::let output = Command::new(\"addgroup\")",
    "src/service/mod.rs::ensure_openprx_user::let output = Command::new(\"adduser\")",
    "src/service/mod.rs::ensure_openprx_user::let output = Command::new(\"getent\")",
    "src/service/mod.rs::ensure_openprx_user::let output = Command::new(\"useradd\")",
    "src/service/mod.rs::ensure_openrc_runtime_path_writable::let output = Command::new(&program)",
    "src/service/mod.rs::install_linux_openrc::run_checked(Command::new(\"rc-update\").args([\"add\", \"prx\", \"default\"])).await?;",
    "src/service/mod.rs::install_linux_systemd::run_checked(Command::new(\"systemctl\").args([\"--user\", \"daemon-reload\"])).await?;",
    "src/service/mod.rs::install_linux_systemd::run_checked(Command::new(\"systemctl\").args([\"--user\", \"enable\", \"prx.service\"])).await?;",
    "src/service/mod.rs::install_windows::run_checked(Command::new(\"schtasks\").args([",
    "src/service/mod.rs::preferred_user_config_dir::if let Ok(output) = Command::new(\"getent\")",
    "src/service/mod.rs::query_status::let output = run_capture_status(Command::new(\"launchctl\").arg(\"list\")).await?;",
    "src/service/mod.rs::query_status::run_capture_status(Command::new(\"schtasks\").args([\"/Query\", \"/TN\", windows_task_name(), \"/FO\", \"LIST\"]))",
    "src/service/mod.rs::query_status_linux::let output = run_capture_status(Command::new(\"rc-service\").args([\"prx\", \"status\"])).await?;",
    "src/service/mod.rs::query_status_linux::run_capture_status(Command::new(\"systemctl\").args([\"--user\", \"is-active\", \"prx.service\"])).await?;",
    "src/service/mod.rs::restart_linux::run_checked(Command::new(\"rc-service\").args([\"prx\", \"restart\"])).await?;",
    "src/service/mod.rs::restart_linux::run_checked(Command::new(\"systemctl\").args([\"--user\", \"daemon-reload\"])).await?;",
    "src/service/mod.rs::restart_linux::run_checked(Command::new(\"systemctl\").args([\"--user\", \"restart\", \"prx.service\"])).await?;",
    "src/service/mod.rs::start::run_checked(Command::new(\"launchctl\").arg(\"load\").arg(\"-w\").arg(&plist)).await?;",
    "src/service/mod.rs::start::run_checked(Command::new(\"launchctl\").arg(\"start\").arg(SERVICE_LABEL)).await?;",
    "src/service/mod.rs::start::run_checked(Command::new(\"schtasks\").args([\"/Run\", \"/TN\", windows_task_name()])).await?;",
    "src/service/mod.rs::start_linux::run_checked(Command::new(\"rc-service\").args([\"prx\", \"start\"])).await?;",
    "src/service/mod.rs::start_linux::run_checked(Command::new(\"systemctl\").args([\"--user\", \"daemon-reload\"])).await?;",
    "src/service/mod.rs::start_linux::run_checked(Command::new(\"systemctl\").args([\"--user\", \"start\", \"prx.service\"])).await?;",
    "src/service/mod.rs::stop::run_checked(Command::new(\"launchctl\").arg(\"stop\").arg(SERVICE_LABEL)).await?;",
    "src/service/mod.rs::stop::run_checked(Command::new(\"launchctl\").arg(\"unload\").arg(\"-w\").arg(&plist)).await?;",
    "src/service/mod.rs::stop::run_checked(Command::new(\"schtasks\").args([\"/End\", \"/TN\", task_name])).await?;",
    "src/service/mod.rs::stop_linux::run_checked(Command::new(\"rc-service\").args([\"prx\", \"stop\"])).await?;",
    "src/service/mod.rs::stop_linux::run_checked(Command::new(\"systemctl\").args([\"--user\", \"stop\", \"prx.service\"])).await?;",
    "src/service/mod.rs::uninstall::run_checked(Command::new(\"schtasks\").args([\"/Delete\", \"/TN\", task_name, \"/F\"])).await?;",
    "src/service/mod.rs::uninstall_linux::run_checked(Command::new(\"rc-update\").args([\"del\", \"prx\", \"default\"])).await?;",
    "src/service/mod.rs::uninstall_linux::run_checked(Command::new(\"systemctl\").args([\"--user\", \"daemon-reload\"])).await?;",
    "src/skills/mod.rs::install_skill_from_source::let mut junction_cmd = tokio::process::Command::new(\"cmd\");",
    "src/tools/mcp.rs::stdio_command::let mut cmd = Command::new(command);",
    "src/tools/message_send.rs::auto_generate_voice::let mut ffmpeg_cmd = tokio::process::Command::new(\"ffmpeg\");",
    "src/tools/message_send.rs::auto_generate_voice::let mut node_cmd = tokio::process::Command::new(\"node\");",
    "src/tools/message_send.rs::auto_generate_voice::let mut npm_cmd = tokio::process::Command::new(\"npm\");",
    "src/tools/sessions_spawn/process_mode.rs::run_sub_agent_process::let mut command = tokio::process::Command::new(executable);",
    "src/tunnel/cloudflare.rs::start::let mut child = Command::new(\"cloudflared\")",
    "src/tunnel/custom.rs::start::let mut child = Command::new(parts[0])",
    "src/tunnel/ngrok.rs::start::Command::new(\"ngrok\")",
    "src/tunnel/ngrok.rs::start::let mut child = Command::new(\"ngrok\")",
    "src/tunnel/tailscale.rs::start::let child = Command::new(\"tailscale\")",
    "src/tunnel/tailscale.rs::start::let output = Command::new(\"tailscale\")",
    "src/tunnel/tailscale.rs::stop::Command::new(\"tailscale\")",
];

/// The string literals of one source line, before any `//` comment.
fn line_string_literals(line: &str) -> Vec<String> {
    let characters = line.chars().collect::<Vec<_>>();
    let mut literals = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        let character = characters[index];
        if character == '/' && characters.get(index + 1) == Some(&'/') {
            break;
        }
        if character == '\'' {
            // `'"'` and `'\"'` are characters, not string delimiters.
            if characters.get(index + 1) == Some(&'"') && characters.get(index + 2) == Some(&'\'') {
                index += 3;
                continue;
            }
            if characters.get(index + 1) == Some(&'\\')
                && characters.get(index + 2) == Some(&'"')
                && characters.get(index + 3) == Some(&'\'')
            {
                index += 4;
                continue;
            }
        }
        if character == '"' {
            let mut literal = String::new();
            index += 1;
            while index < characters.len() && characters[index] != '"' {
                if characters[index] == '\\' {
                    literal.push(characters[index]);
                    index += 1;
                }
                if let Some(next) = characters.get(index) {
                    literal.push(*next);
                }
                index += 1;
            }
            literals.push(literal);
        }
        index += 1;
    }
    literals
}

/// Whether a string literal names the git program.
fn names_git_program(literal: &str) -> bool {
    let literal = literal.to_ascii_lowercase();
    matches!(literal.as_str(), "git" | "git.exe")
        || literal.ends_with("/git")
        || literal.ends_with("/git.exe")
        || literal.ends_with("\\\\git.exe")
}

/// GIT-ENV F-6: the `git` program is named only in reviewed places, so a
/// constant, a path or a variable holding it cannot start git outside the
/// hardened launch unnoticed.
#[test]
pub(super) fn git_is_named_only_in_the_reviewed_places() {
    let mut actual = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let production = production_part(&read_source(&path));
        let lines = production.lines().collect::<Vec<_>>();
        for (index, line) in lines.iter().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            for literal in line_string_literals(line) {
                if names_git_program(&literal) {
                    actual.push(format!("{relative}::{}::{literal}", enclosing_function(&lines, index)));
                }
            }
        }
    }
    assert_inventory("production literals naming git", actual, ALLOWED_GIT_PROGRAM_LITERALS);
}

/// GIT-ENV F-6: every production process command is inventoried, whatever
/// runs it (`.spawn()`, `.output()`, `.status()`, `run_managed_output`).
#[test]
pub(super) fn every_production_process_command_is_inventoried() {
    let mut actual = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let production = production_part(&read_source(&path));
        let lines = production.lines().collect::<Vec<_>>();
        for (index, line) in lines.iter().enumerate() {
            // `Command::new` as a call or as a function value (`.map(Command::new)`).
            let builds_command = line.match_indices("Command::new").any(|(offset, _)| {
                offset == 0
                    || line[..offset]
                        .chars()
                        .next_back()
                        .is_none_or(|before| !(before.is_ascii_alphanumeric() || before == '_'))
            });
            if builds_command {
                actual.push(format!(
                    "{relative}::{}::{}",
                    enclosing_function(&lines, index),
                    normalize_line(line)
                ));
            }
        }
    }
    assert_inventory("production process commands", actual, ALLOWED_PROCESS_COMMANDS);
}

/// GIT-ENV F-6 (D-106): no production code renames a process `Command`
/// type (`use tokio::process::Command as Proc;`), which would hide its
/// launch sites from the inventory above.
#[test]
pub(super) fn process_command_types_are_never_aliased() {
    let mut actual = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let production = production_part(&read_source(&path));
        let lines = production.lines().collect::<Vec<_>>();
        for (index, line) in lines.iter().enumerate() {
            let normalized = normalize_line(line);
            if normalized.contains("Command as ") {
                actual.push(format!(
                    "{relative}::{}::{normalized}",
                    enclosing_function(&lines, index)
                ));
            }
        }
    }
    assert_inventory("aliased process command types", actual, &[]);
}

/// GIT-ENV (D-106, D-107 and its amendment): git in a shell command string
/// is refused on the model-driven surfaces (the `shell` tool, workspace skill
/// scripts, cron jobs and Xin tasks an agent tool created) and hardened
/// through the environment on the operator's (`prx cron`/`prx xin` jobs,
/// system Xin tasks, node allowlists).
#[test]
fn shell_sites_refuse_or_harden_git() {
    let body = |relative: &str, function: &str| {
        let source = fs::read_to_string(repository_root().join(relative))
            .unwrap_or_else(|error| panic!("read {relative}: {error}"));
        function_body(&production_part(&source), function)
    };
    let adapter = body("src/runtime/shell_process.rs", "execute");
    for required in [
        "git_launch::shell_command_runs_git(request.command)",
        "ShellProcessError::GitRefused",
        "git_launch::operator_git_environment()",
    ] {
        assert!(
            adapter.contains(required),
            "ShellProcessAdapter::execute must use {required}"
        );
    }
    for (relative, function) in [
        ("src/tools/shell.rs", "execute_inner"),
        ("src/tools/skills.rs", "execute_resolved"),
    ] {
        assert!(
            body(relative, function).contains("ShellGitPolicy::RefuseGit"),
            "{relative}::{function} is model-driven and must refuse git"
        );
    }
    // D-107 amendment: cron and Xin decide by the recorded creator.
    assert!(
        body("src/cron/scheduler.rs", "run_job_command_with_adapter").contains("git: shell_git_policy(config, job)")
    );
    assert!(body("src/cron/scheduler.rs", "shell_git_policy").contains("job_created_via("));
    assert!(body("src/xin/runner.rs", "run_shell_with_adapter").contains("git: shell_git_policy(config, task)"));
    let xin = body("src/xin/runner.rs", "shell_git_policy");
    assert!(xin.contains("TaskKind::System") && xin.contains("task_created_via("));
    assert!(body("src/nodes/server.rs", "execute_shell_command").contains("git_launch::operator_git_environment()"));
}

/// Every production `git` process is built by the one hardened launch
/// (GIT-ENV, D-098): `src/security/git_launch.rs` is the only production file
/// that names the `git` program, and its builder clears the environment,
/// reads the repository's configuration to neutralise the programs it names
/// (D-100, D-103), sets the fixed program settings on every launch, and
/// points `core.hooksPath` nowhere.
#[test]
pub(super) fn git_runs_only_through_the_hardened_launch() {
    const LAUNCH: &str = "src/security/git_launch.rs";
    let mut outside = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let production = production_part(&read_source(&path));
        let count = production.matches("Command::new(\"git\")").count();
        if relative == LAUNCH {
            assert_eq!(count, 2, "{LAUNCH}: the launch and the configuration read");
            let entry = function_body(&production, "git_command");
            assert!(
                entry.contains("std::env::vars_os()") && entry.contains("git_command_with_parent(scope, parent)"),
                "git_command must launch with PRX's own environment"
            );
            let builder = function_body(&production, "git_command_with_parent");
            for required in [
                "command.env_clear()",
                "git_environment(parent)",
                "inspect_repository(dir, &environment).await?",
                "fixed_overrides(&entries, &paths)",
                "repository_overrides(&entries, &paths, &submodules)?",
                "ALWAYS_OVERRIDES",
                "\"core.hooksPath\"",
                "hooks_path()?",
            ] {
                assert!(
                    builder.contains(required),
                    "git_command_with_parent must use {required}"
                );
            }
            let reader = function_body(&production, "read_git");
            assert!(reader.contains(".env_clear()"), "read_git must clear the environment");
            let environment = function_body(&production, "git_environment");
            assert!(environment.contains("mcp_launch::launch_environment("), "{environment}");
        } else if count > 0 {
            outside.push(relative);
        }
    }
    assert!(
        outside.is_empty(),
        "these files start `git` outside the hardened launch (use security::git_launch::git_command): {outside:?}"
    );
}

/// Every stdio MCP launch starts from a cleared environment (D-045): the
/// one command builder clears it and adds only the launch allowlist and the
/// server's own `env`, and both launch paths (discovery, call sessions) use
/// it.
#[test]
fn stdio_mcp_launches_start_from_a_cleared_environment() {
    let source = fs::read_to_string(repository_root().join("src/tools/mcp.rs"))
        .unwrap_or_else(|error| panic!("read src/tools/mcp.rs: {error}"));
    let production = production_part(&source);
    let builder = function_body(&production, "stdio_command");
    assert!(builder.contains("cmd.env_clear();"), "{builder}");
    // D-088 C-3: never PRX's own working directory.
    assert!(builder.contains("cmd.current_dir(working_dir);"), "{builder}");
    assert!(builder.contains("mcp_launch::launch_environment("), "{builder}");
    assert_eq!(
        production.matches("Command::new(").count(),
        1,
        "src/tools/mcp.rs builds a process command outside `stdio_command`"
    );
    for launch in ["discover_server_tools_stdio", "call_stdio"] {
        let body = function_body(&production, launch);
        assert!(body.contains("TokioChildProcess::builder(cmd)"), "{launch}");
        assert!(
            body.contains("Self::stdio_command(server, command, &working_dir)"),
            "{launch}"
        );
    }
}
