//! Regression guard: all WebSocket channels must use connect_async_with_config
//! (with explicit message size limits) instead of bare connect_async.
//!
//! Bare connect_async accepts messages up to 64 MB by default, which enables
//! trivial memory-exhaustion DoS via oversized WebSocket frames.

const WS_CHANNEL_FILES: &[(&str, &str)] = &[
    ("discord", include_str!("../src/channels/discord.rs")),
    ("lark", include_str!("../src/channels/lark.rs")),
    ("dingtalk", include_str!("../src/channels/dingtalk.rs")),
    ("qq", include_str!("../src/channels/qq.rs")),
];

#[test]
fn all_ws_channels_use_connect_async_with_config() {
    for (name, source) in WS_CHANNEL_FILES {
        // Must use the config variant
        assert!(
            source.contains("connect_async_with_config"),
            "{name}.rs must use connect_async_with_config (not bare connect_async) \
             to enforce WebSocket message size limits"
        );
    }
}

#[test]
fn all_ws_channels_set_max_message_size() {
    for (name, source) in WS_CHANNEL_FILES {
        assert!(
            source.contains("max_message_size"),
            "{name}.rs must set max_message_size on WebSocketConfig"
        );
    }
}

#[test]
fn all_ws_channels_set_max_frame_size() {
    for (name, source) in WS_CHANNEL_FILES {
        assert!(
            source.contains("max_frame_size"),
            "{name}.rs must set max_frame_size on WebSocketConfig"
        );
    }
}

#[test]
fn no_ws_channel_uses_bare_connect_async() {
    for (name, source) in WS_CHANNEL_FILES {
        // Count occurrences of connect_async that are NOT connect_async_with_config
        for (line_num, line) in source.lines().enumerate() {
            if line.contains("connect_async") && !line.contains("connect_async_with_config") {
                // Allow comments and string literals
                let trimmed = line.trim();
                if trimmed.starts_with("//") || trimmed.starts_with("///") {
                    continue;
                }
                panic!(
                    "{name}.rs line {}: bare connect_async found — must use connect_async_with_config: {}",
                    line_num + 1,
                    trimmed
                );
            }
        }
    }
}
