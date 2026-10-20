#[cfg(feature = "channel-matrix")]
use super::MatrixConfig;
use super::{
    ChannelsConfig, Confirm, DingTalkConfig, DiscordConfig, Duration, IMessageConfig, Input, IrcConfig, LarkConfig,
    LarkReceiveMode, LinqConfig, QQConfig, Result, Select, SlackConfig, StreamMode, TelegramConfig, Value,
    WebhookConfig, WhatsAppConfig, print_bullet, style,
};

// ── Step 3: Channels ────────────────────────────────────────────

#[allow(clippy::too_many_lines)]
// Every channel connection probe below runs its blocking HTTP on a dedicated thread and joins it,
// which keeps `reqwest::blocking` off the Tokio worker that awaits `run_wizard`. The join itself
// still parks that worker for the round trip, and the probe clients carry no timeout.
// Known limit: moving the probes onto `runtime::blocking::spawn_blocking` and giving them explicit
// timeouts would require making `setup_channels` async, which is a separate concern from keeping
// the blocking client off the worker.
pub(super) fn setup_channels() -> Result<ChannelsConfig> {
    print_bullet("Channels let you talk to OpenPRX from anywhere.");
    print_bullet("CLI is always available. Connect more channels now.");
    println!();

    let mut config = ChannelsConfig::default();
    #[derive(Clone, Copy)]
    enum ChannelMenuChoice {
        Telegram,
        Discord,
        Slack,
        IMessage,
        #[cfg(feature = "channel-matrix")]
        Matrix,
        WhatsApp,
        Linq,
        Irc,
        Webhook,
        DingTalk,
        QqOfficial,
        LarkFeishu,
        Done,
    }
    let menu_choices = [
        ChannelMenuChoice::Telegram,
        ChannelMenuChoice::Discord,
        ChannelMenuChoice::Slack,
        ChannelMenuChoice::IMessage,
        #[cfg(feature = "channel-matrix")]
        ChannelMenuChoice::Matrix,
        ChannelMenuChoice::WhatsApp,
        ChannelMenuChoice::Linq,
        ChannelMenuChoice::Irc,
        ChannelMenuChoice::Webhook,
        ChannelMenuChoice::DingTalk,
        ChannelMenuChoice::QqOfficial,
        ChannelMenuChoice::LarkFeishu,
        ChannelMenuChoice::Done,
    ];

    loop {
        let options: Vec<String> = menu_choices
            .iter()
            .map(|choice| match choice {
                ChannelMenuChoice::Telegram => format!(
                    "Telegram   {}",
                    if config.telegram.is_some() {
                        "✅ connected"
                    } else {
                        "— connect your bot"
                    }
                ),
                ChannelMenuChoice::Discord => format!(
                    "Discord    {}",
                    if config.discord.is_some() {
                        "✅ connected"
                    } else {
                        "— connect your bot"
                    }
                ),
                ChannelMenuChoice::Slack => format!(
                    "Slack      {}",
                    if config.slack.is_some() {
                        "✅ connected"
                    } else {
                        "— connect your bot"
                    }
                ),
                ChannelMenuChoice::IMessage => format!(
                    "iMessage   {}",
                    if config.imessage.is_some() {
                        "✅ configured"
                    } else {
                        "— macOS only"
                    }
                ),
                #[cfg(feature = "channel-matrix")]
                ChannelMenuChoice::Matrix => format!(
                    "Matrix     {}",
                    if config.matrix.is_some() {
                        "✅ connected"
                    } else {
                        "— self-hosted chat"
                    }
                ),
                ChannelMenuChoice::WhatsApp => format!(
                    "WhatsApp   {}",
                    if config.whatsapp.is_some() {
                        "✅ connected"
                    } else {
                        "— Business Cloud API"
                    }
                ),
                ChannelMenuChoice::Linq => format!(
                    "Linq       {}",
                    if config.linq.is_some() {
                        "✅ connected"
                    } else {
                        "— iMessage/RCS/SMS via Linq API"
                    }
                ),
                ChannelMenuChoice::Irc => format!(
                    "IRC        {}",
                    if config.irc.is_some() {
                        "✅ configured"
                    } else {
                        "— IRC over TLS"
                    }
                ),
                ChannelMenuChoice::Webhook => format!(
                    "Webhook    {}",
                    if config.webhook.is_some() {
                        "✅ configured"
                    } else {
                        "— HTTP endpoint"
                    }
                ),
                ChannelMenuChoice::DingTalk => format!(
                    "DingTalk   {}",
                    if config.dingtalk.is_some() {
                        "✅ connected"
                    } else {
                        "— DingTalk Stream Mode"
                    }
                ),
                ChannelMenuChoice::QqOfficial => format!(
                    "QQ Official {}",
                    if config.qq.is_some() {
                        "✅ connected"
                    } else {
                        "— Tencent QQ Bot"
                    }
                ),
                ChannelMenuChoice::LarkFeishu => format!(
                    "Lark/Feishu {}",
                    if config.lark.is_some() {
                        "✅ connected"
                    } else {
                        "— Lark/Feishu Bot"
                    }
                ),
                ChannelMenuChoice::Done => "Done — finish setup".to_string(),
            })
            .collect();

        let selection = Select::new()
            .with_prompt("  Connect a channel (or Done to continue)")
            .items(&options)
            .default(options.len() - 1)
            .interact()?;

        let choice = menu_choices.get(selection).copied().unwrap_or(ChannelMenuChoice::Done);

        match choice {
            ChannelMenuChoice::Telegram => {
                // ── Telegram ──
                println!();
                println!(
                    "  {} {}",
                    style("Telegram Setup").white().bold(),
                    style("— talk to OpenPRX from Telegram").dim()
                );
                print_bullet("1. Open Telegram and message @BotFather");
                print_bullet("2. Send /newbot and follow the prompts");
                print_bullet("3. Copy the bot token and paste it below");
                println!();

                let token: String = Input::new()
                    .with_prompt("  Bot token (from @BotFather)")
                    .interact_text()?;

                if token.trim().is_empty() {
                    println!("  {} Skipped", style("→").dim());
                    continue;
                }

                // Test connection (run entirely in separate thread — reqwest::blocking Response
                // must be used and dropped there to avoid "Cannot drop a runtime" panic)
                print!("  {} Testing connection... ", style("⏳").dim());
                let token_clone = token.clone();
                let thread_result = std::thread::spawn(move || {
                    let client = reqwest::blocking::Client::new();
                    let url = format!("https://api.telegram.org/bot{token_clone}/getMe");
                    let resp = client.get(&url).send()?;
                    let ok = resp.status().is_success();
                    let data: serde_json::Value = resp.json().unwrap_or_default();
                    let bot_name = data
                        .get("result")
                        .and_then(|r| r.get("username"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown")
                        .to_string();
                    Ok::<_, reqwest::Error>((ok, bot_name))
                })
                .join();
                match thread_result {
                    Ok(Ok((true, bot_name))) => {
                        println!("\r  {} Connected as @{bot_name}        ", style("✅").green().bold());
                    }
                    _ => {
                        println!(
                            "\r  {} Connection failed — check your token and try again",
                            style("❌").red().bold()
                        );
                        continue;
                    }
                }

                print_bullet("Allowlist your own Telegram identity first (recommended for secure + fast setup).");
                print_bullet("Use your @username without '@' (example: argenis), or your numeric Telegram user ID.");
                print_bullet("Use '*' only for temporary open testing.");

                let users_str: String = Input::new()
                    .with_prompt(
                        "  Allowed Telegram identities (comma-separated: username without '@' and/or numeric user ID, '*' for all)",
                    )
                    .allow_empty(true)
                    .interact_text()?;

                let allowed_users = if users_str.trim() == "*" {
                    vec!["*".into()]
                } else {
                    users_str
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                };

                if allowed_users.is_empty() {
                    println!(
                        "  {} No users allowlisted — Telegram inbound messages will be denied until you add your username/user ID or '*'.",
                        style("⚠").yellow().bold()
                    );
                }

                config.telegram = Some(TelegramConfig {
                    bot_token: token,
                    allowed_users,
                    stream_mode: StreamMode::default(),
                    draft_update_interval_ms: 1000,
                    interrupt_on_new_message: false,
                    mention_only: false,
                    group_reply_mode: None,
                });
            }
            ChannelMenuChoice::Discord => {
                // ── Discord ──
                println!();
                println!(
                    "  {} {}",
                    style("Discord Setup").white().bold(),
                    style("— talk to OpenPRX from Discord").dim()
                );
                print_bullet("1. Go to https://discord.com/developers/applications");
                print_bullet("2. Create a New Application → Bot → Copy token");
                print_bullet("3. Enable MESSAGE CONTENT intent under Bot settings");
                print_bullet("4. Invite bot to your server with messages permission");
                println!();

                let token: String = Input::new().with_prompt("  Bot token").interact_text()?;

                if token.trim().is_empty() {
                    println!("  {} Skipped", style("→").dim());
                    continue;
                }

                // Test connection (run entirely in separate thread — Response must be used/dropped there)
                print!("  {} Testing connection... ", style("⏳").dim());
                let token_clone = token.clone();
                let thread_result = std::thread::spawn(move || {
                    let client = reqwest::blocking::Client::new();
                    let resp = client
                        .get("https://discord.com/api/v10/users/@me")
                        .header("Authorization", format!("Bot {token_clone}"))
                        .send()?;
                    let ok = resp.status().is_success();
                    let data: serde_json::Value = resp.json().unwrap_or_default();
                    let bot_name = data
                        .get("username")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown")
                        .to_string();
                    Ok::<_, reqwest::Error>((ok, bot_name))
                })
                .join();
                match thread_result {
                    Ok(Ok((true, bot_name))) => {
                        println!("\r  {} Connected as {bot_name}        ", style("✅").green().bold());
                    }
                    _ => {
                        println!(
                            "\r  {} Connection failed — check your token and try again",
                            style("❌").red().bold()
                        );
                        continue;
                    }
                }

                let guild: String = Input::new()
                    .with_prompt("  Server (guild) ID (optional, Enter to skip)")
                    .allow_empty(true)
                    .interact_text()?;

                print_bullet("Allowlist your own Discord user ID first (recommended).");
                print_bullet(
                    "Get it in Discord: Settings -> Advanced -> Developer Mode (ON), then right-click your profile -> Copy User ID.",
                );
                print_bullet("Use '*' only for temporary open testing.");

                let allowed_users_str: String = Input::new()
                    .with_prompt("  Allowed Discord user IDs (comma-separated, recommended: your own ID, '*' for all)")
                    .allow_empty(true)
                    .interact_text()?;

                let allowed_users = if allowed_users_str.trim().is_empty() {
                    vec![]
                } else {
                    allowed_users_str
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                };

                if allowed_users.is_empty() {
                    println!(
                        "  {} No users allowlisted — Discord inbound messages will be denied until you add IDs or '*'.",
                        style("⚠").yellow().bold()
                    );
                }

                config.discord = Some(DiscordConfig {
                    bot_token: token,
                    guild_id: if guild.is_empty() { None } else { Some(guild) },
                    allowed_users,
                    listen_to_bots: false,
                    mention_only: false,
                    group_reply_mode: None,
                });
            }
            ChannelMenuChoice::Slack => {
                // ── Slack ──
                println!();
                println!(
                    "  {} {}",
                    style("Slack Setup").white().bold(),
                    style("— talk to OpenPRX from Slack").dim()
                );
                print_bullet("1. Go to https://api.slack.com/apps → Create New App");
                print_bullet("2. Add Bot Token Scopes: chat:write, channels:history");
                print_bullet("3. Install to workspace and copy the Bot Token");
                println!();

                let token: String = Input::new().with_prompt("  Bot token (xoxb-...)").interact_text()?;

                if token.trim().is_empty() {
                    println!("  {} Skipped", style("→").dim());
                    continue;
                }

                // Test connection (run entirely in separate thread — Response must be used/dropped there)
                print!("  {} Testing connection... ", style("⏳").dim());
                let token_clone = token.clone();
                let thread_result = std::thread::spawn(move || {
                    let client = reqwest::blocking::Client::new();
                    let resp = client
                        .get("https://slack.com/api/auth.test")
                        .bearer_auth(&token_clone)
                        .send()?;
                    let ok = resp.status().is_success();
                    let data: serde_json::Value = resp.json().unwrap_or_default();
                    let api_ok = data.get("ok").and_then(serde_json::Value::as_bool).unwrap_or(false);
                    let team = data
                        .get("team")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown")
                        .to_string();
                    let err = data
                        .get("error")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown error")
                        .to_string();
                    Ok::<_, reqwest::Error>((ok, api_ok, team, err))
                })
                .join();
                match thread_result {
                    Ok(Ok((true, true, team, _))) => {
                        println!(
                            "\r  {} Connected to workspace: {team}        ",
                            style("✅").green().bold()
                        );
                    }
                    Ok(Ok((true, false, _, err))) => {
                        println!("\r  {} Slack error: {err}", style("❌").red().bold());
                        continue;
                    }
                    _ => {
                        println!("\r  {} Connection failed — check your token", style("❌").red().bold());
                        continue;
                    }
                }

                let app_token: String = Input::new()
                    .with_prompt("  App token (xapp-..., optional, Enter to skip)")
                    .allow_empty(true)
                    .interact_text()?;

                let channel: String = Input::new()
                    .with_prompt("  Default channel ID (optional, Enter to skip)")
                    .allow_empty(true)
                    .interact_text()?;

                print_bullet("Allowlist your own Slack member ID first (recommended).");
                print_bullet("Member IDs usually start with 'U' (open your Slack profile -> More -> Copy member ID).");
                print_bullet("Use '*' only for temporary open testing.");

                let allowed_users_str: String = Input::new()
                    .with_prompt(
                        "  Allowed Slack user IDs (comma-separated, recommended: your own member ID, '*' for all)",
                    )
                    .allow_empty(true)
                    .interact_text()?;

                let allowed_users = if allowed_users_str.trim().is_empty() {
                    vec![]
                } else {
                    allowed_users_str
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                };

                if allowed_users.is_empty() {
                    println!(
                        "  {} No users allowlisted — Slack inbound messages will be denied until you add IDs or '*'.",
                        style("⚠").yellow().bold()
                    );
                }

                config.slack = Some(SlackConfig {
                    bot_token: token,
                    app_token: if app_token.is_empty() { None } else { Some(app_token) },
                    channel_id: if channel.is_empty() { None } else { Some(channel) },
                    allowed_users,
                    mention_only: false,
                });
            }
            ChannelMenuChoice::IMessage => {
                // ── iMessage ──
                println!();
                println!(
                    "  {} {}",
                    style("iMessage Setup").white().bold(),
                    style("— macOS only, reads from Messages.app").dim()
                );

                if !cfg!(target_os = "macos") {
                    println!("  {} iMessage is only available on macOS.", style("⚠").yellow().bold());
                    continue;
                }

                print_bullet("OpenPRX reads your iMessage database and replies via AppleScript.");
                print_bullet("You need to grant Full Disk Access to your terminal in System Settings.");
                println!();

                let contacts_str: String = Input::new()
                    .with_prompt("  Allowed contacts (comma-separated phone/email, or * for all)")
                    .default("*".into())
                    .interact_text()?;

                let allowed_contacts = if contacts_str.trim() == "*" {
                    vec!["*".into()]
                } else {
                    contacts_str.split(',').map(|s| s.trim().to_string()).collect()
                };

                config.imessage = Some(IMessageConfig {
                    allowed_contacts,
                    mention_only: false,
                });
                println!(
                    "  {} iMessage configured (contacts: {})",
                    style("✅").green().bold(),
                    style(&contacts_str).cyan()
                );
            }
            #[cfg(feature = "channel-matrix")]
            ChannelMenuChoice::Matrix => {
                // ── Matrix ──
                println!();
                println!(
                    "  {} {}",
                    style("Matrix Setup").white().bold(),
                    style("— self-hosted, federated chat").dim()
                );
                print_bullet("You need a Matrix account and an access token.");
                print_bullet("Get a token via Element → Settings → Help & About → Access Token.");
                println!();

                let homeserver: String = Input::new()
                    .with_prompt("  Homeserver URL (e.g. https://matrix.org)")
                    .interact_text()?;

                if homeserver.trim().is_empty() {
                    println!("  {} Skipped", style("→").dim());
                    continue;
                }

                let access_token: String = Input::new().with_prompt("  Access token").interact_text()?;

                if access_token.trim().is_empty() {
                    println!("  {} Skipped — token required", style("→").dim());
                    continue;
                }

                // Test connection (run entirely in separate thread — Response must be used/dropped there)
                let hs = homeserver.trim_end_matches('/');
                print!("  {} Testing connection... ", style("⏳").dim());
                let hs_owned = hs.to_string();
                let access_token_clone = access_token.clone();
                let thread_result = std::thread::spawn(move || {
                    let client = reqwest::blocking::Client::new();
                    let resp = client
                        .get(format!("{hs_owned}/_matrix/client/v3/account/whoami"))
                        .header("Authorization", format!("Bearer {access_token_clone}"))
                        .send()?;
                    let ok = resp.status().is_success();

                    if !ok {
                        return Ok::<_, reqwest::Error>((false, None, None));
                    }

                    let payload: Value = resp.json().unwrap_or(Value::Null);
                    let user_id = payload
                        .get("user_id")
                        .and_then(|value| value.as_str())
                        .map(|value| value.to_string());
                    let device_id = payload
                        .get("device_id")
                        .and_then(|value| value.as_str())
                        .map(|value| value.to_string());

                    Ok::<_, reqwest::Error>((true, user_id, device_id))
                })
                .join();

                let (detected_user_id, detected_device_id) = match thread_result {
                    Ok(Ok((true, user_id, device_id))) => {
                        println!("\r  {} Connection verified        ", style("✅").green().bold());

                        if device_id.is_none() {
                            println!(
                                "  {} Homeserver did not return device_id from whoami. If E2EE decryption fails, set channels.matrix.device_id manually in config.toml.",
                                style("⚠️").yellow().bold()
                            );
                        }

                        (user_id, device_id)
                    }
                    _ => {
                        println!(
                            "\r  {} Connection failed — check homeserver URL and token",
                            style("❌").red().bold()
                        );
                        continue;
                    }
                };

                let room_id: String = Input::new()
                    .with_prompt("  Room ID (e.g. !abc123:matrix.org)")
                    .interact_text()?;

                let users_str: String = Input::new()
                    .with_prompt("  Allowed users (comma-separated @user:server, or * for all)")
                    .default("*".into())
                    .interact_text()?;

                let allowed_users = if users_str.trim() == "*" {
                    vec!["*".into()]
                } else {
                    users_str.split(',').map(|s| s.trim().to_string()).collect()
                };

                config.matrix = Some(MatrixConfig {
                    homeserver: homeserver.trim_end_matches('/').to_string(),
                    access_token,
                    user_id: detected_user_id,
                    device_id: detected_device_id,
                    room_id,
                    allowed_users,
                    mention_only: false,
                });
            }
            ChannelMenuChoice::WhatsApp => {
                // ── WhatsApp ──
                println!();
                println!("  {}", style("WhatsApp Setup").white().bold());

                let mode_options = vec![
                    "WhatsApp Web (QR / pair-code, no Meta Business API)",
                    "WhatsApp Business Cloud API (webhook)",
                ];
                let mode_idx = Select::new()
                    .with_prompt("  Choose WhatsApp mode")
                    .items(&mode_options)
                    .default(0)
                    .interact()?;

                if mode_idx == 0 {
                    println!("  {}", style("Mode: WhatsApp Web").dim());
                    print_bullet("1. Build with --features whatsapp-web");
                    print_bullet("2. Start channel/daemon and scan QR in WhatsApp > Linked Devices");
                    print_bullet("3. Keep session_path persistent so relogin is not required");
                    println!();

                    let session_path: String = Input::new()
                        .with_prompt("  Session database path")
                        .default("~/.openprx/state/whatsapp-web/session.db".into())
                        .interact_text()?;

                    if session_path.trim().is_empty() {
                        println!("  {} Skipped — session path required", style("→").dim());
                        continue;
                    }

                    let pair_phone: String = Input::new()
                        .with_prompt("  Pair phone (optional, digits only; leave empty to use QR flow)")
                        .allow_empty(true)
                        .interact_text()?;

                    let pair_code: String = if pair_phone.trim().is_empty() {
                        String::new()
                    } else {
                        Input::new()
                            .with_prompt("  Custom pair code (optional, leave empty for auto-generated)")
                            .allow_empty(true)
                            .interact_text()?
                    };

                    let users_str: String = Input::new()
                        .with_prompt("  Allowed phone numbers (comma-separated +1234567890, or * for all)")
                        .default("*".into())
                        .interact_text()?;

                    let allowed_numbers = if users_str.trim() == "*" {
                        vec!["*".into()]
                    } else {
                        users_str.split(',').map(|s| s.trim().to_string()).collect()
                    };

                    config.whatsapp = Some(WhatsAppConfig {
                        access_token: None,
                        phone_number_id: None,
                        verify_token: None,
                        app_secret: None,
                        session_path: Some(session_path.trim().to_string()),
                        pair_phone: (!pair_phone.trim().is_empty()).then(|| pair_phone.trim().to_string()),
                        pair_code: (!pair_code.trim().is_empty()).then(|| pair_code.trim().to_string()),
                        allowed_numbers,
                        dm_policy: crate::config::schema::DmPolicy::default(),
                        group_policy: crate::config::schema::GroupPolicy::default(),
                        group_allow_from: Vec::new(),
                        mention_only: false,
                        // Group smart-reply: default (None) derives MentionOnly/Off
                        // from `mention_only`. Set `group_reply_mode = "smart"` in
                        // config.toml to enable model-decided group replies.
                        group_reply_mode: None,
                        ws_url: None,
                        read_pool_size: None,
                    });

                    println!("  {} WhatsApp Web configuration saved.", style("✅").green().bold());
                    continue;
                }

                println!("  {} {}", style("Mode:").dim(), style("Business Cloud API").dim());
                print_bullet("1. Go to developers.facebook.com and create a WhatsApp app");
                print_bullet("2. Add the WhatsApp product and get your phone number ID");
                print_bullet("3. Generate a temporary access token (System User)");
                print_bullet("4. Configure webhook URL to: https://your-domain/whatsapp");
                println!();

                let access_token: String = Input::new()
                    .with_prompt("  Access token (from Meta Developers)")
                    .interact_text()?;

                if access_token.trim().is_empty() {
                    println!("  {} Skipped", style("→").dim());
                    continue;
                }

                let phone_number_id: String = Input::new()
                    .with_prompt("  Phone number ID (from WhatsApp app settings)")
                    .interact_text()?;

                if phone_number_id.trim().is_empty() {
                    println!("  {} Skipped — phone number ID required", style("→").dim());
                    continue;
                }

                let verify_token: String = Input::new()
                    .with_prompt("  Webhook verify token (create your own)")
                    .default("openprx-whatsapp-verify".into())
                    .interact_text()?;

                // Test connection (run entirely in separate thread — Response must be used/dropped there)
                print!("  {} Testing connection... ", style("⏳").dim());
                let phone_number_id_clone = phone_number_id.clone();
                let access_token_clone = access_token.clone();
                let thread_result = std::thread::spawn(move || {
                    let client = reqwest::blocking::Client::new();
                    let url = format!("https://graph.facebook.com/v18.0/{}", phone_number_id_clone.trim());
                    let resp = client
                        .get(&url)
                        .header("Authorization", format!("Bearer {}", access_token_clone.trim()))
                        .send()?;
                    Ok::<_, reqwest::Error>(resp.status().is_success())
                })
                .join();
                match thread_result {
                    Ok(Ok(true)) => {
                        println!("\r  {} Connected to WhatsApp API        ", style("✅").green().bold());
                    }
                    _ => {
                        println!(
                            "\r  {} Connection failed — check access token and phone number ID",
                            style("❌").red().bold()
                        );
                        continue;
                    }
                }

                let users_str: String = Input::new()
                    .with_prompt("  Allowed phone numbers (comma-separated +1234567890, or * for all)")
                    .default("*".into())
                    .interact_text()?;

                let allowed_numbers = if users_str.trim() == "*" {
                    vec!["*".into()]
                } else {
                    users_str.split(',').map(|s| s.trim().to_string()).collect()
                };

                config.whatsapp = Some(WhatsAppConfig {
                    access_token: Some(access_token.trim().to_string()),
                    phone_number_id: Some(phone_number_id.trim().to_string()),
                    verify_token: Some(verify_token.trim().to_string()),
                    app_secret: None,
                    session_path: None,
                    pair_phone: None,
                    pair_code: None,
                    allowed_numbers,
                    dm_policy: crate::config::schema::DmPolicy::default(),
                    group_policy: crate::config::schema::GroupPolicy::default(),
                    group_allow_from: Vec::new(),
                    mention_only: false,
                    // Group smart-reply: default (None) derives MentionOnly/Off
                    // from `mention_only`. Set `group_reply_mode = "smart"` in
                    // config.toml to enable model-decided group replies.
                    group_reply_mode: None,
                    ws_url: None,
                    read_pool_size: None,
                });
            }
            ChannelMenuChoice::Linq => {
                // ── Linq ──
                println!();
                println!(
                    "  {} {}",
                    style("Linq Setup").white().bold(),
                    style("— iMessage/RCS/SMS via Linq API").dim()
                );
                print_bullet("1. Sign up at linqapp.com and get your Partner API token");
                print_bullet("2. Note your Linq phone number (E.164 format)");
                print_bullet("3. Configure webhook URL to: https://your-domain/linq");
                println!();

                let api_token: String = Input::new()
                    .with_prompt("  API token (Linq Partner API token)")
                    .interact_text()?;

                if api_token.trim().is_empty() {
                    println!("  {} Skipped", style("→").dim());
                    continue;
                }

                let from_phone: String = Input::new()
                    .with_prompt("  From phone number (E.164 format, e.g. +12223334444)")
                    .interact_text()?;

                if from_phone.trim().is_empty() {
                    println!("  {} Skipped — phone number required", style("→").dim());
                    continue;
                }

                // Test connection
                print!("  {} Testing connection... ", style("⏳").dim());
                let api_token_clone = api_token.clone();
                let thread_result = std::thread::spawn(move || {
                    let client = reqwest::blocking::Client::new();
                    let url = "https://api.linqapp.com/api/partner/v3/phonenumbers";
                    let resp = client
                        .get(url)
                        .header("Authorization", format!("Bearer {}", api_token_clone.trim()))
                        .send()?;
                    Ok::<_, reqwest::Error>(resp.status().is_success())
                })
                .join();
                match thread_result {
                    Ok(Ok(true)) => {
                        println!("\r  {} Connected to Linq API              ", style("✅").green().bold());
                    }
                    _ => {
                        println!("\r  {} Connection failed — check API token", style("❌").red().bold());
                        continue;
                    }
                }

                let users_str: String = Input::new()
                    .with_prompt("  Allowed sender numbers (comma-separated +1234567890, or * for all)")
                    .default("*".into())
                    .interact_text()?;

                let allowed_senders = if users_str.trim() == "*" {
                    vec!["*".into()]
                } else {
                    users_str.split(',').map(|s| s.trim().to_string()).collect()
                };

                let signing_secret: String = Input::new()
                    .with_prompt("  Webhook signing secret (optional, press Enter to skip)")
                    .allow_empty(true)
                    .interact_text()?;

                config.linq = Some(LinqConfig {
                    api_token: api_token.trim().to_string(),
                    from_phone: from_phone.trim().to_string(),
                    signing_secret: if signing_secret.trim().is_empty() {
                        None
                    } else {
                        Some(signing_secret.trim().to_string())
                    },
                    allowed_senders,
                    mention_only: false,
                });
            }
            ChannelMenuChoice::Irc => {
                // ── IRC ──
                println!();
                println!(
                    "  {} {}",
                    style("IRC Setup").white().bold(),
                    style("— IRC over TLS").dim()
                );
                print_bullet("IRC connects over TLS to any IRC server");
                print_bullet("Supports SASL PLAIN and NickServ authentication");
                println!();

                let server: String = Input::new().with_prompt("  IRC server (hostname)").interact_text()?;

                if server.trim().is_empty() {
                    println!("  {} Skipped", style("→").dim());
                    continue;
                }

                let port_str: String = Input::new()
                    .with_prompt("  Port")
                    .default("6697".into())
                    .interact_text()?;

                let port: u16 = port_str.trim().parse().unwrap_or_else(|_| {
                    println!("  {} Invalid port, using 6697", style("→").dim());
                    6697
                });

                let nickname: String = Input::new().with_prompt("  Bot nickname").interact_text()?;

                if nickname.trim().is_empty() {
                    println!("  {} Skipped — nickname required", style("→").dim());
                    continue;
                }

                let channels_str: String = Input::new()
                    .with_prompt("  Channels to join (comma-separated: #channel1,#channel2)")
                    .allow_empty(true)
                    .interact_text()?;

                let channels = if channels_str.trim().is_empty() {
                    vec![]
                } else {
                    channels_str
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                };

                print_bullet("Allowlist nicknames that can interact with the bot (case-insensitive).");
                print_bullet("Use '*' to allow anyone (not recommended for production).");

                let users_str: String = Input::new()
                    .with_prompt("  Allowed nicknames (comma-separated, or * for all)")
                    .allow_empty(true)
                    .interact_text()?;

                let allowed_users = if users_str.trim() == "*" {
                    vec!["*".into()]
                } else {
                    users_str
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                };

                if allowed_users.is_empty() {
                    print_bullet("⚠️  Empty allowlist — only you can interact. Add nicknames above.");
                }

                println!();
                print_bullet("Optional authentication (press Enter to skip each):");

                let server_password: String = Input::new()
                    .with_prompt("  Server password (for bouncers like ZNC, leave empty if none)")
                    .allow_empty(true)
                    .interact_text()?;

                let nickserv_password: String = Input::new()
                    .with_prompt("  NickServ password (leave empty if none)")
                    .allow_empty(true)
                    .interact_text()?;

                let sasl_password: String = Input::new()
                    .with_prompt("  SASL PLAIN password (leave empty if none)")
                    .allow_empty(true)
                    .interact_text()?;

                let verify_tls: bool = Confirm::new()
                    .with_prompt("  Verify TLS certificate?")
                    .default(true)
                    .interact()?;

                println!(
                    "  {} IRC configured as {}@{}:{}",
                    style("✅").green().bold(),
                    style(&nickname).cyan(),
                    style(&server).cyan(),
                    style(port).cyan()
                );

                config.irc = Some(IrcConfig {
                    server: server.trim().to_string(),
                    port,
                    nickname: nickname.trim().to_string(),
                    username: None,
                    channels,
                    allowed_users,
                    server_password: if server_password.trim().is_empty() {
                        None
                    } else {
                        Some(server_password.trim().to_string())
                    },
                    nickserv_password: if nickserv_password.trim().is_empty() {
                        None
                    } else {
                        Some(nickserv_password.trim().to_string())
                    },
                    sasl_password: if sasl_password.trim().is_empty() {
                        None
                    } else {
                        Some(sasl_password.trim().to_string())
                    },
                    verify_tls: Some(verify_tls),
                    mention_only: false,
                });
            }
            ChannelMenuChoice::Webhook => {
                // ── Webhook ──
                println!();
                println!(
                    "  {} {}",
                    style("Webhook Setup").white().bold(),
                    style("— HTTP endpoint for custom integrations").dim()
                );

                let port: String = Input::new()
                    .with_prompt("  Port")
                    .default("8080".into())
                    .interact_text()?;

                let secret: String = Input::new()
                    .with_prompt("  Secret (optional, Enter to skip)")
                    .allow_empty(true)
                    .interact_text()?;

                config.webhook = Some(WebhookConfig {
                    port: port.parse().unwrap_or(8080),
                    secret: if secret.is_empty() { None } else { Some(secret) },
                });
                println!(
                    "  {} Webhook on port {}",
                    style("✅").green().bold(),
                    style(&port).cyan()
                );
            }
            ChannelMenuChoice::DingTalk => {
                // ── DingTalk ──
                println!();
                println!(
                    "  {} {}",
                    style("DingTalk Setup").white().bold(),
                    style("— DingTalk Stream Mode").dim()
                );
                print_bullet("1. Go to DingTalk developer console (open.dingtalk.com)");
                print_bullet("2. Create an app and enable the Stream Mode bot");
                print_bullet("3. Copy the Client ID (AppKey) and Client Secret (AppSecret)");
                println!();

                let client_id: String = Input::new().with_prompt("  Client ID (AppKey)").interact_text()?;

                if client_id.trim().is_empty() {
                    println!("  {} Skipped", style("→").dim());
                    continue;
                }

                let client_secret: String = Input::new()
                    .with_prompt("  Client Secret (AppSecret)")
                    .interact_text()?;

                // Test connection (run entirely in a separate thread — reqwest::blocking enters and
                // drops its own runtime, which must not happen on a Tokio worker)
                print!("  {} Testing connection... ", style("⏳").dim());
                let probe_client_id = client_id.clone();
                let probe_client_secret = client_secret.clone();
                let thread_result = std::thread::spawn(move || {
                    let client = reqwest::blocking::Client::new();
                    let body = serde_json::json!({
                        "clientId": probe_client_id,
                        "clientSecret": probe_client_secret,
                    });
                    let resp = client
                        .post("https://api.dingtalk.com/v1.0/gateway/connections/open")
                        .json(&body)
                        .send()?;
                    Ok::<_, reqwest::Error>(resp.status().is_success())
                })
                .join();
                match thread_result {
                    Ok(Ok(true)) => {
                        println!(
                            "\r  {} DingTalk credentials verified        ",
                            style("✅").green().bold()
                        );
                    }
                    _ => {
                        println!(
                            "\r  {} Connection failed — check your credentials",
                            style("❌").red().bold()
                        );
                        continue;
                    }
                }

                let users_str: String = Input::new()
                    .with_prompt("  Allowed staff IDs (comma-separated, '*' for all)")
                    .allow_empty(true)
                    .interact_text()?;

                let allowed_users: Vec<String> = users_str
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();

                config.dingtalk = Some(DingTalkConfig {
                    client_id,
                    client_secret,
                    allowed_users,
                    mention_only: false,
                });
            }
            ChannelMenuChoice::QqOfficial => {
                // ── QQ Official ──
                println!();
                println!(
                    "  {} {}",
                    style("QQ Official Setup").white().bold(),
                    style("— Tencent QQ Bot SDK").dim()
                );
                print_bullet("1. Go to QQ Bot developer console (q.qq.com)");
                print_bullet("2. Create a bot application");
                print_bullet("3. Copy the App ID and App Secret");
                println!();

                let app_id: String = Input::new().with_prompt("  App ID").interact_text()?;

                if app_id.trim().is_empty() {
                    println!("  {} Skipped", style("→").dim());
                    continue;
                }

                let app_secret: String = Input::new().with_prompt("  App Secret").interact_text()?;

                // Test connection (run entirely in a separate thread — reqwest::blocking enters and
                // drops its own runtime, which must not happen on a Tokio worker)
                print!("  {} Testing connection... ", style("⏳").dim());
                let probe_app_id = app_id.clone();
                let probe_app_secret = app_secret.clone();
                let thread_result = std::thread::spawn(move || {
                    let client = reqwest::blocking::Client::new();
                    let body = serde_json::json!({
                        "appId": probe_app_id,
                        "clientSecret": probe_app_secret,
                    });
                    let resp = client
                        .post("https://bots.qq.com/app/getAppAccessToken")
                        .json(&body)
                        .send()?;
                    if !resp.status().is_success() {
                        return Ok::<_, reqwest::Error>((false, false));
                    }
                    let data: serde_json::Value = resp.json().unwrap_or_default();
                    Ok((true, data.get("access_token").is_some()))
                })
                .join();
                match thread_result {
                    Ok(Ok((true, true))) => {
                        println!("\r  {} QQ Bot credentials verified        ", style("✅").green().bold());
                    }
                    Ok(Ok((true, false))) => {
                        println!("\r  {} Auth error — check your credentials", style("❌").red().bold());
                        continue;
                    }
                    _ => {
                        println!(
                            "\r  {} Connection failed — check your credentials",
                            style("❌").red().bold()
                        );
                        continue;
                    }
                }

                let users_str: String = Input::new()
                    .with_prompt("  Allowed user IDs (comma-separated, '*' for all)")
                    .allow_empty(true)
                    .interact_text()?;

                let allowed_users: Vec<String> = users_str
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();

                config.qq = Some(QQConfig {
                    app_id,
                    app_secret,
                    allowed_users,
                    mention_only: false,
                });
            }
            ChannelMenuChoice::LarkFeishu => {
                // ── Lark/Feishu ──
                println!();
                println!(
                    "  {} {}",
                    style("Lark/Feishu Setup").white().bold(),
                    style("— talk to OpenPRX from Lark or Feishu").dim()
                );
                print_bullet("1. Go to Lark/Feishu Open Platform (open.larksuite.com / open.feishu.cn)");
                print_bullet("2. Create an app and enable 'Bot' capability");
                print_bullet("3. Copy the App ID and App Secret");
                println!();

                let app_id: String = Input::new().with_prompt("  App ID").interact_text()?;
                let app_id = app_id.trim().to_string();

                if app_id.trim().is_empty() {
                    println!("  {} Skipped", style("→").dim());
                    continue;
                }

                let app_secret: String = Input::new().with_prompt("  App Secret").interact_text()?;
                let app_secret = app_secret.trim().to_string();

                if app_secret.is_empty() {
                    println!("  {} App Secret is required", style("❌").red().bold());
                    continue;
                }

                let use_feishu = Select::new()
                    .with_prompt("  Region")
                    .items(["Feishu (CN)", "Lark (International)"])
                    .default(0)
                    .interact()?
                    == 0;

                // Test connection (run entirely in separate thread — Response must be used/dropped there)
                print!("  {} Testing connection... ", style("⏳").dim());
                let base_url = if use_feishu {
                    "https://open.feishu.cn/open-apis"
                } else {
                    "https://open.larksuite.com/open-apis"
                };
                let app_id_clone = app_id.clone();
                let app_secret_clone = app_secret.clone();
                let endpoint = format!("{base_url}/auth/v3/tenant_access_token/internal");

                let thread_result = std::thread::spawn(move || {
                    let client = reqwest::blocking::Client::builder()
                        .timeout(Duration::from_secs(8))
                        .connect_timeout(Duration::from_secs(4))
                        .build()
                        .map_err(|err| format!("failed to build HTTP client: {err}"))?;
                    let body = serde_json::json!({
                        "app_id": app_id_clone,
                        "app_secret": app_secret_clone,
                    });

                    let response = client
                        .post(endpoint)
                        .json(&body)
                        .send()
                        .map_err(|err| format!("request error: {err}"))?;

                    let status = response.status();
                    let payload: Value = response.json().unwrap_or_default();
                    let has_token = payload
                        .get("tenant_access_token")
                        .and_then(Value::as_str)
                        .is_some_and(|token| !token.trim().is_empty());

                    if status.is_success() && has_token {
                        return Ok::<(), String>(());
                    }

                    let detail = payload
                        .get("msg")
                        .or_else(|| payload.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error");

                    Err(format!("auth rejected ({status}): {detail}"))
                })
                .join();

                match thread_result {
                    Ok(Ok(())) => {
                        println!(
                            "\r  {} Lark/Feishu credentials verified        ",
                            style("✅").green().bold()
                        );
                    }
                    Ok(Err(reason)) => {
                        println!(
                            "\r  {} Connection failed — check your credentials",
                            style("❌").red().bold()
                        );
                        println!("    {}", style(reason).dim());
                        continue;
                    }
                    Err(_) => {
                        println!(
                            "\r  {} Connection failed — check your credentials",
                            style("❌").red().bold()
                        );
                        continue;
                    }
                }

                let receive_mode_choice = Select::new()
                    .with_prompt("  Receive Mode")
                    .items([
                        "WebSocket (recommended, no public IP needed)",
                        "Webhook (requires public HTTPS endpoint)",
                    ])
                    .default(0)
                    .interact()?;

                let receive_mode = if receive_mode_choice == 0 {
                    LarkReceiveMode::Websocket
                } else {
                    LarkReceiveMode::Webhook
                };

                let verification_token = if receive_mode == LarkReceiveMode::Webhook {
                    let token: String = Input::new()
                        .with_prompt("  Verification Token (optional, for Webhook mode)")
                        .allow_empty(true)
                        .interact_text()?;
                    if token.is_empty() { None } else { Some(token) }
                } else {
                    None
                };

                if receive_mode == LarkReceiveMode::Webhook && verification_token.is_none() {
                    println!(
                        "  {} Verification Token is empty — webhook authenticity checks are reduced.",
                        style("⚠").yellow().bold()
                    );
                }

                let port = if receive_mode == LarkReceiveMode::Webhook {
                    let p: String = Input::new()
                        .with_prompt("  Webhook Port")
                        .default("8080".into())
                        .interact_text()?;
                    Some(p.parse().unwrap_or(8080))
                } else {
                    None
                };

                let users_str: String = Input::new()
                    .with_prompt("  Allowed user Open IDs (comma-separated, '*' for all)")
                    .allow_empty(true)
                    .interact_text()?;

                let allowed_users: Vec<String> = users_str
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();

                if allowed_users.is_empty() {
                    println!(
                        "  {} No users allowlisted — Lark/Feishu inbound messages will be denied until you add Open IDs or '*'.",
                        style("⚠").yellow().bold()
                    );
                }

                config.lark = Some(LarkConfig {
                    app_id,
                    app_secret,
                    verification_token,
                    encrypt_key: None,
                    allowed_users,
                    use_feishu,
                    receive_mode,
                    port,
                    mention_only: false,
                });
            }
            ChannelMenuChoice::Done => break,
        }
        println!();
    }

    // Summary line
    let mut active: Vec<&str> = vec!["CLI"];
    if config.telegram.is_some() {
        active.push("Telegram");
    }
    if config.discord.is_some() {
        active.push("Discord");
    }
    if config.slack.is_some() {
        active.push("Slack");
    }
    if config.imessage.is_some() {
        active.push("iMessage");
    }
    #[cfg(feature = "channel-matrix")]
    if config.matrix.is_some() {
        active.push("Matrix");
    }
    if config.whatsapp.is_some() {
        active.push("WhatsApp");
    }
    if config.linq.is_some() {
        active.push("Linq");
    }
    if config.email.is_some() {
        active.push("Email");
    }
    if config.irc.is_some() {
        active.push("IRC");
    }
    if config.webhook.is_some() {
        active.push("Webhook");
    }
    if config.dingtalk.is_some() {
        active.push("DingTalk");
    }
    if config.qq.is_some() {
        active.push("QQ");
    }
    if config.lark.is_some() {
        active.push("Lark");
    }

    println!(
        "  {} Channels: {}",
        style("✓").green().bold(),
        style(active.join(", ")).green()
    );

    Ok(config)
}
