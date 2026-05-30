use openprx::llm::route_decision::{RouteDecision, SelectionStrategy};
use proptest::prelude::*;

proptest! {
    #[test]
    fn route_decision_single_candidate_preserves_selection(
        provider in "[a-z][a-z0-9_-]{0,31}",
        model in "[a-z][a-z0-9_./:-]{0,63}",
        owner_id in "owner:[a-z0-9_-]{1,32}",
        session_key in "session:[a-z0-9_-]{1,32}",
        intent in "[a-z_]{1,32}",
        estimated_tokens in 0_u32..1_000_000,
        require_tools in any::<bool>(),
        require_streaming in any::<bool>(),
    ) {
        let decision = RouteDecision::single_candidate_for_context(
            provider.clone(),
            model.clone(),
            owner_id.clone(),
            session_key.clone(),
            None,
            None,
            intent.clone(),
            estimated_tokens,
            require_tools,
            require_streaming,
        );

        prop_assert_eq!(decision.owner_id, owner_id);
        prop_assert_eq!(decision.session_key, session_key);
        prop_assert_eq!(decision.intent, intent);
        prop_assert_eq!(decision.estimated_tokens, estimated_tokens);
        prop_assert_eq!(decision.candidates.len(), 1);
        prop_assert_eq!(decision.selected.provider, provider);
        prop_assert_eq!(decision.selected.model, model);
        prop_assert_eq!(decision.selected.strategy, SelectionStrategy::FallbackDefault);
        prop_assert_eq!(decision.constraints.require_tools, require_tools);
        prop_assert_eq!(decision.constraints.require_streaming, require_streaming);
    }
}
