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
use crate::event_log::EventLog;
use crate::gdrive::{self, GDriveClient};
use crate::motion_event::MotionAnalysisEvent;
use crate::mqtt::MqttPublisher;

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

fn channel_continuous_dir(continuous_dir: &Path, channel_name: &str) -> PathBuf {
    continuous_dir.join(channel_name)
}

/// Config for the optional continuous (non-motion-triggered) recording
/// archive — see CLAUDE.md's "Continuous recording" section. `None`
/// anywhere in this module means the feature is off, same "no-op if
/// unconfigured" convention as `LocalCameraConfig`'s own absence per
/// channel. Unlike other retention flags in this codebase (where `0`
/// means "keep forever"), `retention_hours == 0` here means "feature
/// disabled" — this is the only feature costly enough (tens of GB per
/// camera per day) to need an explicit opt-in via its own retention
/// value, not just an always-on sweep.
#[derive(Clone)]
pub struct ContinuousConfig {
    pub dir: PathBuf,
    pub segment_minutes: u32,
    pub retention_hours: u32,
}

/// Which wall-clock convention to use for every filename this module
/// writes (ring buffer/continuous segments, clips, snapshots) and for the
/// retention sweeps that later parse those same filenames back — the two
/// must always agree, or a sweep computes its cutoff against the wrong
/// "now" for what it's comparing against. Deliberately does NOT affect
/// `MotionEvent`/`MotionAnalysisEvent`'s own `time`/`local_time` fields
/// (already a well-defined UTC/local pair, unrelated to filenames), nor
/// `watch.rs`'s polling windows (tied to `getAlarmMessage`'s own
/// local-as-if-UTC filtering behavior, not a display preference — see
/// CLAUDE.md), nor the Google Drive day-folder grouping (always the real
/// local calendar day, regardless of this setting).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum FilenameTimezone {
    Utc,
    Local,
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
    continuous: Option<ContinuousConfig>,
    filename_tz: FilenameTimezone,
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
            let tasks = start_ring_buffer(
                channel_name.clone(),
                cfg,
                buffer_dir.to_path_buf(),
                retention,
                continuous.clone(),
                filename_tz,
                shutdown_rx.clone(),
            );
            handles.extend(tasks);
        }
        println!(
            "recording local clips for: {} (retention {}s, buffer {})",
            recorded_channels.iter().cloned().collect::<Vec<_>>().join(", "),
            retention.as_secs(),
            buffer_dir.display()
        );
        if let Some(cont) = &continuous {
            println!(
                "recording continuous archive to {} (segments: {}min, retention: {}h)",
                cont.dir.display(),
                cont.segment_minutes,
                cont.retention_hours
            );
        }
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
///
/// If `vision` is configured, AI analysis runs after extraction succeeds:
/// the result is appended to `event_log` and published via `mqtt` (if
/// configured) as a deferred `MotionAnalysisEvent` — see that type's doc
/// note for why this is a second, later event rather than folded into the
/// immediate `MotionEvent` notification. `ai_gate_gdrive_upload`/
/// `ai_gate_mqtt_analyzed` make the Drive upload / analyzed-event MQTT
/// publish conditional on the AI verdict; both fail open — an AI call that
/// errors or isn't configured never suppresses the existing behavior.
///
/// Independently, a frame-diff snapshot is always extracted into
/// `snapshots_dir` (see `imou_vision::snapshot`) — unlike `vision`, this
/// isn't opt-in/config-gated: it's local-only pixel arithmetic (no network,
/// no credentials), cheap enough on any hardware that ships ffmpeg, so it's
/// just a normal part of the local clip pipeline. A failure here only logs
/// a warning, same as everything else in this function.
#[allow(clippy::too_many_arguments)]
pub fn spawn_clip_extraction(
    channel_name: String,
    buffer_dir: PathBuf,
    clips_dir: PathBuf,
    snapshots_dir: PathBuf,
    snapshot_config: Arc<imou_vision::SnapshotConfig>,
    alarm: Alarm,
    pre_roll: StdDuration,
    post_roll: StdDuration,
    gdrive: Option<Arc<GDriveClient>>,
    mqtt: Option<Arc<MqttPublisher>>,
    event_log: Arc<EventLog>,
    vision: Option<Arc<imou_vision::VisionClient>>,
    ai_gate_gdrive_upload: bool,
    ai_gate_mqtt_analyzed: bool,
    filename_tz: FilenameTimezone,
) {
    tokio::spawn(async move {
        match extract_clip(&channel_name, &buffer_dir, &clips_dir, &alarm, pre_roll, post_roll, filename_tz).await {
            Ok(out_path) => {
                // Same `<timestamp>_<alarm_id>` stem `extract_clip` uses for
                // the .mp4 itself (same `filename_tz`), so a clip and its
                // snapshot(s) are trivially correlated by filename.
                if let Ok(alarm_time) = alarm_filename_time(&alarm, filename_tz) {
                    let channel_snapshot_dir = snapshots_dir.join(&channel_name);
                    let file_prefix = format!("{}_{}", alarm_time.format("%Y%m%dT%H%M%S"), alarm.alarm_id);
                    match imou_vision::snapshot::extract_snapshots(
                        &out_path,
                        &channel_snapshot_dir,
                        &file_prefix,
                        &snapshot_config,
                    )
                    .await
                    {
                        Ok(paths) => {
                            for p in paths {
                                println!("snapshot saved: {}", p.display());
                            }
                        }
                        Err(e) => eprintln!("warning: snapshot extraction failed for {channel_name}: {e}"),
                    }
                }

                let analysis = match &vision {
                    Some(v) => match imou_vision::analyze_clip(v, &out_path).await {
                        Ok(r) => Some(r),
                        Err(e) => {
                            eprintln!(
                                "warning: AI analysis failed for {channel_name}: {e} (treating as inconclusive, not blocking upload)"
                            );
                            None
                        }
                    },
                    None => None,
                };

                // Fail open: only skip upload on a confident, successfully
                // parsed "not relevant" verdict. AI misconfigured,
                // unreachable, or erroring never blocks the existing
                // upload behavior.
                let should_upload = match &analysis {
                    Some(r) if ai_gate_gdrive_upload => r.relevant,
                    _ => true,
                };

                let mut uploaded = false;
                if should_upload {
                    if let Some(client) = &gdrive {
                        let date = alarm_local_time(&alarm).map(|dt| dt.date_naive());
                        let upload_result = match date {
                            Ok(date) => gdrive::upload_clip(client, &out_path, &channel_name, date).await,
                            Err(e) => Err(e),
                        };
                        match upload_result {
                            Ok(()) => uploaded = true,
                            Err(e) => eprintln!(
                                "warning: Google Drive upload failed for {channel_name}: {e} (clip kept locally at {})",
                                out_path.display()
                            ),
                        }
                    }
                } else {
                    println!(
                        "skipping Google Drive upload for {channel_name}: AI found no relevant content in {}",
                        out_path.display()
                    );
                }

                if let Some(result) = &analysis {
                    let event = MotionAnalysisEvent::new(&alarm, &channel_name, result, uploaded);
                    if let Err(e) = event_log.append(&event) {
                        eprintln!("warning: failed to write AI analysis event: {e}");
                    }
                    if let Some(m) = &mqtt
                        && (!ai_gate_mqtt_analyzed || result.relevant)
                    {
                        m.publish_analysis(&event, &channel_name);
                    }
                }
            }
            Err(e) => eprintln!("warning: clip extraction failed for {channel_name}: {e}"),
        }
    });
}

