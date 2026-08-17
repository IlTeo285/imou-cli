use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::client::ImouClient;
use crate::error::Result;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Channel {
    pub channel_id: String,
    pub channel_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub bind_id: i64,
    pub device_id: String,
    #[serde(default)]
    pub channels: Vec<Channel>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceList {
    pub count: i64,
    pub device_list: Vec<Device>,
}

#[derive(Serialize)]
struct DeviceBaseListParams {
    #[serde(rename = "bindId")]
    bind_id: i64,
    limit: i64,
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(rename = "needApInfo")]
    need_ap_info: bool,
}

/// Lists devices/channels bound to (or shared with) this app's account.
/// See https://open.imoulife.com/book/http/device/manage/query/deviceBaseList.html
pub async fn list(client: &ImouClient, limit: i64) -> Result<DeviceList> {
    client
        .call(
            "deviceBaseList",
            DeviceBaseListParams {
                bind_id: -1,
                limit,
                kind: "bind",
                need_ap_info: false,
            },
        )
        .await
}

/// Online status codes: 0 offline, 1 online, 3 upgrading, 4 sleeping.
#[derive(Debug, Deserialize)]
pub struct ChannelOnlineStatus {
    #[serde(rename = "channelId")]
    pub channel_id: String,
    #[serde(rename = "onLine")]
    pub on_line: String,
}

#[derive(Debug, Deserialize)]
pub struct OnlineStatus {
    #[serde(rename = "onLine")]
    pub on_line: String,
    #[serde(default)]
    pub channels: Vec<ChannelOnlineStatus>,
}

/// Queries whether a device is online.
/// See https://open.imoulife.com/book/http/device/manage/query/deviceOnline.html
pub async fn online_status(client: &ImouClient, device_id: &str) -> Result<OnlineStatus> {
    client
        .call("deviceOnline", json!({ "deviceId": device_id }))
        .await
}
