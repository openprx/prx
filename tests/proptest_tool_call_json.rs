use openprx::providers::ToolCall;
use proptest::prelude::*;

proptest! {
    #[test]
    fn tool_call_json_roundtrip_preserves_fields(
        id in "[a-zA-Z0-9_-]{1,64}",
        name in "[a-z][a-z0-9_]{0,63}",
        arguments in "\\PC{0,256}",
    ) {
        let call = ToolCall {
            id,
            name,
            arguments,
        };

        let encoded = serde_json::to_string(&call)?;
        let decoded: ToolCall = serde_json::from_str(&encoded)?;

        prop_assert_eq!(decoded.id, call.id);
        prop_assert_eq!(decoded.name, call.name);
        prop_assert_eq!(decoded.arguments, call.arguments);
    }
}
