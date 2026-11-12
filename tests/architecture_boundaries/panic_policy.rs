use super::helpers::{
    assert_inventory, blank_comments_and_literals, close_of_group, is_test_code_file, read_source, relative_path,
    rust_source_files, strip_cfg_test_items, test_module_files,
};

fn violations(source: &str) -> Vec<String> {
    // Preserve punctuation as individual tokens, and identifiers as whole words.
    let mut lexed = Vec::new();
    for token in source.split_whitespace() {
        let mut start = 0;
        for (index, ch) in token.char_indices() {
            if !(ch.is_ascii_alphanumeric() || ch == '_') {
                if start < index {
                    lexed.push(&token[start..index]);
                }
                lexed.push(&token[index..index + ch.len_utf8()]);
                start = index + ch.len_utf8();
            }
        }
        if start < token.len() {
            lexed.push(&token[start..]);
        }
    }
    let mut found = Vec::new();
    for window in lexed.windows(3) {
        if window[0] == "." && window[1] == "expect" && window[2] == "(" {
            found.push(".expect(".to_string());
        }
    }
    for window in lexed.windows(4) {
        if window[0] == "." && window[1] == "unwrap" && window[2] == "(" && window[3] == ")" {
            found.push(".unwrap()".to_string());
        }
    }
    for window in lexed.windows(2) {
        if matches!(window[0], "panic" | "unreachable" | "todo" | "unimplemented") && window[1] == "!" {
            found.push(format!("{}!", window[0]));
        }
    }
    let chars: Vec<char> = source.chars().collect();
    for index in 0..chars.len() {
        if chars[index] != '#' {
            continue;
        }
        let bracket = index + if chars.get(index + 1) == Some(&'!') { 2 } else { 1 };
        if chars.get(bracket) != Some(&'[') {
            continue;
        }
        let end = close_of_group(&chars, bracket);
        let attribute: String = chars[bracket..end].iter().filter(|c| !c.is_whitespace()).collect();
        let suppressed = attribute
            .match_indices("allow(")
            .chain(attribute.match_indices("expect("))
            .any(|(offset, name)| {
                if attribute
                    .get(..offset)
                    .and_then(|s| s.chars().next_back())
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
                {
                    return false;
                }
                let group: Vec<char> = attribute[offset + name.len() - 1..].chars().collect();
                let close = close_of_group(&group, 0);
                let inner: String = group[..close].iter().collect();
                ["clippy::unwrap_used", "clippy::expect_used", "clippy::panic"]
                    .iter()
                    .any(|lint| {
                        inner.match_indices(*lint).any(|(start, _)| {
                            !inner
                                .get(start + lint.len()..)
                                .and_then(|s| s.chars().next())
                                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
                        })
                    })
            });
        if suppressed {
            found.push(attribute);
        }
    }
    found
}

#[test]
pub(super) fn production_has_no_panic_shortcuts() {
    let modules = test_module_files();
    let mut actual = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let raw = read_source(&path);
        if is_test_code_file(&relative, &raw, &modules) {
            continue;
        }
        let production = strip_cfg_test_items(&blank_comments_and_literals(&raw));
        actual.extend(
            violations(&production)
                .into_iter()
                .map(|hit| format!("{relative}: {hit}")),
        );
    }
    assert_inventory("panic-free production", actual, &[]);
}

