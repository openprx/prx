use super::helpers::{
    assert_inventory, blank_comments, blank_comments_and_literals, enclosing_function, is_test_code_file,
    log_macro_arguments, normalize_line, read_source, relative_path, rust_source_files, strip_cfg_test_items,
    test_module_files,
};

/// Byte-aligned structural text: the shared lexer preserves character offsets.
fn structural(source: &str) -> String {
    blank_comments_and_literals(source)
        .chars()
        .zip(source.chars())
        .map(|(masked, raw)| {
            if masked == ' ' {
                " ".repeat(raw.len_utf8())
            } else {
                masked.to_string()
            }
        })
        .collect()
}

/// Balanced call arguments, ignoring delimiters inside literals.
fn calls(source: &str, needle: &str) -> Vec<(usize, String)> {
    let mask = structural(source);
    let mut found = Vec::new();
    for (start, _) in mask.match_indices(needle) {
        let open = start + needle.len() - 1;
        let mut depth = 0usize;
        for (offset, byte) in mask.as_bytes()[open..].iter().enumerate() {
            match byte {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        found.push((start, source[start..=open + offset].to_string()));
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    found
}

fn site(relative: &str, source: &str, offset: usize, text: &str) -> String {
    let lines: Vec<_> = source.lines().collect();
    format!(
        "{relative}::{}::{}",
        enclosing_function(&lines, source[..offset].matches('\n').count()),
        normalize_line(text)
    )
}

fn builders(source: &str) -> Vec<(usize, String)> {
    calls(source, "format!(")
        .into_iter()
        .chain(calls(source, ".query("))
        .filter(|(_, call)| {
            call.contains("/bot{")
                || ["key", "token", "ticket", "sig", "signature", "access_token", "api_key"]
                    .iter()
                    .any(|key| call.contains(&format!("?{key}=")) || call.contains(&format!("&{key}=")))
                || ["key", "token", "access_token", "api_key", "sig"].iter().any(|key| {
                    call.split_whitespace()
                        .collect::<String>()
                        .contains(&format!("(\"{key}\","))
                })
        })
        .collect()
}

// Each builder's mitigation is documented beside its reviewed inventory entry.
const BUILDERS: &[&str] = &[
    // Non-stream send and warmup strip reqwest errors before propagation.
    "src/providers/gemini.rs::build_generate_content_url::format!(\"{base_url}?key={}\", auth.credential())",
    // StreamError::from removes URLs before streaming sinks.
    "src/providers/gemini.rs::build_stream_url::format!(\"{base}?alt=sse&key={}\", auth.credential())",
    "src/providers/gemini.rs::warmup::format!( \"https://generativelanguage.googleapis.com/v1beta/models?key={}\", auth.credential() )",
    // Both blocking send and status errors pass without_url.
    "src/providers/model_discovery.rs::fetch_gemini_models::.query(&[(\"key\", api_key), (\"pageSize\", \"200\")])",
    // Tungstenite errors carry no request URL; the URL is never logged.
    "src/channels/dingtalk.rs::listen::format!(\"{}?ticket={}\", gw.endpoint, gw.ticket)",
    // The wizard strips its Telegram probe error even though it is dropped.
    "src/initialization/wizard/channels.rs::setup_channels::format!(\"https://api.telegram.org/bot{token_clone}/getMe\")",
    // Telegram transport protection belongs to PR #191; excluded until landed.
    "src/channels/telegram.rs::api_url::format!(\"{}/bot{}/{method}\", self.api_base, self.bot_token)",
];

#[test]
pub(super) fn secret_url_builders_are_inventoried() {
    let mut actual = Vec::new();
    let test_modules = test_module_files();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let raw = read_source(&path);
        if is_test_code_file(&relative, &raw, &test_modules) {
            continue;
        }
        let production = blank_comments(&strip_cfg_test_items(&raw));
        actual.extend(
            builders(&production)
                .into_iter()
                .map(|(offset, call)| site(&relative, &production, offset, &call)),
        );
    }
    assert_inventory("secret URL builders", actual, BUILDERS);
}

fn carrier_offenders(relative: &str, source: &str) -> Vec<String> {
    let mut offenders = Vec::new();
    for needle in [
        ".send()",
        ".error_for_status()",
        ".and_then(reqwest::blocking::Response::error_for_status)",
    ] {
        for (offset, _) in source.match_indices(needle) {
            // HTTP calls end at propagation or the end of a match; channel sends
            // have arguments and are not part of this scan.
            let tail = &source[offset + needle.len()..];
            let end = tail.find(';').unwrap_or(tail.len());
            let chain = &tail[..end];
            let compact = chain.split_whitespace().collect::<String>();
            let mapped = compact.strip_prefix(".await").unwrap_or(&compact);
            let helper_closure = mapped.strip_prefix(".map_err(|").is_some_and(|closure| {
                let Some((binding, body)) = closure.split_once('|') else {
                    return false;
                };
                body.starts_with(&format!("StreamError::from({binding})"))
                    || (relative == "src/channels/telegram.rs"
                        && body.starts_with(&format!("self.transport_error({binding})")))
            });
            let same_chain = helper_closure
                || compact.starts_with(".map_err(reqwest::Error::without_url)")
                || compact.starts_with(".await.map_err(reqwest::Error::without_url)")
                || compact.starts_with(
                    ".and_then(reqwest::blocking::Response::error_for_status).map_err(reqwest::Error::without_url)",
                )
                || compact.starts_with(".await.map_err(webhook_failure_text)")
                || compact.starts_with(".await.map_err(Self::transport_error)")
                || compact.starts_with(".await.map_err(transport_error)");
            let handled_arm = chain.find("Err(").is_some_and(|at| {
                let arm = &chain[at + 4..];
                let Some((binding, rest)) = arm.split_once(')') else {
                    return false;
                };
                let binding = binding.trim();
                let rest = rest
                    .trim_start()
                    .strip_prefix("=>")
                    .unwrap_or("")
                    .trim_start()
                    .trim_start_matches('{')
                    .trim_start();
                rest.starts_with(&format!("{binding}.without_url()"))
                    || rest.starts_with(&format!("let {binding} = {binding}.without_url()"))
                    || rest.starts_with(&format!("return Err({binding}.without_url()"))
                    || rest.starts_with(&format!("let _ = tx.send(Err(StreamError::from({binding}))"))
                    || rest.starts_with(&format!("let _ = tx.send(Err({binding}.into()))"))
                    || rest.starts_with(&format!("StreamError::from({binding})"))
                    || rest.starts_with(&format!("Self::transport_error({binding})"))
            });
            if !same_chain && !handled_arm {
                offenders.push(site(relative, source, offset, needle));
            }
        }
    }
    offenders
}

#[test]
pub(super) fn secret_url_carriers_strip_request_urls() {
    let mut offenders = Vec::new();
    let test_modules = test_module_files();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let raw = read_source(&path);
        if is_test_code_file(&relative, &raw, &test_modules) {
            continue;
        }
        let production = blank_comments(&strip_cfg_test_items(&raw));
        if builders(&production).is_empty() {
            continue;
        }
        // PR #191 owns Telegram. Include it once its stripping helper lands.
        if relative == "src/channels/telegram.rs" && !production.contains("fn transport_error(") {
            continue;
        }
        offenders.extend(carrier_offenders(&relative, &production));
    }
    assert_inventory("URL-carrying HTTP errors without a strip", offenders, &[]);
}

#[test]
pub(super) fn stream_http_errors_have_one_constructor() {
    let mut patterns = Vec::new();
    let mut constructors = Vec::new();
    let mut offenders = Vec::new();
    let tests = test_module_files();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let raw = read_source(&path);
        if is_test_code_file(&relative, &raw, &tests) {
            continue;
        }
        let production = strip_cfg_test_items(&blank_comments_and_literals(&raw));
        if relative == "src/providers/traits.rs" {
            constructors.extend(
                production
                    .match_indices("Self::Http(")
                    .map(|(offset, _)| site(&relative, &production, offset, "Self::Http(")),
            );
        }
        for (offset, _) in production.match_indices("StreamError::Http") {
            let tail = &production[offset..];
            if !tail.starts_with("StreamError::Http(") && !tail.starts_with("StreamError::Http)") {
                continue;
            }
            if relative == "src/providers/traits.rs" {
                constructors.push(site(
                    &relative,
                    &production,
                    offset,
                    tail.lines().next().expect("test: constructor line"),
                ));
                continue;
            }
            let line = production[..offset].matches('\n').count();
            let lines: Vec<_> = production.lines().collect();
            let name = enclosing_function(&lines, line);
            // Values are disallowed even where the file also contains patterns.
            let prefix = production[..offset].trim_end();
            if prefix.ends_with("Err(") || prefix.ends_with("map_err(") {
                offenders.push(site(
                    &relative,
                    &production,
                    offset,
                    tail.lines().next().expect("test: line"),
                ));
            } else {
                patterns.push(format!("{relative}::{name}"));
            }
        }
    }
    assert_eq!(
        constructors.len(),
        1,
        "test: streaming HTTP errors must have exactly one conversion constructor"
    );
    assert_inventory("raw streaming HTTP constructors", offenders, &[]);
    assert_inventory(
        "stream HTTP patterns",
        patterns,
        &[
            "src/agent/provider_round.rs::streaming_error_is_context_overflow",
            "src/agent/provider_round.rs::streaming_error_is_network_timeout",
            "src/agent/provider_round.rs::run_streaming_provider_turn",
            "src/providers/reliable.rs::classify_stream_error",
            "src/providers/reliable.rs::classify_stream_error",
            "src/chat/error.rs::from",
        ],
    );
}

