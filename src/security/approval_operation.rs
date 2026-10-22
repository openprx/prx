//! Shared operation identity derivation for runtime approval producers.

/// Risk class a resource operation is issued at. Mirrors the risk the matching
/// tool passes to `SideEffectGate::authorize_resource_operation`, so the
/// no-escalation check in the v2 gate path never rejects a legitimately approved
/// call.
#[derive(Clone)]
pub(crate) enum GrantOp {
    /// Shell-style command, bound by command hash (v1 path; kept for cron/runner
    /// persisted-grant compatibility).
    Command(String),
    /// Exact resource operation id + the risk it is authorized at. Used when the
    /// agent loop can reconstruct the tool's gate op-id verbatim from the raw
    /// args (no runtime resolution involved).
    ResourceExact(String, crate::acl::approval_grant::RiskLevel),
    /// Conservative glob op pattern (`{tool}:{verb}:*`) + risk. Used when the
    /// tool derives its gate op-id from runtime-resolved state (canonicalized
    /// path, resolved recipient, owner-principal ref) that the loop cannot
    /// reproduce byte-for-byte. The glob is still tool+verb scoped (never a bare
    /// wildcard), and the grant remains principal-bound, time-bound, single-use
    /// and signed — far tighter than the removed v1 wildcard grant.
    ResourceGlob(String, crate::acl::approval_grant::RiskLevel),
}