/// Spawns the auto-restarting ring-buffer recorder and its periodic cleanup
/// sweep for one channel, plus — if `continuous` is configured — a second
/// cleanup sweep for the continuous archive directory (the recorder itself
/// writes both from the single ffmpeg process spawned by
/// `run_recorder_supervisor`; see that function). All tasks stop when
/// `shutdown_rx` reports `true`; the recorder kills its ffmpeg child before
/// returning so no process is left running past `watch`'s exit.
#[allow(clippy::too_many_arguments)]
pub fn start_ring_buffer(
    channel_name: String,
    cfg: LocalCameraConfig,
    buffer_dir: PathBuf,
    retention: StdDuration,
    continuous: Option<ContinuousConfig>,
    filename_tz: FilenameTimezone,
    shutdown_rx: watch::Receiver<bool>,
) -> Vec<JoinHandle<()>> {
    let dir = channel_buffer_dir(&buffer_dir, &channel_name);
    let url = rtsp_url(&cfg);

    let recorder_task = {
        let dir = dir.clone();
        let channel_name = channel_name.clone();
        let continuous = continuous.clone();
        let shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            run_recorder_supervisor(channel_name, url, dir, continuous, filename_tz, shutdown_rx).await;
        })
    };

    let cleanup_task = {
        let dir = dir.clone();
        let channel_name = channel_name.clone();
        let mut shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = cleanup_old_segments(&dir, "ts", retention, filename_tz).await {
                    eprintln!("warning: buffer cleanup failed for {channel_name}: {e}");
                }
                tokio::select! {
                    _ = tokio::time::sleep(CLEANUP_INTERVAL) => {}
                    _ = shutdown_rx.changed() => break,
                }
                if *shutdown_rx.borrow() {
                    break;
                }
            }
        })
    };

    let mut tasks = vec![recorder_task, cleanup_task];

    if let Some(cont) = continuous {
        let cont_dir = channel_continuous_dir(&cont.dir, &channel_name);
        let cont_retention = StdDuration::from_secs(cont.retention_hours as u64 * 3600);
        let mut shutdown_rx = shutdown_rx.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                if let Err(e) = cleanup_old_segments(&cont_dir, "mp4", cont_retention, filename_tz).await {
                    eprintln!("warning: continuous archive cleanup failed for {channel_name}: {e}");
                }
                tokio::select! {
                    _ = tokio::time::sleep(CLEANUP_INTERVAL) => {}
                    _ = shutdown_rx.changed() => break,
                }
                if *shutdown_rx.borrow() {
                    break;
                }
            }
        }));
    }

    tasks
}

