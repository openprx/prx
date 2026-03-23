use super::AppState;
use axum::{Json, extract::State};
use serde::Serialize;

#[derive(Serialize)]
struct ChannelStatus {
    name: String,
    enabled: bool,
    #[serde(rename = "type")]
    channel_type: String,
    status: String,
}

#[derive(Serialize)]
pub(super) struct ChannelsStatusResponse {
    channels: Vec<ChannelStatus>,
}

pub async fn get_channels_status(State(state): State<AppState>) -> Json<ChannelsStatusResponse> {
    let config = state.config.lock();
    let mut channels = Vec::new();

    let mut push_channel = |name: &str, enabled: bool| {
        if enabled {
            channels.push(ChannelStatus {
                name: name.to_string(),
                enabled,
                channel_type: name.to_string(),
                status: "configured".to_string(),
            });
        }
    };

    push_channel("cli", config.channels_config.cli);
    push_channel("telegram", config.channels_config.telegram.is_some());
    push_channel("discord", config.channels_config.discord.is_some());
    push_channel("slack", config.channels_config.slack.is_some());
    push_channel("mattermost", config.channels_config.mattermost.is_some());
    push_channel("webhook", config.channels_config.webhook.is_some());
    push_channel("imessage", config.channels_config.imessage.is_some());
    push_channel("matrix", config.channels_config.matrix.is_some());
    push_channel("signal", config.channels_config.signal.is_some());
    push_channel("whatsapp", config.channels_config.whatsapp.is_some());
    push_channel("wacli", config.channels_config.wacli.is_some());
    push_channel("linq", config.channels_config.linq.is_some());
    push_channel("nextcloud_talk", config.channels_config.nextcloud_talk.is_some());
    push_channel("email", config.channels_config.email.is_some());
    push_channel("irc", config.channels_config.irc.is_some());
    push_channel("lark", config.channels_config.lark.is_some());
    push_channel("dingtalk", config.channels_config.dingtalk.is_some());
    push_channel("qq", config.channels_config.qq.is_some());

    channels.sort_by(|a, b| a.name.cmp(&b.name));
    Json(ChannelsStatusResponse { channels })
}
