#![cfg(test)]

use super::*;

#[test]
fn backend_key_from_choice_maps_supported_backends() {
    assert_eq!(backend_key_from_choice(0), "sqlite");
    assert_eq!(backend_key_from_choice(1), "lucid");
    assert_eq!(backend_key_from_choice(2), "markdown");
    assert_eq!(backend_key_from_choice(3), "none");
    assert_eq!(backend_key_from_choice(999), "sqlite");
}

#[test]
fn memory_backend_profile_marks_lucid_as_optional_sqlite_backed() {
    let lucid = memory_backend_profile("lucid");
    assert!(lucid.auto_save_default);
    assert!(lucid.uses_sqlite_hygiene);
    assert!(lucid.sqlite_based);
    assert!(lucid.optional_dependency);

    let markdown = memory_backend_profile("markdown");
    assert!(markdown.auto_save_default);
    assert!(!markdown.uses_sqlite_hygiene);

    let none = memory_backend_profile("none");
    assert!(!none.auto_save_default);
    assert!(!none.uses_sqlite_hygiene);

    let custom = memory_backend_profile("custom-memory");
    assert!(custom.auto_save_default);
    assert!(!custom.uses_sqlite_hygiene);
}

#[test]
fn memory_config_defaults_for_lucid_enable_sqlite_hygiene() {
    let config = memory_config_defaults_for_backend("lucid");
    assert_eq!(config.backend, "lucid");
    assert!(config.auto_save);
    assert_eq!(config.embedding_provider, "local");
    assert_eq!(config.embedding_model, "prx-local-hash-v1");
    assert_eq!(config.embedding_dimensions, 384);
    assert_eq!(config.archive_after_days, 7);
    assert_eq!(config.purge_after_days, 30);
    assert_eq!(config.embedding_cache_size, 10000);
}

#[test]
fn memory_config_defaults_for_none_keep_safe_hygiene_retention() {
    let config = memory_config_defaults_for_backend("none");
    assert_eq!(config.backend, "none");
    assert!(!config.auto_save);
    assert_eq!(config.embedding_provider, "none");
    assert_eq!(config.embedding_dimensions, 0);
    assert_eq!(config.archive_after_days, 7);
    assert_eq!(config.purge_after_days, 30);
    assert_eq!(config.embedding_cache_size, 0);
}

#[test]
fn launchable_channels_include_mattermost_and_qq() {
    let mut channels = ChannelsConfig::default();
    assert!(!has_launchable_channels(&channels));

    channels.mattermost = Some(crate::config::schema::MattermostConfig {
        url: "https://mattermost.example.com".into(),
        bot_token: "token".into(),
        channel_id: Some("channel".into()),
        allowed_users: vec!["*".into()],
        thread_replies: Some(true),
        mention_only: Some(false),
    });
    assert!(has_launchable_channels(&channels));

    channels.mattermost = None;
    channels.qq = Some(crate::config::schema::QQConfig {
        app_id: "app-id".into(),
        app_secret: "app-secret".into(),
        allowed_users: vec!["*".into()],
        mention_only: false,
    });
    assert!(has_launchable_channels(&channels));
}
