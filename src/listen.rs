use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::Router;
use serde::Deserialize;

use crate::api;
use crate::api::alarm::Alarm;
use crate::client::ImouClient;
use crate::error::Result;
use crate::event_log::EventLog;
use crate::gdrive;
use crate::motion_event::MotionEvent;
use crate::mqtt;
use crate::recorder;

const CALLBACK_PATH: &str = "/imou-callback";

/// Extra safety margin on top of `pre_roll + max_push_latency`, same role
/// as `watch::RETENTION_MARGIN`.
const RETENTION_MARGIN: Duration = Duration::from_secs(15);

/// Mirrors the REAL push payload observed live against this account — not
/// what `push/event.html` documents. Confirmed differences: `msgType` is
/// `"iotEvent"` (not `"videoMotion"`), there's no `cid`, the alarm subtype
/// lives in `content.event` (matches `Alarm.alarm_type` from
/// `getAlarmMessage`, e.g. `"34500"`), the channel name is `dname` (not
/// `cname`), and `time`/`utcTime` are `"yyyyMMddTHHmmss"` strings, not unix
/// epoch numbers. Every field is optional because Imou also sends
/// no-op verification pings with an empty `{}` body.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PushEvent {
    #[serde(rename = "alarmId")]
    alarm_id: Option<String>,
    did: Option<String>,
    dname: Option<String>,
    #[serde(rename = "msgType")]
    msg_type: Option<String>,
    content: Option<PushContent>,
    time: Option<String>,
    #[serde(rename = "utcTime")]
    utc_time: Option<String>,
    #[serde(rename = "appId")]
    app_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PushContent {
    event: Option<String>,
}

/// Parses a push `"yyyyMMddTHHmmss"` timestamp string into Unix epoch
/// seconds, treating the digits as UTC — correct as-is for `utcTime`
/// (genuinely UTC), and matching the same "local-wall-clock-as-if-UTC"
/// convention already used for `getAlarmMessage`'s `time` field when
/// applied to push's `time` (see the note on `Alarm` in `api::alarm`).
fn parse_push_timestamp(s: &str) -> Option<i64> {
    chrono::NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%S")
        .ok()
        .map(|naive| naive.and_utc().timestamp())
}

#[derive(Clone)]
struct AppState {
    app_id: Arc<str>,
    event_log: Arc<EventLog>,
    recorded_channels: Arc<HashSet<String>>,
    /// Fallback channel name lookup by device id, for the (unobserved but
    /// possible) case where a push payload omits `dname`.
    device_channel_names: Arc<HashMap<String, String>>,
    buffer_dir: Arc<PathBuf>,
    clips_dir: Arc<PathBuf>,
    pre_roll: Duration,
    post_roll: Duration,
    gdrive: Option<Arc<gdrive::GDriveClient>>,
    mqtt: Option<Arc<mqtt::MqttPublisher>>,
}