/// Derive the operation a freshly approved tool call is authorized for.
///
/// For `ResourceExact` the returned op id MUST equal the `operation_name` the
/// target tool computes before calling the gate. For `ResourceGlob` the tool's
/// resolved op id must start with the `{tool}:{verb}:` prefix. Returning `None`
/// means we cannot bind even a tool+verb scope; with the d08-tightened gate such
/// a call is denied rather than wildcard-approved.
pub(crate) fn grant_op_for_call(tool_name: &str, args: &serde_json::Value) -> Option<GrantOp> {
    use crate::acl::approval_grant::RiskLevel;
    let string_arg = |name: &str| args.get(name).and_then(serde_json::Value::as_str);
    match tool_name {
        "shell" => string_arg("command").map(|command| GrantOp::Command(command.to_string())),
        "cron" => {
            let action = string_arg("action").unwrap_or_default();
            let command = if action == "update" {
                args.get("patch")
                    .and_then(|patch| patch.get("command"))
                    .and_then(serde_json::Value::as_str)
            } else {
                string_arg("command")
            };
            command.map(|command| GrantOp::Command(command.to_string()))
        }
        // ── Exact: loop reconstructs the tool's op-id verbatim from raw args ──
        "config_reload" => Some(GrantOp::ResourceExact(
            "config_reload:reload".to_string(),
            RiskLevel::Low,
        )),
        "memory_reindex" => Some(GrantOp::ResourceExact(
            "memory_reindex:rebuild".to_string(),
            RiskLevel::Medium,
        )),
        "proxy_config" => {
            let action = string_arg("action").unwrap_or_default();
            let (operation, risk) = match action {
                "set" | "apply_env" => ("proxy_config:set", RiskLevel::Medium),
                "disable" | "clear_env" => ("proxy_config:unset", RiskLevel::Medium),
                _ => ("proxy_config:show", RiskLevel::Low),
            };
            Some(GrantOp::ResourceExact(operation.to_string(), risk))
        }
        "nodes" => {
            let action = string_arg("action").unwrap_or_default();
            let node = string_arg("node").unwrap_or_default();
            let task_id = string_arg("task_id").unwrap_or_default();
            // Risk MUST match NodesTool::execute's authorize_resource_operation
            // risk for each action, or the v2 no-escalation check rejects an
            // approved call: exec=High, write=High, cancel=Medium, list=Low.
            let (operation, risk) = match action {
                "exec" => (format!("nodes:exec:{node}"), RiskLevel::High),
                "write" => (format!("nodes:write:{node}"), RiskLevel::High),
                "cancel" => (format!("nodes:cancel:{node}:{task_id}"), RiskLevel::Medium),
                _ => ("nodes:list".to_string(), RiskLevel::Low),
            };
            Some(GrantOp::ResourceExact(operation, risk))
        }
        "git_operations" => {
            let operation = string_arg("operation").unwrap_or_default();
            let (resource_operation, risk) = match operation {
                "push" => (
                    format!("git_operations:push:{}", string_arg("remote").unwrap_or("origin")),
                    RiskLevel::High,
                ),
                "reset_hard" => ("git_operations:reset_hard".to_string(), RiskLevel::High),
                "commit" => ("git_operations:commit".to_string(), RiskLevel::Medium),
                "add" => ("git_operations:add".to_string(), RiskLevel::Medium),
                "checkout" => {
                    let branch = string_arg("branch").unwrap_or("unknown");
                    (format!("git_operations:checkout:{branch}"), RiskLevel::Medium)
                }
                "stash" => {
                    let action = string_arg("stash_action").unwrap_or("push");
                    (format!("git_operations:stash:{action}"), RiskLevel::Medium)
                }
                other => (format!("git_operations:{other}"), RiskLevel::Medium),
            };
            Some(GrantOp::ResourceExact(resource_operation, risk))
        }
        "sessions_send" => {
            let run_id = string_arg("run_id").unwrap_or_default();
            Some(GrantOp::ResourceExact(
                format!("sessions_send:steer:{run_id}"),
                RiskLevel::Low,
            ))
        }
        "sessions_spawn" => {
            // Mirror SessionsSpawnTool::execute's per-action gate op-id + risk
            // verbatim, or the v2 no-escalation check rejects an approved call:
            //   spawn (default/unknown)     → sessions_spawn:spawn,           Medium
            //   kill                        → sessions_spawn:kill:<run_id>,   Medium
            //   steer                       → sessions_spawn:steer:<run_id>,  Low
            //   list / history (no gate)    → None
            // run_id is trimmed to match the tool, which trims it before
            // building the op-id (see execute_kill / execute_steer callers).
            let action = string_arg("action").unwrap_or("spawn");
            let run_id = string_arg("run_id").unwrap_or_default().trim();
            match action {
                "kill" => Some(GrantOp::ResourceExact(
                    format!("sessions_spawn:kill:{run_id}"),
                    RiskLevel::Medium,
                )),
                "steer" => Some(GrantOp::ResourceExact(
                    format!("sessions_spawn:steer:{run_id}"),
                    RiskLevel::Low,
                )),
                // list / history are read-only and the tool gates neither.
                "list" | "history" => None,
                // Default and any unrecognized action fall through to spawn,
                // exactly as the tool's `_ => {}` arm does.
                _ => Some(GrantOp::ResourceExact(
                    "sessions_spawn:spawn".to_string(),
                    RiskLevel::Medium,
                )),
            }
        }
        "subagents" => {
            let action = string_arg("action").unwrap_or("list");
            let run_id = string_arg("run_id").unwrap_or_default();
            let risk = if action == "kill" {
                RiskLevel::Medium
            } else {
                RiskLevel::Low
            };
            Some(GrantOp::ResourceExact(format!("subagents:{action}:{run_id}"), risk))
        }
        "gateway" => {
            let action = string_arg("action").unwrap_or_default();
            Some(GrantOp::ResourceExact(format!("gateway:{action}"), RiskLevel::Medium))
        }
        "skills_manage" => {
            skills_manage_lifecycle_operation(args).map(|(operation, risk)| GrantOp::ResourceExact(operation, risk))
        }
        // ── Glob: tool derives op-id from runtime-resolved state ──────────────
        // file_write op-id uses the canonicalized resolved path; message_send /
        // memory_* use resolved recipient / owner-principal refs; http_request
        // uses the parsed URL host. The loop cannot reproduce these verbatim, so
        // we scope to `{tool}:{verb}:*` (still tool+verb bound, signed, single
        // use, principal bound).
        "file_write" => Some(GrantOp::ResourceGlob(
            "file_write:write:*".to_string(),
            RiskLevel::Medium,
        )),
        // Decision approval resolves this discriminator to a canonical exact
        // file operation; the legacy callback must not issue this glob.
        "file_edit" => Some(GrantOp::ResourceGlob("file_edit:edit:*".to_string(), RiskLevel::Medium)),
        "http_request" => Some(GrantOp::ResourceGlob(
            "http_request:request:*".to_string(),
            RiskLevel::Medium,
        )),
        "message_send" => Some(GrantOp::ResourceGlob(
            "message_send:send:*".to_string(),
            RiskLevel::Medium,
        )),
        "memory_store" => Some(GrantOp::ResourceGlob(
            "memory_store:write:*".to_string(),
            RiskLevel::Medium,
        )),
        "memory_forget" => Some(GrantOp::ResourceGlob(
            "memory_forget:delete:*".to_string(),
            RiskLevel::High,
        )),
        // Unknown / unmodelled tools get no grant; the tightened gate denies
        // their supervised medium/high calls rather than wildcard-approving.
        _ => None,
    }
}

