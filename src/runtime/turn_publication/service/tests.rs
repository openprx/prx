use super::*;

#[test]
fn pinned_policy_version_is_order_independent_and_tracks_policy_changes() {
    let key = crate::security::runtime_key::test_support::ephemeral_key();
    let security = SecurityPolicy::default();
    let surface = SurfacePolicy::default();
    let mut first = crate::config::Config::default();
    let mut second = first.clone();
    let a = crate::config::schema::ModelPricing {
        input: 1.0,
        output: 2.0,
        cache_write: 0.5,
        cache_read: 0.25,
    };
    let b = crate::config::schema::ModelPricing {
        input: 3.0,
        output: 4.0,
        cache_write: 1.0,
        cache_read: 0.5,
    };
    first.cost.prices.insert("provider/a".to_string(), a.clone());
    first.cost.prices.insert("provider/b".to_string(), b.clone());
    second.cost.prices.insert("provider/b".to_string(), b);
    second.cost.prices.insert("provider/a".to_string(), a);
    first
        .tool_tiering
        .model_allowlists
        .insert("model/a".to_string(), vec!["tool/b".to_string(), "tool/a".to_string()]);
    second
        .tool_tiering
        .model_allowlists
        .insert("model/a".to_string(), vec!["tool/a".to_string(), "tool/b".to_string()]);
    let stable = pinned_policy_version(&first, &security, &surface, &key).unwrap();
    assert_eq!(
        stable,
        pinned_policy_version(&second, &security, &surface, &key).unwrap()
    );
    assert!(stable.starts_with("cdp-policy-v1:"));
    assert!(!stable.contains("provider/a"));
    second.cost.daily_limit_usd += 1.0;
    assert_ne!(
        stable,
        pinned_policy_version(&second, &security, &surface, &key).unwrap()
    );
    second.cost.daily_limit_usd -= 1.0;
    second.cost.prices.get_mut("provider/a").unwrap().cache_read += 0.25;
    assert_ne!(
        stable,
        pinned_policy_version(&second, &security, &surface, &key).unwrap()
    );
    let mut changed_security = security;
    changed_security.scope_default_allow = !changed_security.scope_default_allow;
    assert_ne!(
        stable,
        pinned_policy_version(&first, &changed_security, &surface, &key).unwrap()
    );
}
