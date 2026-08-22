use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Local, NaiveDateTime, Utc};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::api::alarm::Alarm;
use crate::error::{ImouError, Result};
use crate::gdrive::{self, GDriveClient};

const SEGMENT_TIME_SECS: u32 = 2;
const SEGMENT_TS_FMT: &str = "%Y%m%dT%H%M%S";
const RESTART_BACKOFF: StdDuration = StdDuration::from_secs(5);
/// Passed to ffmpeg's RTSP-demuxer-private `-timeout` option (microseconds;
/// the generic `-rw_timeout` AVOption is *not* honored by the rtsp demuxer
/// itself — confirmed live, ffmpeg rejects it with "Option not found" when
/// `-rtsp_transport tcp` is in play) so a stalled RTSP TCP connection
/// (half-open, no data, no error) makes ffmpeg exit instead of hanging
/// forever — without this, a stalled camera connection silently stops
/// producing segments but the process never exits, so the auto-restart loop
/// below (which only triggers on exit) never fires. Same failure class
/// documented in CLAUDE.md for the `watch` poll loop.
const RTSP_READ_TIMEOUT_USECS: u64 = 15_000_000;
const CLEANUP_INTERVAL: StdDuration = StdDuration::from_secs(5);
const LOCAL_RETENTION_SWEEP_INTERVAL: StdDuration = StdDuration::from_secs(24 * 60 * 60);
/// Extra time to wait past a segment's nominal start before trusting it
/// covers that instant, and past a clip window's end before assuming
/// ffmpeg has flushed the segment that covers it to disk.
const FLUSH_MARGIN: chrono::Duration = chrono::Duration::seconds(5);

pub struct LocalCameraConfig {
    pub ip: String,
    pub secure: String,
}

/// Reads `CAM_{NAME}_IP` / `CAM_{NAME}_SECURE` from the environment, where
/// `{NAME}` is `channel_name` upper-cased (e.g. "ingresso" -> CAM_INGRESSO_IP).
/// Returns `None` if either is missing — that channel simply isn't recorded
/// locally, motion logging continues as normal.
pub fn local_config_for(channel_name: &str) -> Option<LocalCameraConfig> {
    let key = channel_name.to_uppercase();
    let ip = std::env::var(format!("CAM_{key}_IP")).ok()?;
    let secure = std::env::var(format!("CAM_{key}_SECURE")).ok()?;
    Some(LocalCameraConfig { ip, secure })
}

/// Main HD stream (`subtype=0`) over local RTSP — verified live against a
/// real camera (Digest auth, username always `admin` for this Dahua/Imou
/// family). This never touches the Imou cloud.
fn rtsp_url(cfg: &LocalCameraConfig) -> String {
    format!(
        "rtsp://admin:{}@{}:554/cam/realmonitor?channel=1&subtype=0",
        cfg.secure, cfg.ip
    )
}

pub async fn check_ffmpeg_available() -> Result<()> {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map_err(|_| {
            ImouError::Config(
                "ffmpeg not found on PATH — required for local clip recording in `watch`".into(),
            )
        })?;
    Ok(())
}

fn channel_buffer_dir(buffer_dir: &Path, channel_name: &str) -> PathBuf {
    buffer_dir.join(channel_name)
}

/// Ring-buffer recorders started for every channel that has local RTSP
/// config, shared between `watch` (polling) and `listen` (push) — both
/// need the identical "start a recorder per configured channel, track
/// which channels are covered, stop everything cleanly on shutdown" setup.
pub struct RecordingSetup {
    pub recorded_channels: HashSet<String>,
    shutdown_tx: watch::Sender<bool>,
    handles: Vec<JoinHandle<()>>,
}

/// Starts a ring-buffer recorder + cleanup sweep for every `(device_id,
/// channel_id, channel_name)` in `channels` that has `CAM_<NAME>_IP`/
/// `_SECURE` configured. Checks `ffmpeg` is on `PATH` first (only if at
/// least one channel needs it) so missing ffmpeg fails fast at startup
/// rather than on the first trigger.
pub async fn start_all(
    channels: &[(String, String, String)],
    buffer_dir: &Path,
    retention: StdDuration,
) -> Result<RecordingSetup> {
    let recorded_channels: HashSet<String> = channels
        .iter()
        .filter(|(_, _, name)| local_config_for(name).is_some())
        .map(|(_, _, name)| name.clone())
        .collect();

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut handles = Vec::new();

    if !recorded_channels.is_empty() {
        check_ffmpeg_available().await?;
        for (_, _, channel_name) in channels {
            let Some(cfg) = local_config_for(channel_name) else {
                println!(
                    "no local RTSP config for '{channel_name}' (CAM_{}_IP/_SECURE not set) — motion will be logged but not recorded",
                    channel_name.to_uppercase()
                );
                continue;
            };
            let (rec, cleanup) = start_ring_buffer(
                channel_name.clone(),
                cfg,
                buffer_dir.to_path_buf(),
                retention,
                shutdown_rx.clone(),
            );
            handles.push(rec);
            handles.push(cleanup);
        }
        println!(
            "recording local clips for: {} (retention {}s, buffer {})",
            recorded_channels.iter().cloned().collect::<Vec<_>>().join(", "),
            retention.as_secs(),
            buffer_dir.display()
        );
    }

    Ok(RecordingSetup {
        recorded_channels,
        shutdown_tx,
        handles,
    })
}