/// What `skills_manage` needs for one lifecycle action: the operation it
/// checks (when the target arguments are present), the risk it checks it at,
/// and whether `SkillsManageTool::authorize` demands a signed one-shot grant
/// bound to that operation. This is the single table behind both the grant
/// minting and `requires_explicit_grant`, so the two cannot drift.
struct LifecycleAction {
    operation: Option<String>,
    risk: crate::acl::approval_grant::RiskLevel,
    grant_bound: bool,
}

fn skills_manage_lifecycle_action(args: &serde_json::Value) -> Option<LifecycleAction> {
    use crate::acl::approval_grant::RiskLevel;
    let arg = |name: &str| args.get(name).and_then(serde_json::Value::as_str).map(str::trim);
    match arg("action")? {
        "lifecycle_publish" => Some(LifecycleAction {
            operation: arg("proposal_id").map(|proposal| format!("skills_manage:lifecycle_publish:{proposal}")),
            risk: RiskLevel::High,
            grant_bound: true,
        }),
        // Cancel only withdraws a proposal; `SkillsManageTool::authorize`
        // gates it through the ordinary side-effect gate without requiring a
        // signed one-shot grant.
        "lifecycle_cancel" => Some(LifecycleAction {
            operation: arg("proposal_id").map(|proposal| format!("skills_manage:lifecycle_cancel:{proposal}")),
            risk: RiskLevel::Medium,
            grant_bound: false,
        }),
        // Bound to the directory and to the complete bundle hash the operator
        // reviewed (SL11); the same text `SkillsManageTool::authorize` builds.
        "lifecycle_import_executable" => Some(LifecycleAction {
            operation: arg("source")
                .zip(arg("revision_id"))
                .map(|(source, revision)| format!("skills_manage:lifecycle_import_executable:{source}:{revision}")),
            risk: RiskLevel::High,
            grant_bound: true,
        }),
        "lifecycle_rollback" => Some(LifecycleAction {
            operation: args
                .get("expected_generation")
                .and_then(serde_json::Value::as_u64)
                .zip(arg("skill_id").zip(arg("revision_id")))
                .map(|(generation, (skill, revision))| {
                    format!("skills_manage:lifecycle_rollback:{skill}:{revision}:{generation}")
                }),
            risk: RiskLevel::High,
            grant_bound: true,
        }),
        _ => None,
    }
}

/// The operation `skills_manage` checks for a lifecycle mutation, with the risk
/// it checks it at. It MUST equal what `SkillsManageTool::authorize` computes:
/// the target is bound into the operation, trimmed the same way, and a rollback
/// is also bound to the generation the caller observed. Read-only lifecycle
/// actions and the non-lifecycle actions get no grant here.
fn skills_manage_lifecycle_operation(
    args: &serde_json::Value,
) -> Option<(String, crate::acl::approval_grant::RiskLevel)> {
    let action = skills_manage_lifecycle_action(args)?;
    Some((action.operation?, action.risk))
}