/// Builds the ffmpeg argument vector for one recorder invocation: always the
/// short ring-buffer `.ts` segment output at `pattern`, plus — when
/// `continuous_branch` is `Some((path, segment_time_secs))` — a second,
/// independent `-f segment` output for the continuous archive. Pure
/// function (no I/O), so the exact command line can be asserted against
/// without spawning ffmpeg — see the tests below for the real command this
/// was verified against live.
fn build_recorder_args(rtsp_url: &str, pattern: &Path, continuous_branch: Option<&(PathBuf, u32)>) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "warning".into(),
        "-rtsp_transport".into(),
        "tcp".into(),
        "-timeout".into(),
        RTSP_READ_TIMEOUT_USECS.to_string(),
        "-i".into(),
        rtsp_url.to_string(),
        "-c".into(),
        "copy".into(),
        "-f".into(),
        "segment".into(),
        "-segment_time".into(),
        SEGMENT_TIME_SECS.to_string(),
        "-reset_timestamps".into(),
        "1".into(),
        "-strftime".into(),
        "1".into(),
        pattern.to_string_lossy().into_owned(),
    ];

    if let Some((cont_pattern, segment_time_secs)) = continuous_branch {
        args.extend([
            "-c".into(),
            "copy".into(),
            "-f".into(),
            "segment".into(),
            "-segment_time".into(),
            segment_time_secs.to_string(),
            "-reset_timestamps".into(),
            "1".into(),
            "-strftime".into(),
            "1".into(),
            cont_pattern.to_string_lossy().into_owned(),
        ]);
    }

    args
}

/// The `TZ` env var to set on the spawned ffmpeg child so its `-strftime`
/// segment naming uses the requested convention — `None` for `Local` means
/// "don't override, inherit the container's own `TZ`" (which must itself
/// be configured correctly for that to actually be local time; see
/// CLAUDE.md). Forcing `TZ=UTC` for the `Utc` case avoids needing to know
/// the real local IANA zone name at all. Pure function, no I/O.
fn ffmpeg_tz_env(tz: FilenameTimezone) -> Option<(&'static str, &'static str)> {
    match tz {
        FilenameTimezone::Utc => Some(("TZ", "UTC")),
        FilenameTimezone::Local => None,
    }
}

