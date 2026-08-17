use serde::{Deserialize, Serialize};

use crate::client::ImouClient;
use crate::error::Result;

#[derive(Serialize)]
struct GetAlarmMessageParams<'a> {
    #[serde(rename = "deviceId")]
    device_id: &'a str,
    #[serde(rename = "channelId")]
    channel_id: &'a str,
    #[serde(rename = "beginTime")]
    begin_time: &'a str,
    #[serde(rename = "endTime")]
    end_time: &'a str,
    count: &'a str,
    #[serde(rename = "nextAlarmId")]
    next_alarm_id: &'a str,
}

/// The `type` code table in the docs (0 = human infrared, 1 = motion, ...)
/// does not match reality: every alarm observed live from a real account
/// came back as `type: "34500"`. `labelType` (undocumented, but
/// human-readable — e.g. `"humanAlarm"`) is what's actually usable, so
/// `type` is kept only as an opaque passthrough for the event log, not
/// filtered on.
///
/// `time` and `utcTime` are NOT both UTC despite the naming: `time` is the
/// account's local wall-clock time, encoded as a Unix epoch as if it *were*
/// UTC (i.e. `utcfromtimestamp(time)` prints the right digits but the wrong
/// offset). `utcTime` is the genuine UTC instant, offset from `time` by the
/// account's timezone (observed: 2 hours, matching W. Europe/CEST). Use
/// `utc_time` for any real timestamp math or display; `time`'s epoch value
/// is only meaningful alongside `beginTime`/`endTime` in the *request*,
/// which the API filters using this same local-as-if-UTC convention (see
/// `watch.rs`'s use of `chrono::Local` instead of `Utc` for query windows —
/// using true UTC there silently drops every event less than the account's
/// UTC offset old).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Alarm {
    pub alarm_id: String,
    pub device_id: String,
    pub channel_id: String,
    pub name: String,
    /// Unix timestamp (seconds) — see the local-vs-UTC note above.
    pub time: i64,
    /// Unix timestamp (seconds) as a string — the genuine UTC instant.
    pub utc_time: String,
    #[serde(rename = "type")]
    pub alarm_type: String,
    #[serde(default)]
    pub label_type: String,
}

fn default_next_alarm_id() -> i64 {
    -1
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlarmList {
    /// Absent from the response entirely when `alarms` is empty (observed
    /// live — not documented), so this defaults to `-1` (the same value
    /// that means "start of window" in the request).
    #[serde(default = "default_next_alarm_id")]
    pub next_alarm_id: i64,
    #[serde(default)]
    pub alarms: Vec<Alarm>,
}

/// Queries alarm/event messages for one device channel in `[begin_time,
/// end_time]` (format `"yyyy-MM-dd HH:mm:ss"`), up to `count` (max 30) at a
/// time. Pass the previous response's `next_alarm_id` back in to page
/// through more results within the same window; `-1` starts from the
/// beginning of the window.
/// See https://open.imoulife.com/book/http/device/alarm/getAlarmMessage.html
pub async fn get_alarm_message(
    client: &ImouClient,
    device_id: &str,
    channel_id: &str,
    begin_time: &str,
    end_time: &str,
    next_alarm_id: i64,
) -> Result<AlarmList> {
    client
        .call(
            "getAlarmMessage",
            GetAlarmMessageParams {
                device_id,
                channel_id,
                begin_time,
                end_time,
                count: "30",
                next_alarm_id: &next_alarm_id.to_string(),
            },
        )
        .await
}
