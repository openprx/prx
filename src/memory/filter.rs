/// Filter out content that should not be auto-saved to memory.
/// Heartbeat prompts, cron triggers, trivial acks, and very short messages are noise.
pub fn should_autosave_content(content: &str) -> bool {
    let noise_patterns = [
        "HEARTBEAT",
        "heartbeat",
        "Check HEARTBEAT",
        "[cron:",
        "[Heartbeat Task]",
        "心跳",
        "系统健康",
        "HEARTBEAT_OK",
        "NO_REPLY",
        "no_reply",
    ];
    if noise_patterns.iter().any(|p| content.contains(p)) {
        return false;
    }
    if content.chars().count() < 30 {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::should_autosave_content;

    #[test]
    fn filters_heartbeat_and_cron_noise() {
        assert!(!should_autosave_content("Check HEARTBEAT now"));
        assert!(!should_autosave_content("[cron:heartbeat] run task"));
        assert!(!should_autosave_content("系统健康检查完成 HEARTBEAT_OK"));
    }

    #[test]
    fn filters_very_short_messages() {
        assert!(!should_autosave_content("ok"));
        assert!(!should_autosave_content("thanks, got it"));
    }

    #[test]
    fn allows_meaningful_user_content() {
        let content = "Need you to remember my preferred deployment window after 10pm local time.";
        assert!(should_autosave_content(content));
    }
}
