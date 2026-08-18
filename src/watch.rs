use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Local};

use crate::api;
use crate::api::alarm::Alarm;
use crate::client::ImouClient;
use crate::error::Result;
use crate::event_log::EventLog;
use crate::gdrive;
use crate::motion_event::MotionEvent;
use crate::recorder;

/// Extra safety margin added on top of `pre_roll + interval` when computing
/// how long buffered segments must be retained — covers processing/flush
/// slack, not just the raw detection-latency math.
const RETENTION_MARGIN: Duration = Duration::from_secs(15);

/// How many pages of `getAlarmMessage` to drain per channel per poll cycle
/// before giving up — a safety bound, not expected to be hit in normal use
/// (30 alarms/page; a home account isn't generating hundreds of alarms
/// between polls).
const MAX_PAGES_PER_POLL: u32 = 20;

struct ChannelCursor {
    /// Start of the next query window; advances to "now" after each poll.
    /// Local time, not UTC — see the note on `Alarm` in `api::alarm`:
    /// `getAlarmMessage` filters `beginTime`/`endTime` against the account's
    /// local wall-clock time, so a UTC-based window silently misses every
    /// event less than the account's UTC offset old.
    begin_time: DateTime<Local>,
    /// Highest alarmId processed so far for this channel, to avoid
    /// re-logging an alarm that falls in the same second as the last poll's
    /// end time (beginTime/endTime only have second resolution).
    last_alarm_id: u64,
}

fn format_time(dt: DateTime<Local>) -> String {
    dt.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Fetches all new alarms for one channel since `cursor`, draining
/// pagination via `nextAlarmId`, and advances `cursor` to `poll_time`.
async fn poll_channel(
    client: &ImouClient,
    device_id: &str,
    channel_id: &str,
    cursor: &mut ChannelCursor,
    poll_time: DateTime<Local>,
) -> Result<Vec<Alarm>> {
    let begin = format_time(cursor.begin_time);
    let end = format_time(poll_time);

    let mut collected = Vec::new();
    let mut next_id: i64 = -1;

    for _ in 0..MAX_PAGES_PER_POLL {
        let page =
            api::alarm::get_alarm_message(client, device_id, channel_id, &begin, &end, next_id)
                .await?;
        let got = page.alarms.len();

        for alarm in page.alarms {
            if let Ok(id) = alarm.alarm_id.parse::<u64>()
                && id > cursor.last_alarm_id
            {
                cursor.last_alarm_id = id;
                collected.push(alarm);
            }
        }

        if got < 30 || page.next_alarm_id == next_id {
            break;
        }
        next_id = page.next_alarm_id;
    }

    cursor.begin_time = poll_time;
    Ok(collected)
}

/// Runs a foreground loop that polls `getAlarmMessage` for every bound
/// device/channel every `interval`, appending a JSON line to `events_file`
/// for each motion-detection alarm, until interrupted (Ctrl+C). For any
/// channel with a `CAM_<NAME>_IP`/`CAM_<NAME>_SECURE` pair in the
/// environment (see `recorder::local_config_for`), also keeps a rolling
/// local RTSP recording buffer and, on each new alarm, extracts a
/// `pre_roll..post_roll` clip into `clips_dir` — entirely over the LAN, no
/// cloud API involved for the video itself.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    client: &ImouClient,
    interval: Duration,
    events_file: &Path,
    buffer_dir: &Path,
    clips_dir: &Path,
    pre_roll: Duration,
    post_roll: Duration,
    gdrive_retention_days: u32,
) -> Result<()> {
    let event_log = EventLog::open(events_file)?;

    let devices = api::devices::list(client, 100).await?;
    let mut channels = Vec::new();
    let mut cursors: HashMap<(String, String), ChannelCursor> = HashMap::new();
    let start = Local::now();

    for d in &devices.device_list {
        for ch in &d.channels {
            let key = (d.device_id.clone(), ch.channel_id.clone());
            cursors.insert(
                key,
                ChannelCursor {
                    begin_time: start,
                    last_alarm_id: 0,
                },
            );
            channels.push((d.device_id.clone(), ch.channel_id.clone(), ch.channel_name.clone()));
        }
    }

    if channels.is_empty() {
        println!("no devices/channels bound to this account — nothing to watch");
        return Ok(());
    }

    // Retention must cover the pre-roll plus the worst-case detection lag
    // (up to a full poll `interval`), or the pre-roll footage can already be
    // gone from the buffer by the time an alarm is actually discovered.
    let retention = pre_roll + interval + RETENTION_MARGIN;

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

    println!(
        "watching {} channel(s) across {} device(s), polling every {}s, writing motion events to {}",
        channels.len(),
        devices.device_list.len(),
        interval.as_secs(),
        events_file.display()
    );
    println!("press Ctrl+C to stop");

    let mut shutdown = Box::pin(tokio::signal::ctrl_c());

    loop {
        for (device_id, channel_id, channel_name) in &channels {
            let cursor = cursors
                .get_mut(&(device_id.clone(), channel_id.clone()))
                .expect("cursor initialized for every channel above");
            let poll_time = Local::now();

            match poll_channel(client, device_id, channel_id, cursor, poll_time).await {
                Ok(alarms) => {
                    // getAlarmMessage is the device's motion/security alarm
                    // feed by definition (governed by the motion detection
                    // config APIs) — every entry it returns is logged, since
                    // the doc's `type` code table doesn't hold up against
                    // real data (see the note on `Alarm`).
                    for alarm in alarms {
                        let event = MotionEvent::from_alarm(&alarm, channel_name);
                        match event_log.append(&event) {
                            Ok(()) => println!(
                                "motion detected: device={device_id} channel={channel_id} ({channel_name})"
                            ),
                            Err(e) => eprintln!("failed to write motion event: {e}"),
                        }

                        if recording.recorded_channels.contains(channel_name) {
                            recorder::spawn_clip_extraction(
                                channel_name.clone(),
                                buffer_dir.to_path_buf(),
                                clips_dir.to_path_buf(),
                                alarm,
                                pre_roll,
                                post_roll,
                                gdrive_client.clone(),
                            );
                        }
                    }
                }
                Err(e) => {
                    eprintln!("warning: poll failed for device={device_id} channel={channel_id}: {e}");
                }
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = &mut shutdown => {
                println!("stopping...");
                break;
            }
        }
    }

    recorder::shutdown_all(recording).await;

    Ok(())
}