// Ratchet for part B. No new file may acquire catch_unwind. Remove entries
// as their last use disappears; never add an entry to bypass this guard.
const CATCH_UNWIND_FILES: &[&str] = &[
    "src/agent/agent/cleanup_diagnostic_tests.rs",
    "src/agent/agent/native_decision_fixture.rs",
    "src/agent/agent/retired_escalation_tests.rs",
    "src/agent/turn_tests/admission.rs",
    "src/agent/turn_tests/engine_budget.rs",
    "src/agent/turn_tests/engine_stay_silent.rs",
    "src/agent/turn_tests/engine_surface.rs",
    "src/agent/turn_tests/entry_provider_selection.rs",
    "src/agent/turn_tests/turn_engine.rs",
    "src/causal_cli/backend_transport_tests.rs",
    "src/channels/tests/factory_reload_fixture.rs",
    "src/channels/tests/mod.rs",
    "src/channels/tests/persistence_probe.rs",
    "src/channels/tests/prompt.rs",
    "src/channels/tests/signal_accounted.rs",
    "src/channels/tests/smart_group_accounted.rs",
    "src/channels/wacli.rs",
    "src/chat/decision_source/admission_failure_tests/operator_accounting.rs",
    "src/chat/decision_source/admission_failure_tests/worker_stdin_protocol.rs",
    "src/chat/dispatcher/real_mode_tests/dual_write.rs",
    "src/chat/sessions/pty.rs",
    "src/chat/sessions/runtime.rs",
    "src/chat/tests/legacy_chat_compaction_audit_tests.rs",
    "src/chat/tests/session_cleanup_tests.rs",
    "src/decision/builtin/mod.rs",
    "src/gateway/api/runtime.rs",
    "src/gateway/api/sessions/tests/collection.rs",
    "src/gateway/api/sessions/tests/collection/two_owner.rs",
    "src/gateway/api/sessions/tests/collection/two_owner/browser_fixture.rs",
    "src/gateway/tests/mod.rs",
    "src/gateway/tests/platform_jobs.rs",
    "src/gateway/tests/webhook_turns.rs",
    "src/memory/postgres/tests/basics.rs",
    "src/nodes/server/tests/process_group.rs",
    "src/providers/label_redaction_tests.rs",
    "src/runtime/decision_accounting/transport_tests.rs",
    "src/runtime/h2_entry/authority/tests.rs",
    "src/runtime/turn_decision/routing/tests/paid_embedding.rs",
    "src/runtime/turn_decision/tests/child_process.rs",
    "src/runtime/turn_decision/tests/child_process/g8_segment.rs",
    "src/runtime/turn_decision/tests/child_process/worker_probe.rs",
    "src/runtime/turn_decision/tests/exact_call_approval.rs",
    "src/runtime/turn_decision/tests/exact_call_approval/collection.rs",
    "src/runtime/turn_decision/tests/exact_call_approval/collection_usage.rs",
    "src/runtime/turn_decision/tests/exact_call_approval/native_usage.rs",
    "src/runtime/turn_decision/tests/exact_call_approval/reported_usage.rs",
    "src/runtime/turn_decision/tests/exact_call_approval/skills_manage_grants.rs",
    "src/runtime/turn_decision/tests/ledger_probe.rs",
    "src/runtime/turn_decision/tests/performance.rs",
    "src/runtime/turn_decision/tests/performance/model_binding.rs",
    "src/runtime/turn_decision/tests/performance/model_binding/original_service_wire.rs",
    "src/runtime/turn_decision/tests/performance/witness_mutations.rs",
    "src/tools/delegate.rs",
    "src/tools/delegate_diagnostic.rs",
    "src/tools/sessions_spawn/process_mode.rs",
    "src/tools/sessions_spawn/spawn.rs",
    "src/tools/sessions_spawn/tests/lineage.rs",
    "src/tools/sessions_spawn/tests/owned_child.rs",
];

#[test]
pub(super) fn catch_unwind_file_inventory_only_shrinks() {
    let mut actual = Vec::new();
    for path in rust_source_files() {
        let code = blank_comments_and_literals(&read_source(&path));
        if code
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .any(|token| token == "catch_unwind")
        {
            actual.push(relative_path(&path));
        }
    }
    assert_inventory("catch_unwind ratchet (remove entries only)", actual, CATCH_UNWIND_FILES);
}

#[test]
fn panic_literals_and_comments_are_ignored() {
    let code = blank_comments_and_literals(
        r#"
        /// .unwrap() .expect("doc") panic!()
        const TEXT: &str = ".unwrap() .expect( panic!() unreachable!()";
        // todo!() unimplemented!()
        const CHAR: char = '!';
    "#,
    );
    assert!(
        violations(&code).is_empty(),
        "documentation and literal decoys must not trigger policy"
    );
}