/// The `skills_manage` actions the tool accepts only under a grant bound to
/// their target. A unit test pins this list to the table above.
pub(crate) const SKILLS_MANAGE_GRANT_BOUND_ACTIONS: [&str; 3] =
    ["lifecycle_publish", "lifecycle_rollback", "lifecycle_import_executable"];

/// True when the call can only succeed with its own human approval: the tool
/// itself demands a signed one-shot grant bound to the exact target, at every
/// autonomy level. Execution upgrades an Allow decision to Ask for these, and
/// session allowlisting must never mint a grant for them. Derived from the same
/// table as `skills_manage_lifecycle_operation`.
pub(crate) fn requires_explicit_grant(tool: &str, args: &serde_json::Value) -> bool {
    tool == "skills_manage" && skills_manage_lifecycle_action(args).is_some_and(|action| action.grant_bound)
}

/// Match memory tools' gate identity exactly. The optional principal is the
/// runtime-authored backend argument, not a display sender or owner fallback.
pub(crate) fn memory_write_operation(principal: Option<&str>) -> String {
    let owner = principal
        .map(super::op_id::ref_for_owner)
        .unwrap_or_else(|| "default".to_owned());
    super::op_id::op_id("memory_store", "write", &[&owner])
}

pub(crate) fn memory_delete_operation(principal: Option<&str>, key: &str) -> String {
    let owner = principal
        .map(super::op_id::ref_for_owner)
        .unwrap_or_else(|| "default".to_owned());
    let key_ref = super::op_id::fingerprint16(key);
    super::op_id::op_id("memory_forget", "delete", &[&owner, &key_ref])
}

/// Resolve the parent without creating directories. Missing suffixes are
/// appended only after reaching an existing canonical directory. Dangling
/// symlinks and non-directory ancestors are refused.
pub(crate) async fn resolve_file_target(
    workspace: &std::path::Path,
    path: &str,
    allow_missing_parent: bool,
) -> std::io::Result<std::path::PathBuf> {
    use std::io::{Error, ErrorKind};
    use std::path::Component;
    if path.contains('\0')
        || std::path::Path::new(path)
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(Error::new(ErrorKind::InvalidInput, "invalid file path"));
    }
    let full = workspace.join(path);
    let name = full
        .file_name()
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "file name missing"))?;
    let mut ancestor = full
        .parent()
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "file parent missing"))?
        .to_path_buf();
    let mut missing = Vec::new();
    let mut resolved = loop {
        match tokio::fs::canonicalize(&ancestor).await {
            Ok(resolved) => {
                if !tokio::fs::metadata(&resolved).await?.is_dir() {
                    return Err(Error::new(ErrorKind::NotADirectory, "file parent is not a directory"));
                }
                break resolved;
            }
            Err(error) if allow_missing_parent && error.kind() == ErrorKind::NotFound => {
                match tokio::fs::symlink_metadata(&ancestor).await {
                    Ok(_) => return Err(Error::new(ErrorKind::InvalidInput, "unresolved existing file parent")),
                    Err(error) if error.kind() == ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
                let part = ancestor
                    .file_name()
                    .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "no existing file ancestor"))?
                    .to_os_string();
                missing.push(part);
                if !ancestor.pop() {
                    return Err(Error::new(ErrorKind::InvalidInput, "no existing file ancestor"));
                }
            }
            Err(error) => return Err(error),
        }
    };
    for part in missing.into_iter().rev() {
        resolved.push(part);
    }
    Ok(resolved.join(name))
}

/// Use the same operation and risk for approval and the backend's own gate.
pub(crate) fn file_operation(
    tool: &str,
    workspace: &std::path::Path,
    target: &std::path::Path,
) -> (String, super::policy::ResourceRiskLevel, bool, bool) {
    let launch = crate::config::mcp_launch::is_workspace_launch_file(workspace, target);
    let git = !launch && super::git_launch::is_git_control_file(target);
    let action = if tool == "file_edit" { "edit" } else { "write" };
    let (verb, risk) = if launch {
        (
            format!("{action}-launch-definition"),
            super::policy::ResourceRiskLevel::High,
        )
    } else if git {
        (format!("{action}-git-control"), super::policy::ResourceRiskLevel::High)
    } else {
        (action.to_owned(), super::policy::ResourceRiskLevel::Medium)
    };
    (
        super::op_id::op_id(tool, &verb, &[&super::op_id::ref_for_file(target)]),
        risk,
        launch,
        git,
    )
}