/// Signals every recorder to stop (killing its ffmpeg child) and waits for
/// them to actually exit, so no orphaned process survives the caller's exit.
pub async fn shutdown_all(setup: RecordingSetup) {
    if setup.handles.is_empty() {
        return;
    }
    let _ = setup.shutdown_tx.send(true);
    for handle in setup.handles {
        let _ = tokio::time::timeout(StdDuration::from_secs(10), handle).await;
    }
}

/// Spawns `extract_clip` as a detached task — used by both `watch` and
/// `listen` so a slow clip extraction never blocks their respective
/// detection loops (polling / the push HTTP handler). If `gdrive` is
/// configured, a successfully extracted clip is also uploaded — same
/// fire-and-forget/log-only error handling as extraction itself, and the
/// same "no-op if unconfigured" convention as `local_config_for`. The local
/// file is left untouched either way: its lifetime is governed independently
/// by `start_local_retention_sweep`, not by upload success.
pub fn spawn_clip_extraction(
    channel_name: String,
    buffer_dir: PathBuf,
    clips_dir: PathBuf,
    alarm: Alarm,
    pre_roll: StdDuration,
    post_roll: StdDuration,
    gdrive: Option<Arc<GDriveClient>>,
) {
    tokio::spawn(async move {
        match extract_clip(&channel_name, &buffer_dir, &clips_dir, &alarm, pre_roll, post_roll).await {
            Ok(out_path) => {
                if let Some(client) = gdrive {
                    let date = alarm_local_time(&alarm).map(|dt| dt.date_naive());
                    let upload_result = match date {
                        Ok(date) => gdrive::upload_clip(&client, &out_path, &channel_name, date).await,
                        Err(e) => Err(e),
                    };
                    if let Err(e) = upload_result {
                        eprintln!(
                            "warning: Google Drive upload failed for {channel_name}: {e} (clip kept locally at {})",
                            out_path.display()
                        );
                    }
                }
            }
            Err(e) => eprintln!("warning: clip extraction failed for {channel_name}: {e}"),
        }
    });
}

/// Spawns the continuous ring-buffer recorder (auto-restarting on failure)
/// and its periodic cleanup sweep for one channel. Both stop when
/// `shutdown_rx` reports `true`; the recorder kills its ffmpeg child before
/// returning so no process is left running past `watch`'s exit.
pub fn start_ring_buffer(
    channel_name: String,
    cfg: LocalCameraConfig,
    buffer_dir: PathBuf,
    retention: StdDuration,
    mut shutdown_rx: watch::Receiver<bool>,
) -> (JoinHandle<()>, JoinHandle<()>) {
    let dir = channel_buffer_dir(&buffer_dir, &channel_name);
    let url = rtsp_url(&cfg);

    let recorder_dir = dir.clone();
    let recorder_name = channel_name.clone();
    let recorder_shutdown = shutdown_rx.clone();
    let recorder_task = tokio::spawn(async move {
        run_recorder_supervisor(recorder_name, url, recorder_dir, recorder_shutdown).await;
    });

    let cleanup_dir = dir;
    let cleanup_name = channel_name;
    let cleanup_task = tokio::spawn(async move {
        loop {
            if let Err(e) = cleanup_old_segments(&cleanup_dir, retention).await {
                eprintln!("warning: buffer cleanup failed for {cleanup_name}: {e}");
            }
            tokio::select! {
                _ = tokio::time::sleep(CLEANUP_INTERVAL) => {}
                _ = shutdown_rx.changed() => break,
            }
            if *shutdown_rx.borrow() {
                break;
            }
        }
    });

    (recorder_task, cleanup_task)
}

