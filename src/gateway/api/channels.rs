use super::AppState;
use axum::{extract::State, Json};
use serde::Serialize;

#[derive(Serialize)]
struct ChannelStatus {
    name: String,
    enabled: bool,
    #[serde(rename = "type")]
    channel_type: String,
}

#[derive(Serialize)]
pub(super) struct ChannelsStatusResponse {
    channels: Vec<ChannelStatus>,
}

pub async fn get_channels_status(State(state): State<AppState>) -> Json<ChannelsStatusResponse> {
    let config = state.config.lock();
    let channels = vec![
        ChannelStatus {
            name: "signal".to_string(),
            enabled: config.channels_config.signal.is_some(),
            channel_type: "signal".to_string(),
        },
        ChannelStatus {
            name: "whatsapp".to_string(),
            enabled: config.channels_config.whatsapp.is_some(),
            channel_type: "whatsapp".to_string(),
        },
        ChannelStatus {
            name: "linq".to_string(),
            enabled: config.channels_config.linq.is_some(),
            channel_type: "linq".to_string(),
        },
        ChannelStatus {
            name: "nextcloud_talk".to_string(),
            enabled: config.channels_config.nextcloud_talk.is_some(),
            channel_type: "nextcloud_talk".to_string(),
        },
    ];
    Json(ChannelsStatusResponse { channels })
}
