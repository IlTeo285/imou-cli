use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::api::alarm::Alarm;

/// One line written to the events file per detected motion — shared by
/// `watch` (polling) and `listen` (push), both of which synthesize an
/// `Alarm` from their respective data sources before calling this.
#[derive(Debug, Serialize)]
pub struct MotionEvent {
    /// Always `"motion"` — distinguishes this line from a later
    /// `MotionAnalysisEvent` line for the same `alarm_id` in the same
    /// append-only file. Additive: existing consumers reading known keys
    /// are unaffected by this field's presence.
    kind: &'static str,
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
            kind: "motion",
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

/// A second, deferred line appended to the same events file once AI
/// analysis of a clip completes — correlated to the original `MotionEvent`
/// line by `alarm_id`. Deferred because analysis can only run after the
/// clip itself is extracted (see `recorder::spawn_clip_extraction`), well
/// after the immediate `MotionEvent`/MQTT notification already fired; see
/// CLAUDE.md's note on why the fast path is never delayed to wait for it.
#[derive(Debug, Serialize)]
pub struct MotionAnalysisEvent {
    kind: &'static str,
    alarm_id: String,
    channel_name: String,
    /// True UTC instant (RFC3339) when analysis completed.
    time: String,
    category: String,
    relevant: bool,
    description: String,
    uploaded_to_gdrive: bool,
}

impl MotionAnalysisEvent {
    pub fn new(
        alarm: &Alarm,
        channel_name: &str,
        result: &imou_vision::AnalysisResult,
        uploaded_to_gdrive: bool,
    ) -> Self {
        MotionAnalysisEvent {
            kind: "motion_analyzed",
            alarm_id: alarm.alarm_id.clone(),
            channel_name: channel_name.to_string(),
            time: Utc::now().to_rfc3339(),
            category: result.category.to_string(),
            relevant: result.relevant,
            description: result.description.clone(),
            uploaded_to_gdrive,
        }
    }
}