async fn run_recorder_supervisor(
    channel_name: String,
    rtsp_url: String,
    dir: PathBuf,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        eprintln!("error: cannot create buffer dir for {channel_name}: {e}");
        return;
    }

    let log_path = dir.join("ffmpeg.log");

    loop {
        if *shutdown_rx.borrow() {
            return;
        }

        let log_stdio = match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            Ok(f) => Stdio::from(f),
            Err(e) => {
                eprintln!("warning: cannot open ffmpeg log for {channel_name}: {e}");
                Stdio::null()
            }
        };

        let pattern = dir.join(format!("seg_{SEGMENT_TS_FMT}.ts"));
        let mut child = match Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "warning",
                "-rtsp_transport",
                "tcp",
                "-timeout",
                &RTSP_READ_TIMEOUT_USECS.to_string(),
                "-i",
                &rtsp_url,
                "-c",
                "copy",
                "-f",
                "segment",
                "-segment_time",
                &SEGMENT_TIME_SECS.to_string(),
                "-reset_timestamps",
                "1",
                "-strftime",
                "1",
            ])
            .arg(&pattern)
            .stdout(Stdio::null())
            .stderr(log_stdio)
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("warning: failed to spawn ffmpeg recorder for {channel_name}: {e}");
                tokio::time::sleep(RESTART_BACKOFF).await;
                continue;
            }
        };

        tokio::select! {
            status = child.wait() => {
                match status {
                    Ok(s) if s.success() => {
                        eprintln!("recorder for {channel_name} exited cleanly, restarting");
                    }
                    Ok(s) => {
                        eprintln!(
                            "warning: recorder for {channel_name} exited with {s} (see {}), restarting",
                            log_path.display()
                        );
                    }
                    Err(e) => {
                        eprintln!("warning: recorder for {channel_name} wait() failed: {e}");
                    }
                }
            }
            _ = shutdown_rx.changed() => {
                let _ = child.kill().await;
                return;
            }
        }

        if *shutdown_rx.borrow() {
            return;
        }
        tokio::time::sleep(RESTART_BACKOFF).await;
    }
}

/// Parses `seg_YYYYMMDDTHHMMSS.ts` filenames as naive local wall-clock
/// timestamps (matching ffmpeg's `-strftime 1`, which formats using the
/// process's local time — same convention `watch.rs` already uses for
/// `getAlarmMessage` windows, for the same underlying reason).
fn parse_segment_time(path: &Path) -> Option<NaiveDateTime> {
    let stem = path.file_stem()?.to_str()?;
    let ts = stem.strip_prefix("seg_")?;
    NaiveDateTime::parse_from_str(ts, SEGMENT_TS_FMT).ok()
}

async fn cleanup_old_segments(dir: &Path, retention: StdDuration) -> Result<()> {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    let cutoff = Local::now().naive_local() - chrono::Duration::from_std(retention).unwrap();

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("ts") {
            continue;
        }
        if let Some(ts) = parse_segment_time(&path)
            && ts < cutoff
        {
            let _ = tokio::fs::remove_file(&path).await;
        }
    }
    Ok(())
}

/// Parses the `<YYYYmmddTHHMMSS>_<alarm_id>.mp4` filenames written by
/// `extract_clip` — same local-wall-clock convention as `parse_segment_time`,
/// just a different suffix after the timestamp (an alarm id instead of
/// nothing). The timestamp portion itself never contains `_`, so splitting
/// on the first one is safe.
fn parse_clip_time(path: &Path) -> Option<NaiveDateTime> {
    let stem = path.file_stem()?.to_str()?;
    let ts = stem.split('_').next()?;
    NaiveDateTime::parse_from_str(ts, SEGMENT_TS_FMT).ok()
}

async fn sweep_local_clips(clips_dir: &Path, retention_days: u32) -> Result<()> {
    let mut channels = match tokio::fs::read_dir(clips_dir).await {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    let cutoff = Local::now().naive_local() - chrono::Duration::days(retention_days as i64);

    while let Some(channel_entry) = channels.next_entry().await? {
        if !channel_entry.file_type().await?.is_dir() {
            continue;
        }
        let mut entries = tokio::fs::read_dir(channel_entry.path()).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("mp4") {
                continue;
            }
            if let Some(ts) = parse_clip_time(&path)
                && ts < cutoff
            {
                let _ = tokio::fs::remove_file(&path).await;
            }
        }
    }
    Ok(())
}

