#![cfg(test)]

/// USERINFO-2: the chat banner is the first line of every session; a
/// `custom:` URL may carry `user:password@`.
#[test]
fn the_banner_names_the_provider_without_url_userinfo() {
    let banner = super::startup_banner("custom:http://u:SECRET-fixture-pw@127.0.0.1:1/v1", "fixture-model");
    assert!(
        banner.contains("custom:http://[redacted]@127.0.0.1:1/v1/fixture-model"),
        "{banner}"
    );
    assert!(!banner.contains("SECRET-fixture-pw"), "{banner}");
}
