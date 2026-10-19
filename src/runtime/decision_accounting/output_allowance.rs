//! Output headroom for a request whose input exceeds its fair retry share.
pub(super) fn request_output_cap(text: &str) -> anyhow::Result<Option<u64>> {
    let Ok(body) = serde_json::from_str::<serde_json::Value>(text) else {
        return Ok(None);
    };
    let fields = [
        body.get("max_tokens"),
        body.get("max_completion_tokens"),
        body.get("max_output_tokens"),
        body.pointer("/generationConfig/maxOutputTokens"),
    ];
    let mut cap = None;
    for value in fields.into_iter().flatten() {
        let tokens = value
            .as_u64()
            .filter(|tokens| *tokens > 0)
            .ok_or_else(|| anyhow::anyhow!("physical request has an invalid output token cap"))?;
        cap = Some(cap.unwrap_or(0).max(tokens));
    }
    Ok(cap)
}

#[cfg(test)]
mod tests {
    use super::super::claim_input_tokens;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn an_oversized_input_reserves_its_output_cap_and_clamps_only_to_available_tokens() {
        for (field, cap, expected) in [
            ("max_tokens", 200, 1236),
            ("max_completion_tokens", 400, 1436),
            ("max_output_tokens", 2048, 2059),
        ] {
            let remaining = AtomicU64::new(2059);
            let text = serde_json::json!({"input": "large input ".repeat(1024), (field): cap}).to_string();
            assert_eq!(
                claim_input_tokens(&remaining, 2, &text, || 1036).expect("headroom fits"),
                expected
            );
            assert_eq!(remaining.load(Ordering::Acquire), 2059 - expected);
        }
    }

    #[test]
    fn an_invalid_output_cap_does_not_release_a_minimal_estimate_to_transport() {
        let remaining = AtomicU64::new(2059);
        let text = serde_json::json!({"input": "large input ".repeat(1024), "max_tokens": "200"}).to_string();
        assert!(claim_input_tokens(&remaining, 2, &text, || 1036).is_err());
        assert_eq!(remaining.load(Ordering::Acquire), 2059);
    }
}
