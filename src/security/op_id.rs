use sha2::{Digest, Sha256};
use std::path::Path;

#[must_use]
pub fn fingerprint16(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    let mut first = [0_u8; 8];
    for (slot, byte) in first.iter_mut().zip(digest.iter().take(8)) {
        *slot = *byte;
    }
    hex::encode(first)
}

#[must_use]
pub fn ref_for_file(path: &Path) -> String {
    fingerprint16(&path.to_string_lossy())
}

#[must_use]
pub fn ref_for_url_host(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(normalize_ref_segment))
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

#[must_use]
pub fn ref_for_channel_recipient(channel: &str, recipient: &str) -> String {
    fingerprint16(&format!("{channel}:{recipient}"))
}

#[must_use]
pub fn ref_for_owner(owner_id: &str) -> String {
    normalize_ref_segment(owner_id)
}

#[must_use]
pub fn op_id(tool_module: &str, action: &str, refs: &[&str]) -> String {
    let mut value = format!(
        "{}:{}",
        normalize_ref_segment(tool_module),
        normalize_ref_segment(action)
    );
    for reference in refs {
        let reference = normalize_ref_segment(reference);
        if !reference.is_empty() {
            value.push(':');
            value.push_str(&reference);
        }
    }
    value
}

fn normalize_ref_segment(value: &str) -> String {
    let normalized = value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    normalized.trim_matches('_').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint16_is_stable_hex() {
        assert_eq!(fingerprint16("abc").len(), 16);
        assert_eq!(fingerprint16("abc"), fingerprint16("abc"));
        assert_ne!(fingerprint16("abc"), fingerprint16("abcd"));
    }

    #[test]
    fn op_id_normalizes_segments() {
        assert_eq!(
            op_id("Message_Send", "send", &["Signal:User+1"]),
            "message_send:send:signal_user_1"
        );
    }
}