/// Handles one push delivery. Always returns 200 — per push.html, Imou
/// disables the callback after repeated non-200 responses, so anything we
/// don't recognize (a ping, an unrelated msgType, a malformed body) is
/// logged and swallowed rather than surfaced as an HTTP error.
async fn callback_handler(State(state): State<AppState>, body: Bytes) -> StatusCode {
    let payload: PushEvent = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("warning: failed to parse push payload: {e}");
            return StatusCode::OK;
        }
    };

    if let Some(pushed_app_id) = &payload.app_id
        && pushed_app_id.as_str() != &*state.app_id
    {
        eprintln!(
            "warning: ignoring push with mismatched appId {pushed_app_id} (expected {})",
            state.app_id
        );
        return StatusCode::OK;
    }

    if payload.msg_type.as_deref() != Some("iotEvent") {
        return StatusCode::OK; // verification ping or a msgType we don't handle
    }
    let Some(event_code) = payload.content.and_then(|c| c.event) else {
        return StatusCode::OK;
    };
    let Some(did) = payload.did else {
        return StatusCode::OK;
    };
    let Some(utc_epoch) = payload.utc_time.as_deref().and_then(parse_push_timestamp) else {
        eprintln!("warning: push event for {did} missing/invalid utcTime, skipping");
        return StatusCode::OK;
    };
    let time_epoch = payload
        .time
        .as_deref()
        .and_then(parse_push_timestamp)
        .unwrap_or(utc_epoch);

    let channel_name = payload
        .dname
        .or_else(|| state.device_channel_names.get(&did).cloned())
        .unwrap_or_else(|| did.clone());

    let alarm = Alarm {
        alarm_id: payload.alarm_id.unwrap_or_else(|| format!("push-{utc_epoch}")),
        device_id: did.clone(),
        channel_id: "0".to_string(),
        name: channel_name.clone(),
        time: time_epoch,
        utc_time: utc_epoch.to_string(),
        alarm_type: event_code,
        label_type: String::new(), // not present in the push payload
    };

    let event = MotionEvent::from_alarm(&alarm, &channel_name);
    match state.event_log.append(&event) {
        Ok(()) => println!("motion detected (push): device={did} ({channel_name})"),
        Err(e) => eprintln!("failed to write motion event: {e}"),
    }

    if let Some(mqtt) = &state.mqtt {
        mqtt.publish(&event, &channel_name);
    }

    if state.recorded_channels.contains(&channel_name) {
        recorder::spawn_clip_extraction(
            channel_name,
            (*state.buffer_dir).clone(),
            (*state.clips_dir).clone(),
            alarm,
            state.pre_roll,
            state.post_roll,
            state.gdrive.clone(),
        );
    }

    StatusCode::OK
}

