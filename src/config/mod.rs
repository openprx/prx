pub mod files;
pub mod hotreload;
pub mod schema;

pub use hotreload::{HotReloadManager, SharedConfig, new_shared};

#[allow(unused_imports)]
pub use schema::{
    AgentCompactionConfig, AgentCompactionMode, AgentConfig, AuditConfig, AuthConfig, AutomixConfig, AutonomyConfig,
    BrowserComputerUseConfig, BrowserConfig, ChannelsConfig, ClassificationRule, ComposioConfig, Config, CostConfig,
    CronConfig, DelegateAgentConfig, DiscordConfig, DmPolicy, DockerRuntimeConfig, EmbeddingRouteConfig, GatewayConfig,
    GroupPolicy, HeartbeatConfig, HttpRequestConfig, IMessageConfig, IdentityBindingConfig, IdentityConfig, LarkConfig,
    MatrixConfig, McpConfig, McpServerConfig, McpTransport, MediaConfig, MemoryConfig, MemoryWebhookConfig,
    ModelRouteConfig, MultimodalConfig, NextcloudTalkConfig, NodeServerConfig, NodesConfig, ObservabilityConfig,
    ProxyConfig, ProxyScope, QueryClassificationConfig, ReliabilityConfig, RemoteNodeConfig, ResourceLimitsConfig,
    RouterConfig, RouterModelConfig, RuntimeConfig, SandboxBackend, SandboxConfig, SchedulerConfig, ScopeConfig,
    ScopeRule, SecretsConfig, SecurityConfig, SelfSystemConfig, SessionsSpawnConfig, SkillsConfig, SlackConfig,
    StorageConfig, StorageProviderConfig, StorageProviderSection, StreamMode, TaskRoutingConfig,
    TaskRoutingIntentConfig, TaskRoutingRule, TelegramConfig, ToolPolicyConfig, TunnelConfig, UserPolicyConfig,
    WebSearchConfig, WebhookConfig, apply_runtime_proxy_to_builder, build_runtime_proxy_client,
    build_runtime_proxy_client_with_timeouts, runtime_proxy_config, set_runtime_proxy_config,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reexported_config_default_is_constructible() {
        let config = Config::default();

        assert!(config.default_provider.is_some());
        assert!(config.default_model.is_some());
        assert!(config.default_temperature > 0.0);
    }

    #[test]
    fn reexported_channel_configs_are_constructible() {
        let telegram = TelegramConfig {
            bot_token: "token".into(),
            allowed_users: vec!["alice".into()],
            stream_mode: StreamMode::default(),
            draft_update_interval_ms: 1000,
            interrupt_on_new_message: false,
            mention_only: false,
        };

        let discord = DiscordConfig {
            bot_token: "token".into(),
            guild_id: Some("123".into()),
            allowed_users: vec![],
            listen_to_bots: false,
            mention_only: false,
        };

        let lark = LarkConfig {
            app_id: "app-id".into(),
            app_secret: "app-secret".into(),
            encrypt_key: None,
            verification_token: None,
            allowed_users: vec![],
            use_feishu: false,
            receive_mode: crate::config::schema::LarkReceiveMode::Websocket,
            port: None,
            mention_only: false,
        };

        let nextcloud_talk = NextcloudTalkConfig {
            base_url: "https://cloud.example.com".into(),
            app_token: "app-token".into(),
            webhook_secret: None,
            allowed_users: vec!["*".into()],
            mention_only: false,
        };

        assert_eq!(telegram.allowed_users.len(), 1);
        assert_eq!(discord.guild_id.as_deref(), Some("123"));
        assert!(!lark.use_feishu);
        assert_eq!(nextcloud_talk.allowed_users, vec!["*"]);
    }
}
