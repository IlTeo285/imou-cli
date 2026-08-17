use serde::{Deserialize, Serialize};

use crate::client::ImouClient;
use crate::error::{ImouError, Result};

#[derive(Serialize)]
struct BindDeviceLiveParams<'a> {
    #[serde(rename = "deviceId")]
    device_id: &'a str,
    #[serde(rename = "channelId")]
    channel_id: &'a str,
    #[serde(rename = "streamId")]
    stream_id: u8,
}

#[derive(Serialize)]
struct GetLiveStreamInfoParams<'a> {
    #[serde(rename = "deviceId")]
    device_id: &'a str,
    #[serde(rename = "channelId")]
    channel_id: &'a str,
}

#[derive(Debug, Deserialize)]
pub struct Stream {
    #[serde(rename = "streamId")]
    pub stream_id: i64,
    pub hls: String,
}

/// `live_status` is only present on `bindDeviceLive`'s response, not
/// `getLiveStreamInfo`'s — left `None` when queried that way.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveInfo {
    #[serde(default)]
    pub live_status: Option<i64>,
    pub streams: Vec<Stream>,
}

/// Creates a live source for a device channel and returns its HLS URL.
/// `stream_id` is 0 for the HD main stream, 1 for the SD substream.
///
/// A prior `bind` call (from this CLI or elsewhere) may still have a live
/// session open — `bindDeviceLive` errors with `LV1001` ("The video live
/// exists") rather than just returning it, so on that error this falls back
/// to `getLiveStreamInfo` to fetch the existing session instead.
/// See https://open.imoulife.com/book/http/device/live/bindDeviceLive.html
pub async fn bind(
    client: &ImouClient,
    device_id: &str,
    channel_id: &str,
    stream_id: u8,
) -> Result<LiveInfo> {
    let result = client
        .call(
            "bindDeviceLive",
            BindDeviceLiveParams {
                device_id,
                channel_id,
                stream_id,
            },
        )
        .await;

    match result {
        Err(ImouError::Api { code, .. }) if code == "LV1001" => {
            get_stream_info(client, device_id, channel_id).await
        }
        other => other,
    }
}

/// Fetches the HLS URL(s) of an already-open live session.
/// See https://open.imoulife.com/book/http/device/live/getLiveStreamInfo.html
pub async fn get_stream_info(
    client: &ImouClient,
    device_id: &str,
    channel_id: &str,
) -> Result<LiveInfo> {
    client
        .call(
            "getLiveStreamInfo",
            GetLiveStreamInfoParams {
                device_id,
                channel_id,
            },
        )
        .await
}