async fn run_recorder_supervisor(
    channel_name: String,
    rtsp_url: String,
    dir: PathBuf,
    continuous: Option<ContinuousConfig>,
    filename_tz: FilenameTimezone,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        eprintln!("error: cannot create buffer dir for {channel_name}: {e}");
        return;
    }

    let continuous_dir = match &continuous {
        Some(cont) => {
            let d = channel_continuous_dir(&cont.dir, &channel_name);
            if let Err(e) = tokio::fs::create_dir_all(&d).await {
                eprintln!("error: cannot create continuous archive dir for {channel_name}: {e}");
                return;
            }
            Some(d)
        }
        None => None,
    };

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
        // A single ffmpeg process/RTSP connection feeds both outputs — the
        // ring buffer (`.ts`, short retention, pre-roll source) and,
        // optionally, the continuous archive (`.mp4`, long retention) —
        // rather than opening a second connection to the same camera for
        // the archive. Verified live that ffmpeg accepts two independent
        // `-f segment` muxers (different segment_time/format) from one
        // input in a single invocation; untested how many concurrent RTSP
        // sessions these cameras tolerate, so avoiding a second connection
        // entirely is the safer default.
        let continuous_branch = continuous
            .as_ref()
            .zip(continuous_dir.as_ref())
            .map(|(cont, cont_dir)| (cont_dir.join(format!("seg_{SEGMENT_TS_FMT}.mp4")), cont.segment_minutes * 60));
        let args = build_recorder_args(&rtsp_url, &pattern, continuous_branch.as_ref());

        let mut command = Command::new("ffmpeg");
        command.args(&args).stdout(Stdio::null()).stderr(log_stdio).kill_on_drop(true);
        if let Some((key, value)) = ffmpeg_tz_env(filename_tz) {
            command.env(key, value);
        }

        let mut child = match command.spawn() {
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

/// Used for both the short-lived ring buffer (`extension = "ts"`) and the
/// continuous archive (`extension = "mp4"`) — same `seg_<timestamp>.<ext>`
/// naming, same `parse_segment_time`, just different directories,
/// extensions, and retention windows.
async fn cleanup_old_segments(dir: &Path, extension: &str, retention: StdDuration, tz: FilenameTimezone) -> Result<()> {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    let cutoff = now_naive(tz) - chrono::Duration::from_std(retention).unwrap();

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(extension) {
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

/// Sweeps `<root>/<channel>/*.<extension>` for files older than
/// `retention_days`, keyed off `parse_clip_time`'s naming convention. Used
/// for both `clips_dir` (`"mp4"`) and `snapshots_dir` (`"jpg"`) — snapshots
/// are written by `snapshot::extract_snapshots` using the exact same
/// `<timestamp>_<alarm_id>[_n].jpg` naming, so the existing parser applies
/// unchanged (it only reads the first `_`-delimited token).
async fn sweep_local_files(root: &Path, extension: &str, retention_days: u32, tz: FilenameTimezone) -> Result<()> {
    let mut channels = match tokio::fs::read_dir(root).await {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    let cutoff = now_naive(tz) - chrono::Duration::days(retention_days as i64);

    while let Some(channel_entry) = channels.next_entry().await? {
        if !channel_entry.file_type().await?.is_dir() {
            continue;
        }
        let mut entries = tokio::fs::read_dir(channel_entry.path()).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some(extension) {
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

/// Runs `sweep_local_files` every 24h for as long as the process runs —
/// same detached, no-shutdown-signal reasoning as
/// `gdrive::retention::start_retention_sweep` (no OS resource is held here
/// either). Independent of whether Google Drive upload is configured: a
/// clip's local lifetime is no longer tied to its upload outcome (see
/// `spawn_clip_extraction`), so this sweep is what actually bounds
/// `root`'s growth now. `retention_days == 0` disables it (kept forever),
/// same convention as the Drive-side sweep. Called once for `clips_dir`
/// (`"mp4"`) and once for `snapshots_dir` (`"jpg"`) — same policy, same
/// `--local-retention-days` flag, two independent sweep loops since they're
/// separate directory trees.
pub fn start_local_retention_sweep(root: PathBuf, extension: &'static str, retention_days: u32, tz: FilenameTimezone) {
    if retention_days == 0 {
        return;
    }
    tokio::spawn(async move {
        loop {
            if let Err(e) = sweep_local_files(&root, extension, retention_days, tz).await {
                eprintln!("warning: local retention sweep failed for {extension} files under {}: {e}", root.display());
            }
            tokio::time::sleep(LOCAL_RETENTION_SWEEP_INTERVAL).await;
        }
    });
}

/// Parses `alarm.utc_time` into the account's real local wall-clock time —
/// same convention as `Alarm`'s own doc note. Used only by
/// `spawn_clip_extraction` to pick the Google Drive day folder a clip
/// belongs to, which is always the real local calendar day regardless of
/// `FilenameTimezone` (see that type's doc note) — filename timestamps
/// themselves go through `alarm_filename_time` instead.
fn alarm_local_time(alarm: &Alarm) -> Result<DateTime<Local>> {
    alarm
        .utc_time
        .parse::<i64>()
        .ok()
        .and_then(|secs| DateTime::<Utc>::from_timestamp(secs, 0))
        .map(|dt| dt.with_timezone(&Local))
        .ok_or_else(|| ImouError::Config(format!("alarm {} has invalid utc_time", alarm.alarm_id)))
}

/// Parses `alarm.utc_time` into a naive wall-clock time in whichever
/// convention `tz` selects — UTC digits or real local digits. Used for
/// every filename this module writes (`extract_clip`'s output name, and
/// the pre/post-roll window used to select covering ring-buffer segments,
/// which must use the *same* convention those segments were themselves
/// named in — see `FilenameTimezone`'s doc note) plus the snapshot
/// filename prefix in `spawn_clip_extraction`.
fn alarm_filename_time(alarm: &Alarm, tz: FilenameTimezone) -> Result<NaiveDateTime> {
    let utc = alarm
        .utc_time
        .parse::<i64>()
        .ok()
        .and_then(|secs| DateTime::<Utc>::from_timestamp(secs, 0))
        .ok_or_else(|| ImouError::Config(format!("alarm {} has invalid utc_time", alarm.alarm_id)))?;
    Ok(match tz {
        FilenameTimezone::Utc => utc.naive_utc(),
        FilenameTimezone::Local => utc.with_timezone(&Local).naive_local(),
    })
}

/// "Now," naive, in whichever convention `tz` selects — must match
/// whatever convention was used to write the filenames being compared
/// against (retention sweep cutoffs, or `extract_clip`'s "has the window
/// landed on disk yet" check).
fn now_naive(tz: FilenameTimezone) -> NaiveDateTime {
    match tz {
        FilenameTimezone::Utc => Utc::now().naive_utc(),
        FilenameTimezone::Local => Local::now().naive_local(),
    }
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
    tz: FilenameTimezone,
) -> Result<PathBuf> {
    let alarm_time = alarm_filename_time(alarm, tz)?;

    let window_start = alarm_time - chrono::Duration::from_std(pre_roll).unwrap();
    let window_end = alarm_time + chrono::Duration::from_std(post_roll).unwrap();

    let target = window_end + FLUSH_MARGIN;
    let now = now_naive(tz);
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
            if seg_end >= window_start && ts <= window_end {
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

    let file_stamp = alarm_time.format("%Y%m%dT%H%M%S");
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

#[cfg(test)]
mod tests {
    use super::*;

    // Matches, argument-for-argument, the command verified live to produce
    // two independent, correctly-rotating segment streams from one ffmpeg
    // process/RTSP connection (see CLAUDE.md's "Continuous recording"
    // section) — this test pins that exact shape so a future refactor
    // can't silently drop or reorder a flag ffmpeg turned out to need.
    #[test]
    fn ring_buffer_only_when_continuous_not_configured() {
        let args = build_recorder_args("rtsp://cam/stream", Path::new("/buf/ingresso/seg_%Y%m%dT%H%M%S.ts"), None);
        assert_eq!(
            args,
            vec![
                "-hide_banner",
                "-loglevel",
                "warning",
                "-rtsp_transport",
                "tcp",
                "-timeout",
                &RTSP_READ_TIMEOUT_USECS.to_string(),
                "-i",
                "rtsp://cam/stream",
                "-c",
                "copy",
                "-f",
                "segment",
                "-segment_time",
                "2",
                "-reset_timestamps",
                "1",
                "-strftime",
                "1",
                "/buf/ingresso/seg_%Y%m%dT%H%M%S.ts",
            ]
        );
    }

    #[test]
    fn adds_second_independent_segment_output_when_continuous_configured() {
        let branch = (PathBuf::from("/continuous/ingresso/seg_%Y%m%dT%H%M%S.mp4"), 900u32);
        let args = build_recorder_args(
            "rtsp://cam/stream",
            Path::new("/buf/ingresso/seg_%Y%m%dT%H%M%S.ts"),
            Some(&branch),
        );
        // First branch (ring buffer) unchanged...
        assert_eq!(&args[..20], &build_recorder_args("rtsp://cam/stream", Path::new("/buf/ingresso/seg_%Y%m%dT%H%M%S.ts"), None)[..]);
        // ...followed by a second, independent `-c copy -f segment` branch
        // for the continuous archive, same flags, different segment_time
        // and output path.
        assert_eq!(
            &args[20..],
            &[
                "-c",
                "copy",
                "-f",
                "segment",
                "-segment_time",
                "900",
                "-reset_timestamps",
                "1",
                "-strftime",
                "1",
                "/continuous/ingresso/seg_%Y%m%dT%H%M%S.mp4",
            ]
        );
    }

    #[test]
    fn ffmpeg_tz_env_forces_utc_only_for_utc_mode() {
        assert_eq!(ffmpeg_tz_env(FilenameTimezone::Utc), Some(("TZ", "UTC")));
        assert_eq!(ffmpeg_tz_env(FilenameTimezone::Local), None);
    }

    fn test_alarm(utc_time: &str) -> Alarm {
        Alarm {
            alarm_id: "1".into(),
            device_id: "dev".into(),
            channel_id: "0".into(),
            name: "ingresso".into(),
            time: 0,
            utc_time: utc_time.into(),
            alarm_type: "34500".into(),
            label_type: String::new(),
        }
    }

    #[test]
    fn alarm_filename_time_utc_matches_the_epoch_exactly() {
        let epoch = chrono::NaiveDate::from_ymd_opt(2026, 8, 22)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp();
        let alarm = test_alarm(&epoch.to_string());
        let t = alarm_filename_time(&alarm, FilenameTimezone::Utc).unwrap();
        assert_eq!(t.format("%Y%m%dT%H%M%S").to_string(), "20260822T120000");
    }

    #[test]
    fn alarm_filename_time_local_matches_manual_conversion() {
        let alarm = test_alarm("1787479200");
        let expected = DateTime::<Utc>::from_timestamp(1787479200, 0).unwrap().with_timezone(&Local).naive_local();
        let t = alarm_filename_time(&alarm, FilenameTimezone::Local).unwrap();
        assert_eq!(t, expected);
    }

    #[test]
    fn alarm_filename_time_errors_on_invalid_utc_time() {
        let alarm = test_alarm("not-a-number");
        assert!(alarm_filename_time(&alarm, FilenameTimezone::Utc).is_err());
    }

    #[test]
    fn now_naive_utc_and_local_differ_by_the_process_utc_offset() {
        let utc = now_naive(FilenameTimezone::Utc);
        let local = now_naive(FilenameTimezone::Local);
        let expected_offset = Local::now().offset().local_minus_utc();
        assert_eq!((local - utc).num_seconds(), expected_offset as i64);
    }
}
