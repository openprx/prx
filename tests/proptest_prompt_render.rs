use openprx::identity::{AieosIdentity, aieos_to_system_prompt};
use proptest::prelude::*;

proptest! {
    #[test]
    fn identity_prompt_render_never_panics_for_default_identity(repeats in 0_usize..32) {
        let mut rendered = String::new();
        for _ in 0..repeats {
            rendered.push_str(&aieos_to_system_prompt(&AieosIdentity::default()));
        }

        prop_assert!(rendered.len() <= repeats.saturating_mul(16_384));
    }
}