fn raw_url_logs(relative: &str, source: &str) -> Vec<String> {
    log_macro_arguments(source)
        .into_iter()
        .filter(|(_, _, value)| {
            let mut wrappers = calls(value, "ws_target_label(");
            for name in value.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
                if name.starts_with("redact_") {
                    wrappers.extend(calls(value, &format!("{name}(")));
                }
            }
            [
                "ws_url",
                "wss_url",
                "sse_url",
                "webhook_url",
                "wsurl",
                "wssurl",
                "sseurl",
                "webhookurl",
            ]
            .iter()
            .any(|name| {
                value.match_indices(*name).any(|(offset, _)| {
                    let before = value[..offset].chars().next_back();
                    let after = value[offset + name.len()..].chars().next();
                    let identifier = |c: char| c.is_ascii_alphanumeric() || c == '_';
                    !before.is_some_and(identifier)
                        && !after.is_some_and(identifier)
                        && !wrappers
                            .iter()
                            .any(|(start, call)| offset >= *start && offset < *start + call.len())
                })
            })
        })
        .map(|(offset, _, value)| site(relative, source, offset, &value))
        .collect()
}

#[test]
pub(super) fn server_url_logs_are_redacted() {
    let mut offenders = Vec::new();
    let tests = test_module_files();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if !relative.starts_with("src/channels/") {
            continue;
        }
        let raw = read_source(&path);
        if is_test_code_file(&relative, &raw, &tests) {
            continue;
        }
        let production = blank_comments(&strip_cfg_test_items(&raw));
        offenders.extend(raw_url_logs(&relative, &production));
    }
    assert_inventory("unredacted server-issued URL log arguments", offenders, &[]);
}

#[test]
fn comment_blanker_preserves_url_and_raw_literals() {
    let source = r##"fn f() { /* outer /* inner */ comment */
        let url = r#"https://example.com/?sig={marker}//literal"#;
        // format!("?key={marker}")
        let escaped = "quote \" // still literal";
    }"##;
    let blanked = blank_comments(source);
    assert!(
        blanked.contains("https://example.com/?sig={marker}//literal"),
        "test: raw URL literal was blanked"
    );
    assert!(
        blanked.contains("still literal"),
        "test: escaped quote confused comment recognition"
    );
    assert!(
        !blanked.contains("outer") && !blanked.contains("?key="),
        "test: comments survived blanking"
    );
}
