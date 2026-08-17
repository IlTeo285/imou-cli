use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::api::alarm::Alarm;

/// One line written to the events file per detected motion — shared by
/// `watch` (polling) and `listen` (push), both of which synthesize an
/// `Alarm` from their respective data sources before calling this.
#[derive(Debug, Serialize)]
pub struct MotionEvent {
    /// True UTC instant (RFC3339), from `Alarm::utc_time`.
    time: String,
    /// Account-local wall-clock time (same convention as `beginTime`/
    /// `endTime` in the `getAlarmMessage` request), for human readability.
    local_time: String,
    device_id: String,
    channel_id: String,
    channel_name: String,
    alarm_id: String,
    alarm_name: String,
    /// Raw passthrough fields — see the doc-accuracy note on `Alarm` in
    /// `api::alarm`. Kept so events can be filtered/refined later once more
    /// `label_type` variety is observed across accounts/devices.
    alarm_type: String,
    label_type: String,
}

impl MotionEvent {
    pub fn from_alarm(alarm: &Alarm, channel_name: &str) -> Self {
        // `utc_time` (not `time` — see the note on `Alarm`) is the genuine
        // UTC instant.
        let time = alarm
            .utc_time
            .parse::<i64>()
            .ok()
            .and_then(|secs| DateTime::<Utc>::from_timestamp(secs, 0))
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_else(|| alarm.utc_time.clone());

        // `alarm.time`'s epoch value is local-wall-clock-as-if-UTC — format
        // it naively (no offset suffix) rather than through `DateTime<Utc>`,
        // which would attach a misleading "+00:00".
        let local_time = DateTime::<Utc>::from_timestamp(alarm.time, 0)
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| alarm.time.to_string());

        MotionEvent {
            time,
            local_time,
            device_id: alarm.device_id.clone(),
            channel_id: alarm.channel_id.clone(),
            channel_name: channel_name.to_string(),
            alarm_id: alarm.alarm_id.clone(),
            alarm_name: alarm.name.clone(),
            alarm_type: alarm.alarm_type.clone(),
            label_type: alarm.label_type.clone(),
        }
    }
}