/// Serves the push endpoint until Ctrl+C. Does **not** call
/// `setMessageCallback` — the callback URL must already be registered
/// manually via the Imou console (see the note printed at startup and
/// `CLAUDE.md`'s push/webhook section for why). Unlike `watch`, detection is
/// entirely push-driven — no polling loop.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    client: &ImouClient,
    callback_url: &str,
    listen_addr: SocketAddr,
    events_file: &Path,
    buffer_dir: &Path,
    clips_dir: &Path,
    pre_roll: Duration,
    post_roll: Duration,
    max_push_latency: Duration,
    gdrive_retention_days: u32,
) -> Result<()> {
    let event_log = Arc::new(EventLog::open(events_file)?);

    let devices = api::devices::list(client, 100).await?;
    let mut channels = Vec::new();
    let mut device_channel_names = HashMap::new();
    for d in &devices.device_list {
        for ch in &d.channels {
            device_channel_names.insert(d.device_id.clone(), ch.channel_name.clone());
            channels.push((d.device_id.clone(), ch.channel_id.clone(), ch.channel_name.clone()));
        }
    }

    if channels.is_empty() {
        println!("no devices/channels bound to this account — nothing to listen for");
        return Ok(());
    }

    // Retention must cover the pre-roll plus the worst-case push delivery
    // delay, same principle as `watch`'s `interval`-based margin — except
    // there's no polling interval here, so this is an explicit setting
    // (default generous: a cold registration was observed live to take
    // ~48 minutes before its first real delivery).
    let retention = pre_roll + max_push_latency + RETENTION_MARGIN;
    let recording = recorder::start_all(&channels, buffer_dir, retention).await?;

    let gdrive_client = gdrive::config_from_env().map(|cfg| Arc::new(gdrive::GDriveClient::new(cfg)));
    match &gdrive_client {
        Some(client) => {
            println!("uploading clips to Google Drive (retention: {gdrive_retention_days}d, 0 = forever)");
            gdrive::start_retention_sweep(client.clone(), gdrive_retention_days);
        }
        None => println!(
            "Google Drive upload not configured (GDRIVE_CLIENT_ID/GDRIVE_CLIENT_SECRET not set) — clips stay local only"
        ),
    }

    let mqtt_client = mqtt::config_from_env().map(|cfg| Arc::new(mqtt::MqttPublisher::connect(cfg)));
    match &mqtt_client {
        Some(m) => println!(
            "publishing motion events to MQTT broker {}:{} (topic prefix \"{}\")",
            m.host, m.port, m.topic_prefix
        ),
        None => println!(
            "MQTT publish not configured (MQTT_BROKER_HOST/MQTT_BROKER_PORT not set) — motion events are not published"
        ),
    }

    let state = AppState {
        app_id: Arc::from(client.app_id()),
        event_log,
        recorded_channels: Arc::new(recording.recorded_channels.clone()),
        device_channel_names: Arc::new(device_channel_names),
        buffer_dir: Arc::new(buffer_dir.to_path_buf()),
        clips_dir: Arc::new(clips_dir.to_path_buf()),
        pre_roll,
        post_roll,
        gdrive: gdrive_client,
        mqtt: mqtt_client,
    };

    let app = Router::new()
        .route(CALLBACK_PATH, post(callback_handler))
        .with_state(state);

    // Bind and start serving *before* registering the callback — Imou
    // (or an intermediate proxy) may check reachability synchronously
    // during `setMessageCallback`, and registering while nothing is
    // listening yet produced `OP1003` live even for an already-registered,
    // already-working URL.
    let listener = tokio::net::TcpListener::bind(listen_addr).await?;
    println!("listening on http://{listen_addr}{CALLBACK_PATH}");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
    });

    println!(
        "not registering the push callback — this must already be set to \
         {callback_url} manually via the Imou console (registering via \
         setMessageCallback here was found to reset the account's \"IoT \
         Device Message\" push subscription, which real motion events \
         depend on)"
    );
    println!("press Ctrl+C to stop");

    tokio::signal::ctrl_c().await.ok();
    let _ = shutdown_tx.send(());
    let _ = server.await;

    println!("stopping...");
    recorder::shutdown_all(recording).await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shape and timestamps captured from a real payload live in this
    /// project (device id/user id/appId replaced with placeholders below —
    /// only the structure and the two timestamp strings matter for this
    /// test) — regression test for the timestamp conversion, since getting
    /// local-vs-UTC wrong here has bitten this project twice already (see
    /// `api::alarm::Alarm`'s doc note).
    #[test]
    fn parses_real_push_payload() {
        let raw = r#"{"alarmId":"1171628849267639184","content":{"outputData":{},"localTime":"20260817T203200","event":"34500"},"did":"EXAMPLE0DEVICEID","dname":"frontdoor","msgType":"iotEvent","picUrlArr":["https://example.invalid/x.jpg"],"pid":"EXAMPLEPID","thumbUrl":"https://example.invalid/x.jpg","time":"20260817T203200","token":"x","utcTime":"20260817T183201","appId":"example_app_id","userId":"00000000"}"#;

        let payload: PushEvent = serde_json::from_str(raw).expect("real payload must parse");
        assert_eq!(payload.msg_type.as_deref(), Some("iotEvent"));
        assert_eq!(payload.did.as_deref(), Some("EXAMPLE0DEVICEID"));
        assert_eq!(payload.dname.as_deref(), Some("frontdoor"));
        assert_eq!(
            payload.content.as_ref().and_then(|c| c.event.as_deref()),
            Some("34500")
        );

        let utc_epoch = parse_push_timestamp(payload.utc_time.as_deref().unwrap()).unwrap();
        let time_epoch = parse_push_timestamp(payload.time.as_deref().unwrap()).unwrap();

        // utcTime "20260817T183201" is genuinely UTC; time "20260817T203200"
        // is local wall-clock digits treated as if UTC (same convention as
        // getAlarmMessage's `time` field).
        assert_eq!(utc_epoch, 1786991521);
        assert_eq!(time_epoch, 1786998720);
    }

    #[test]
    fn empty_ping_body_parses_with_no_fields() {
        let payload: PushEvent = serde_json::from_str("{}").expect("empty ping must parse");
        assert!(payload.msg_type.is_none());
        assert!(payload.did.is_none());
        assert!(payload.content.is_none());
    }

    #[test]
    fn rejects_unparseable_timestamp() {
        assert_eq!(parse_push_timestamp("not-a-timestamp"), None);
    }
}
