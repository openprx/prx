//! Channel status, channel CLI commands and health checks.

use super::service::bind_telegram_identity;
use super::{
    Arc, Channel, Config, DingTalkChannel, DiscordChannel, Duration, EmailChannel, IMessageChannel, IrcChannel,
    LarkChannel, LinqChannel, MatrixChannel, NextcloudTalkChannel, QQChannel, Result, SignalChannel,
    SignalNativeChannel, SlackChannel, TelegramChannel, WacliChannel, WhatsAppChannel, WhatsAppWebChannel, irc, wacli,
};

/// Return the configured state of every real-time channel supported by this build.
///
/// CLI diagnostics share this list so newly added channels cannot disappear from
/// one status command while remaining visible in another.
pub fn channel_configuration_statuses(config: &Config) -> Vec<(&'static str, bool)> {
    vec![
        ("Telegram", config.channels_config.telegram.is_some()),
        ("Discord", config.channels_config.discord.is_some()),
        ("Slack", config.channels_config.slack.is_some()),
        ("Mattermost", config.channels_config.mattermost.is_some()),
        ("Webhook", config.channels_config.webhook.is_some()),
        ("iMessage", config.channels_config.imessage.is_some()),
        (
            "Matrix",
            cfg!(feature = "channel-matrix") && config.channels_config.matrix.is_some(),
        ),
        ("Signal", config.channels_config.signal.is_some()),
        ("WhatsApp", config.channels_config.whatsapp.is_some()),
        ("Wacli", config.channels_config.wacli.is_some()),
        ("Linq", config.channels_config.linq.is_some()),
        ("Nextcloud Talk", config.channels_config.nextcloud_talk.is_some()),
        ("Email", config.channels_config.email.is_some()),
        ("IRC", config.channels_config.irc.is_some()),
        ("Lark", config.channels_config.lark.is_some()),
        ("DingTalk", config.channels_config.dingtalk.is_some()),
        ("QQ", config.channels_config.qq.is_some()),
    ]
}