/// Runs `sweep_local_clips` every 24h for as long as the process runs —
/// same detached, no-shutdown-signal reasoning as
/// `gdrive::retention::start_retention_sweep` (no OS resource is held here
/// either). Independent of whether Google Drive upload is configured: a
/// clip's local lifetime is no longer tied to its upload outcome (see
/// `spawn_clip_extraction`), so this sweep is what actually bounds
/// `clips_dir`'s growth now. `retention_days == 0` disables it (kept
/// forever), same convention as the Drive-side sweep.
pub fn start_local_retention_sweep(clips_dir: PathBuf, retention_days: u32) {
    if retention_days == 0 {
        return;
    }
    tokio::spawn(async move {
        loop {
            if let Err(e) = sweep_local_clips(&clips_dir, retention_days).await {
                eprintln!("warning: local clip retention sweep failed: {e}");
            }
            tokio::time::sleep(LOCAL_RETENTION_SWEEP_INTERVAL).await;
        }
    });
}

/// Parses `alarm.utc_time` into the account's local wall-clock time — same
/// convention as `Alarm`'s own doc note. Shared by `extract_clip` (to place
/// the pre/post-roll window) and by `spawn_clip_extraction` (to pick the
/// day folder a Google Drive upload belongs in), so both always agree on
/// which day a clip belongs to.
fn alarm_local_time(alarm: &Alarm) -> Result<DateTime<Local>> {
    alarm
        .utc_time
        .parse::<i64>()
        .ok()
        .and_then(|secs| DateTime::<Utc>::from_timestamp(secs, 0))
        .map(|dt| dt.with_timezone(&Local))
        .ok_or_else(|| ImouError::Config(format!("alarm {} has invalid utc_time", alarm.alarm_id)))
}

/// Waits for the buffer to have accumulated `alarm.utc_time - pre_roll ..
/// alarm.utc_time + post_roll`, then concatenates the covering segments
/// into `<clips_dir>/<channel_name>/<local-time>_<alarm_id>.mp4`. Returns
/// the written file's path so callers (e.g. a Google Drive upload step)
/// know what to act on next.
pub async fn extract_clip(
    channel_name: &str,
    buffer_dir: &Path,
    clips_dir: &Path,
    alarm: &Alarm,
    pre_roll: StdDuration,
    post_roll: StdDuration,
) -> Result<PathBuf> {
    let alarm_local = alarm_local_time(alarm)?;

    let window_start = alarm_local - chrono::Duration::from_std(pre_roll).unwrap();
    let window_end = alarm_local + chrono::Duration::from_std(post_roll).unwrap();

    let target = window_end + FLUSH_MARGIN;
    let now = Local::now();
    if target > now
        && let Ok(d) = (target - now).to_std()
    {
        tokio::time::sleep(d).await;
    }

    let dir = channel_buffer_dir(buffer_dir, channel_name);
    let mut segments = Vec::new();
    let mut entries = tokio::fs::read_dir(&dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("ts") {
            continue;
        }
        if let Some(ts) = parse_segment_time(&path) {
            // A segment covers [ts, ts + SEGMENT_TIME_SECS); include it if
            // that span overlaps the requested window at all.
            let seg_end = ts + chrono::Duration::seconds(SEGMENT_TIME_SECS as i64);
            if seg_end >= window_start.naive_local() && ts <= window_end.naive_local() {
                segments.push((ts, path));
            }
        }
    }

    if segments.is_empty() {
        return Err(ImouError::Config(format!(
            "no buffered segments found for {channel_name} covering alarm {}",
            alarm.alarm_id
        )));
    }
    segments.sort_by_key(|(ts, _)| *ts);

    let out_dir = clips_dir.join(channel_name);
    tokio::fs::create_dir_all(&out_dir).await?;

    let file_stamp = alarm_local.format("%Y%m%dT%H%M%S");
    let out_path = out_dir.join(format!("{file_stamp}_{}.mp4", alarm.alarm_id));
    let list_path = out_dir.join(format!(".concat_{}.txt", alarm.alarm_id));

    let mut list_contents = String::new();
    for (_, path) in &segments {
        let abs = tokio::fs::canonicalize(path).await?;
        // ffmpeg concat demuxer format; escape single quotes per its rules.
        let s = abs.to_string_lossy().replace('\'', "'\\''");
        list_contents.push_str(&format!("file '{s}'\n"));
    }
    {
        let mut f = tokio::fs::File::create(&list_path).await?;
        f.write_all(list_contents.as_bytes()).await?;
    }

    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "warning", "-y", "-f", "concat", "-safe", "0", "-i"])
        .arg(&list_path)
        .args(["-c", "copy"])
        .arg(&out_path)
        .output()
        .await?;

    let _ = tokio::fs::remove_file(&list_path).await;

    if !output.status.success() {
        return Err(ImouError::Config(format!(
            "ffmpeg concat failed for {channel_name}/{}: {}",
            alarm.alarm_id,
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    println!("clip saved: {}", out_path.display());
    Ok(out_path)
}