#[cfg(test)]
mod grant_requirement_tests {
    use super::{GrantOp, grant_op_for_call, requires_explicit_grant};
    use serde_json::json;

    /// The three actions `SkillsManageTool::authorize` refuses without a signed
    /// one-shot grant bound to the target, with and without their target
    /// arguments (an incomplete call is still asked, then refused by the tool).
    #[test]
    fn exactly_the_grant_bound_lifecycle_actions_require_an_explicit_grant() {
        let revision = format!("r1-{}", "0".repeat(64));
        for args in [
            json!({"action": "lifecycle_publish", "proposal_id": "prop-1"}),
            json!({"action": "lifecycle_publish"}),
            json!({"action": "lifecycle_rollback", "skill_id": "a", "revision_id": revision, "expected_generation": 1}),
            json!({"action": "lifecycle_rollback", "skill_id": "a"}),
            json!({"action": "lifecycle_import_executable", "source": "/x", "revision_id": revision}),
            json!({"action": "lifecycle_import_executable"}),
        ] {
            assert!(requires_explicit_grant("skills_manage", &args), "{args}");
        }
        // Cancel only closes a proposal: the tool grant-binds it under
        // supervised autonomy through the ordinary gate, not at full.
        for args in [
            json!({"action": "lifecycle_cancel", "proposal_id": "prop-1"}),
            json!({"action": "lifecycle_status"}),
            json!({"action": "lifecycle_list"}),
            json!({"action": "lifecycle_inspect", "proposal_id": "prop-1"}),
            json!({"action": "create", "name": "x"}),
            json!({"action": "enable", "name": "x"}),
            json!({"action": "lifecycle_force_close", "proposal_id": "prop-1"}),
            json!({}),
        ] {
            assert!(!requires_explicit_grant("skills_manage", &args), "{args}");
        }
        // Only skills_manage: the same arguments on another tool are not it.
        assert!(!requires_explicit_grant(
            "shell",
            &json!({"action": "lifecycle_publish", "proposal_id": "prop-1"})
        ));
    }

    #[test]
    fn the_published_action_list_matches_the_table() {
        use super::SKILLS_MANAGE_GRANT_BOUND_ACTIONS;
        for action in SKILLS_MANAGE_GRANT_BOUND_ACTIONS {
            assert!(
                requires_explicit_grant("skills_manage", &json!({"action": action})),
                "{action}"
            );
        }
        for action in ["lifecycle_cancel", "lifecycle_status", "create", "enable"] {
            assert!(!SKILLS_MANAGE_GRANT_BOUND_ACTIONS.contains(&action));
            assert!(
                !requires_explicit_grant("skills_manage", &json!({"action": action})),
                "{action}"
            );
        }
    }

    /// The predicate and the minted operation come from one table: every call
    /// that mints a lifecycle operation other than cancel requires the grant.
    #[test]
    fn the_predicate_agrees_with_the_minted_operation() {
        let revision = format!("r1-{}", "0".repeat(64));
        for args in [
            json!({"action": "lifecycle_publish", "proposal_id": "prop-1"}),
            json!({"action": "lifecycle_cancel", "proposal_id": "prop-1"}),
            json!({"action": "lifecycle_rollback", "skill_id": "a", "revision_id": revision, "expected_generation": 1}),
            json!({"action": "lifecycle_import_executable", "source": "/x", "revision_id": revision}),
        ] {
            let minted = matches!(
                grant_op_for_call("skills_manage", &args),
                Some(GrantOp::ResourceExact(..))
            );
            assert!(minted, "{args}");
            assert_eq!(
                requires_explicit_grant("skills_manage", &args),
                args.get("action").and_then(serde_json::Value::as_str) != Some("lifecycle_cancel"),
                "{args}"
            );
        }
    }
}