pub async fn handle_command(command: crate::ChannelCommands, config: &Config) -> Result<()> {
    match command {
        crate::ChannelCommands::Start => {
            anyhow::bail!("Start must be handled in main.rs (requires async runtime)")
        }
        crate::ChannelCommands::Doctor => {
            anyhow::bail!("Doctor must be handled in main.rs (requires async runtime)")
        }
        crate::ChannelCommands::List => {
            println!("Channels:");
            println!("  ✅ CLI (always available)");
            for (name, configured) in channel_configuration_statuses(config) {
                println!("  {} {name}", if configured { "✅" } else { "❌" });
            }
            if !cfg!(feature = "channel-matrix") {
                println!("  ℹ️ Matrix channel support is disabled in this build (enable `channel-matrix`).");
            }
            println!("\nTo start channels: prx channel start");
            println!("To check health:    prx channel doctor");
            println!("To configure:      prx onboard");
            Ok(())
        }
        crate::ChannelCommands::Add {
            channel_type,
            config: _,
        } => {
            anyhow::bail!("Channel type '{channel_type}' — use `prx onboard` to configure channels");
        }
        crate::ChannelCommands::Remove { name } => {
            anyhow::bail!("Remove channel '{name}' — edit ~/.openprx/config.toml directly");
        }
        crate::ChannelCommands::BindTelegram { identity } => bind_telegram_identity(config, &identity).await,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChannelHealthState {
    Healthy,
    Unhealthy,
    Timeout,
}

pub(super) const fn classify_health_result(
    result: &std::result::Result<bool, tokio::time::error::Elapsed>,
) -> ChannelHealthState {
    match result {
        Ok(true) => ChannelHealthState::Healthy,
        Ok(false) => ChannelHealthState::Unhealthy,
        Err(_) => ChannelHealthState::Timeout,
    }
}

/// Run health checks for configured channels.
pub async fn doctor_channels(config: Config) -> Result<()> {
    let mut channels: Vec<(&'static str, Arc<dyn Channel>)> = Vec::new();

    if let Some(ref tg) = config.channels_config.telegram {
        channels.push((
            "Telegram",
            Arc::new(
                TelegramChannel::new(tg.bot_token.clone(), tg.allowed_users.clone(), tg.mention_only)
                    .with_streaming(tg.stream_mode, tg.draft_update_interval_ms)
                    .with_group_reply_mode(crate::config::GroupReplyMode::resolve(
                        tg.group_reply_mode,
                        tg.mention_only,
                    )),
            ),
        ));
    }

    if let Some(ref dc) = config.channels_config.discord {
        channels.push((
            "Discord",
            Arc::new(
                DiscordChannel::new(
                    dc.bot_token.clone(),
                    dc.guild_id.clone(),
                    dc.allowed_users.clone(),
                    dc.listen_to_bots,
                    dc.mention_only,
                )
                .with_group_reply_mode(crate::config::GroupReplyMode::resolve(
                    dc.group_reply_mode,
                    dc.mention_only,
                )),
            ),
        ));
    }

    if let Some(ref sl) = config.channels_config.slack {
        channels.push((
            "Slack",
            Arc::new(SlackChannel::new(
                sl.bot_token.clone(),
                sl.channel_id.clone(),
                sl.allowed_users.clone(),
            )),
        ));
    }

    if let Some(ref im) = config.channels_config.imessage {
        channels.push(("iMessage", Arc::new(IMessageChannel::new(im.allowed_contacts.clone()))));
    }

    #[cfg(feature = "channel-matrix")]
    if let Some(ref mx) = config.channels_config.matrix {
        channels.push((
            "Matrix",
            Arc::new(MatrixChannel::new_with_session_hint(
                mx.homeserver.clone(),
                mx.access_token.clone(),
                mx.room_id.clone(),
                mx.allowed_users.clone(),
                mx.user_id.clone(),
                mx.device_id.clone(),
            )),
        ));
    }

    #[cfg(not(feature = "channel-matrix"))]
    if config.channels_config.matrix.is_some() {
        tracing::warn!(
            "Matrix channel is configured but this build was compiled without `channel-matrix`; skipping Matrix health check."
        );
    }

    if let Some(ref sig) = config.channels_config.signal {
        let media_artifacts = crate::media::MediaArtifactOwner::for_workspace(&config.workspace_dir);
        let signal_channel: Arc<dyn Channel + Send + Sync> = if sig.is_native_mode() {
            Arc::new(
                SignalNativeChannel::new(
                    sig.cli_path.clone().unwrap_or_else(|| "signal-cli".to_string()),
                    sig.account.clone(),
                    sig.data_dir.clone(),
                    sig.daemon_http_port.unwrap_or(16866),
                    sig.startup_timeout_ms,
                    sig.group_id.clone(),
                    sig.allowed_from.clone(),
                    sig.ignore_attachments,
                    sig.ignore_stories,
                    config.media.clone(),
                    sig.storm_protection.clone(),
                )
                .with_artifact_owner(media_artifacts),
            )
        } else {
            Arc::new(
                SignalChannel::new_with_storm_protection(
                    sig.effective_http_url(),
                    sig.account.clone(),
                    sig.group_id.clone(),
                    sig.allowed_from.clone(),
                    sig.ignore_attachments,
                    sig.ignore_stories,
                    config.media.clone(),
                    sig.storm_protection.clone(),
                )
                .with_artifact_owner(media_artifacts),
            )
        };
        channels.push(("Signal", signal_channel));
    }

    if let Some(ref wa) = config.channels_config.whatsapp {
        if wa.is_ambiguous_config() {
            tracing::warn!(
                "WhatsApp config has both phone_number_id and session_path set; preferring Cloud API mode. Remove one selector to avoid ambiguity."
            );
        }
        // Runtime negotiation: detect backend type from config
        match wa.backend_type() {
            "cloud" => {
                // Cloud API mode: requires phone_number_id, access_token, verify_token
                if wa.is_cloud_config() {
                    channels.push((
                        "WhatsApp",
                        Arc::new(WhatsAppChannel::new(
                            wa.access_token.clone().unwrap_or_default(),
                            wa.phone_number_id.clone().unwrap_or_default(),
                            wa.verify_token.clone().unwrap_or_default(),
                            wa.allowed_numbers.clone(),
                        )),
                    ));
                } else {
                    tracing::warn!(
                        "WhatsApp Cloud API configured but missing required fields (phone_number_id, access_token, verify_token)"
                    );
                }
            }
            "web" => {
                // Web mode: requires session_path
                #[cfg(feature = "whatsapp-web")]
                if wa.is_web_config() {
                    channels.push((
                        "WhatsApp",
                        Arc::new(
                            WhatsAppWebChannel::new(
                                wa.session_path.clone().unwrap_or_default(),
                                wa.pair_phone.clone(),
                                wa.pair_code.clone(),
                                wa.allowed_numbers.clone(),
                            )
                            .with_ws_url(wa.ws_url.clone())
                            .with_read_pool_size(wa.read_pool_size),
                        ),
                    ));
                } else {
                    tracing::warn!("WhatsApp Web configured but session_path not set");
                }
                #[cfg(not(feature = "whatsapp-web"))]
                {
                    tracing::warn!(
                        "WhatsApp Web backend requires 'whatsapp-web' feature. Enable with: cargo build --features whatsapp-web"
                    );
                }
            }
            _ => {
                tracing::warn!(
                    "WhatsApp config invalid: neither phone_number_id (Cloud API) nor session_path (Web) is set"
                );
            }
        }
    }

    if let Some(ref wc) = config.channels_config.wacli {
        channels.push((
            "Wacli",
            Arc::new(WacliChannel::new(wacli::WacliChannelConfig {
                webhook_listen: wc.webhook_listen.clone(),
                webhook_path: wc.webhook_path.clone(),
                webhook_secret: wc.webhook_secret.clone(),
                allow_unsigned_loopback: wc.allow_unsigned_loopback,
                allowed_from: wc.allowed_from.clone(),
                cli_path: wc.cli_path.clone(),
                store_dir: wc.store_dir.clone(),
                bot_jid: wc.bot_jid.clone(),
                bot_number: wc.bot_number.clone(),
                bot_lid: wc.bot_lid.clone(),
                self_chat_mode: wc.self_chat_mode,
                outbound_media_workspace_only: wc.outbound_media_workspace_only,
            })),
        ));
    }

    if let Some(ref lq) = config.channels_config.linq {
        channels.push((
            "Linq",
            Arc::new(LinqChannel::new(
                lq.api_token.clone(),
                lq.from_phone.clone(),
                lq.allowed_senders.clone(),
            )),
        ));
    }

    if let Some(ref nc) = config.channels_config.nextcloud_talk {
        channels.push((
            "Nextcloud Talk",
            Arc::new(NextcloudTalkChannel::new(
                nc.base_url.clone(),
                nc.app_token.clone(),
                nc.allowed_users.clone(),
            )),
        ));
    }

    if let Some(ref email_cfg) = config.channels_config.email {
        channels.push(("Email", Arc::new(EmailChannel::new(email_cfg.clone()))));
    }

    if let Some(ref irc) = config.channels_config.irc {
        channels.push((
            "IRC",
            Arc::new(IrcChannel::new(irc::IrcChannelConfig {
                server: irc.server.clone(),
                port: irc.port,
                nickname: irc.nickname.clone(),
                username: irc.username.clone(),
                channels: irc.channels.clone(),
                allowed_users: irc.allowed_users.clone(),
                server_password: irc.server_password.clone(),
                nickserv_password: irc.nickserv_password.clone(),
                sasl_password: irc.sasl_password.clone(),
                verify_tls: irc.verify_tls.unwrap_or(true),
            })),
        ));
    }

    if let Some(ref lk) = config.channels_config.lark {
        channels.push(("Lark", Arc::new(LarkChannel::from_config(lk))));
    }

    if let Some(ref dt) = config.channels_config.dingtalk {
        channels.push((
            "DingTalk",
            Arc::new(DingTalkChannel::new(
                dt.client_id.clone(),
                dt.client_secret.clone(),
                dt.allowed_users.clone(),
            )),
        ));
    }

    if let Some(ref qq) = config.channels_config.qq {
        channels.push((
            "QQ",
            Arc::new(QQChannel::new(
                qq.app_id.clone(),
                qq.app_secret.clone(),
                qq.allowed_users.clone(),
            )),
        ));
    }

    if channels.is_empty() {
        println!("No real-time channels configured. Run `prx onboard` first.");
        return Ok(());
    }

    println!("🩺 OpenPRX Channel Doctor");
    println!();

    let mut healthy = 0_u32;
    let mut unhealthy = 0_u32;
    let mut timeout = 0_u32;

    for (name, channel) in channels {
        let result = tokio::time::timeout(Duration::from_secs(10), channel.health_check()).await;
        let state = classify_health_result(&result);

        match state {
            ChannelHealthState::Healthy => {
                healthy += 1;
                println!("  ✅ {name:<9} healthy");
            }
            ChannelHealthState::Unhealthy => {
                unhealthy += 1;
                println!("  ❌ {name:<9} unhealthy (auth/config/network)");
            }
            ChannelHealthState::Timeout => {
                timeout += 1;
                println!("  ⏱️  {name:<9} timed out (>10s)");
            }
        }
    }

    if config.channels_config.webhook.is_some() {
        println!("  ℹ️  Webhook   check via `prx gateway` then GET /health");
    }

    println!();
    println!("Summary: {healthy} healthy, {unhealthy} unhealthy, {timeout} timed out");
    Ok(())
}
