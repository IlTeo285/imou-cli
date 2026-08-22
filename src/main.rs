mod api;
mod client;
mod config;
mod error;
mod event_log;
mod gdrive;
mod listen;
mod motion_event;
mod mqtt;
mod recorder;
mod signing;
mod token_cache;
mod watch;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};

use api::ptz::Direction;
use client::ImouClient;
use config::Config;
use recorder::FilenameTimezone;

#[derive(Parser)]
#[command(name = "imou", about = "CLI for the Imou Open Platform")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Fetch (or reuse the cached) access token and print it.
    Token,
    /// List devices/channels bound to this account.
    Devices {
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// Check whether a device is online.
    Status {
        device_id: String,
    },
    /// Start a live stream for a device channel and print its HLS URL.
    Live {
        device_id: String,
        #[arg(long, default_value = "0")]
        channel_id: String,
        /// 0 = HD main stream, 1 = SD substream.
        #[arg(long, default_value_t = 0)]
        stream_id: u8,
    },
    /// Move (or stop) a PTZ-capable camera.
    Ptz {
        device_id: String,
        #[arg(long, default_value = "0")]
        channel_id: String,
        #[arg(value_enum)]
        direction: Direction,
        /// Move duration in milliseconds (ignored for `stop`).
        #[arg(long, default_value_t = 1000)]
        duration_ms: u64,
    },
    /// Run a foreground service that polls every bound camera for motion
    /// detection and appends a JSON line to `--events-file` for each one,
    /// until interrupted (Ctrl+C). For channels with local RTSP config
    /// (`CAM_<NAME>_IP`/`CAM_<NAME>_SECURE` in `.env`), also records a
    /// rolling local buffer and saves a `--pre-roll-secs`/`--post-roll-secs`
    /// clip per alarm into `--clips-dir`, entirely over the LAN.
    Watch {
        #[arg(long, default_value_t = 30)]
        interval_secs: u64,
        #[arg(long, default_value = "motion_events.jsonl")]
        events_file: PathBuf,
        #[arg(long, default_value = "clips")]
        clips_dir: PathBuf,
        #[arg(long, default_value = ".imou_ring_buffer")]
        buffer_dir: PathBuf,
        /// Where per-clip frame-diff snapshots are saved (see CLAUDE.md's
        /// "snapshot" section) — always on, no configuration required: it's
        /// local pixel arithmetic, not a network/credentialed integration.
        #[arg(long, default_value = "snapshots")]
        snapshots_dir: PathBuf,
        /// How many distinct snapshots to save per clip (the most visually
        /// different moments, spaced at least a couple seconds apart).
        #[arg(long, default_value_t = 1)]
        snapshot_count: u8,
        /// Where the continuous (non-motion-triggered) recording archive is
        /// saved, if enabled via `--continuous-retention-hours`. Written by
        /// the same ffmpeg process as the pre-roll ring buffer (see
        /// CLAUDE.md's "Continuous recording" section), one `.mp4` per
        /// `--continuous-segment-minutes`.
        #[arg(long, default_value = "continuous")]
        continuous_dir: PathBuf,
        /// Length of each continuous-archive segment. Shorter segments mean
        /// more files but a shorter wait before the most recent footage is
        /// reliably playable (an in-progress segment's .mp4 isn't finalized
        /// until it rotates) — see CLAUDE.md.
        #[arg(long, default_value_t = 15)]
        continuous_segment_minutes: u32,
        /// How long to keep the continuous archive (0 = feature disabled —
        /// unlike every other `--*-retention-*` flag in this CLI, `0` here
        /// does NOT mean "keep forever": continuous recording is costly
        /// enough (tens of GB/camera/day) that it needs an explicit opt-in,
        /// not just an always-on sweep. Set e.g. 48 for "last 2 days".
        #[arg(long, default_value_t = 0)]
        continuous_retention_hours: u32,
        #[arg(long, default_value_t = 30)]
        pre_roll_secs: u64,
        #[arg(long, default_value_t = 60)]
        post_roll_secs: u64,
        /// How long to keep uploaded clips on Google Drive before deleting
        /// them (0 = keep forever). Only relevant if Google Drive upload is
        /// configured (see `gdrive-login`) — ignored otherwise.
        #[arg(long, default_value_t = 30)]
        gdrive_retention_days: u32,
        /// How long to keep clips in `--clips-dir` before deleting them (0 =
        /// keep forever). Independent of `--gdrive-retention-days` — a clip
        /// uploaded to Drive is no longer deleted locally on upload success,
        /// so this is what now bounds local disk usage. Also governs
        /// `--snapshots-dir` (same policy, separate sweep).
        #[arg(long, default_value_t = 30)]
        local_retention_days: u32,
        /// Only relevant if AI analysis is configured
        /// (AI_OLLAMA_URL/AI_MODEL_NAME in the environment). When true,
        /// Google Drive upload is skipped for clips the AI classifies as
        /// not relevant (e.g. an empty scene). Default false: AI results
        /// are logged and published for visibility, but existing upload
        /// behavior is unchanged until explicitly opted into.
        #[arg(long, default_value_t = false)]
        ai_gate_gdrive_upload: bool,
        /// Only relevant if AI analysis is configured. When true, the
        /// `.../motion-analyzed` MQTT message is only published for clips
        /// the AI found relevant. Default false: always publish (with a
        /// `relevant` field) so downstream automations can filter
        /// themselves instead of risking a dropped notification.
        #[arg(long, default_value_t = false)]
        ai_gate_mqtt_analyzed: bool,
        /// Wall-clock convention for every filename this process writes
        /// (ring buffer/continuous segments, clips, snapshots) and the
        /// retention sweeps that later parse those names back — does NOT
        /// affect `local_time` in the JSON event log (always local) or the
        /// Google Drive day-folder grouping (always the real local day).
        /// `local` requires the container's own timezone to actually be
        /// set correctly (e.g. `TZ=Europe/Rome`) — see CLAUDE.md.
        #[arg(long, value_enum, default_value_t = FilenameTimezone::Local)]
        filename_timezone: FilenameTimezone,
    },
    /// Run a foreground service that registers a push callback with Imou
    /// and reacts to motion events as they're delivered — no polling.
    /// Same `--clips-dir`/local-recording behavior as `watch`. The first
    /// delivery after a fresh registration can take a long time to start
    /// (observed ~48 minutes live) even though the endpoint is reachable
    /// immediately — an Imou-side warm-up, not a bug here.
    Listen {
        /// Public HTTPS URL Imou will POST events to, ending in
        /// `/imou-callback` (fixed path). Must already be reachable when
        /// this starts — registration doesn't wait for that.
        callback_url: String,
        #[arg(long, default_value = "0.0.0.0:8787")]
        listen_addr: SocketAddr,
        #[arg(long, default_value = "motion_events.jsonl")]
        events_file: PathBuf,
        #[arg(long, default_value = "clips")]
        clips_dir: PathBuf,
        #[arg(long, default_value = ".imou_ring_buffer")]
        buffer_dir: PathBuf,
        /// Where per-clip frame-diff snapshots are saved (see CLAUDE.md's
        /// "snapshot" section) — always on, no configuration required: it's
        /// local pixel arithmetic, not a network/credentialed integration.
        #[arg(long, default_value = "snapshots")]
        snapshots_dir: PathBuf,
        /// How many distinct snapshots to save per clip (the most visually
        /// different moments, spaced at least a couple seconds apart).
        #[arg(long, default_value_t = 1)]
        snapshot_count: u8,
        /// Where the continuous (non-motion-triggered) recording archive is
        /// saved, if enabled via `--continuous-retention-hours`. Written by
        /// the same ffmpeg process as the pre-roll ring buffer (see
        /// CLAUDE.md's "Continuous recording" section), one `.mp4` per
        /// `--continuous-segment-minutes`.
        #[arg(long, default_value = "continuous")]
        continuous_dir: PathBuf,
        /// Length of each continuous-archive segment. Shorter segments mean
        /// more files but a shorter wait before the most recent footage is
        /// reliably playable (an in-progress segment's .mp4 isn't finalized
        /// until it rotates) — see CLAUDE.md.
        #[arg(long, default_value_t = 15)]
        continuous_segment_minutes: u32,
        /// How long to keep the continuous archive (0 = feature disabled —
        /// unlike every other `--*-retention-*` flag in this CLI, `0` here
        /// does NOT mean "keep forever": continuous recording is costly
        /// enough (tens of GB/camera/day) that it needs an explicit opt-in,
        /// not just an always-on sweep. Set e.g. 48 for "last 2 days".
        #[arg(long, default_value_t = 0)]
        continuous_retention_hours: u32,
        #[arg(long, default_value_t = 30)]
        pre_roll_secs: u64,
        #[arg(long, default_value_t = 60)]
        post_roll_secs: u64,
        /// Worst-case delay to assume between a real event and its push
        /// delivery, for sizing the local recording buffer's retention.
        #[arg(long, default_value_t = 300)]
        max_push_latency_secs: u64,
        /// How long to keep uploaded clips on Google Drive before deleting
        /// them (0 = keep forever). Only relevant if Google Drive upload is
        /// configured (see `gdrive-login`) — ignored otherwise.
        #[arg(long, default_value_t = 30)]
        gdrive_retention_days: u32,
        /// How long to keep clips in `--clips-dir` before deleting them (0 =
        /// keep forever). Independent of `--gdrive-retention-days` — a clip
        /// uploaded to Drive is no longer deleted locally on upload success,
        /// so this is what now bounds local disk usage. Also governs
        /// `--snapshots-dir` (same policy, separate sweep).
        #[arg(long, default_value_t = 30)]
        local_retention_days: u32,
        /// Only relevant if AI analysis is configured
        /// (AI_OLLAMA_URL/AI_MODEL_NAME in the environment). When true,
        /// Google Drive upload is skipped for clips the AI classifies as
        /// not relevant (e.g. an empty scene). Default false: AI results
        /// are logged and published for visibility, but existing upload
        /// behavior is unchanged until explicitly opted into.
        #[arg(long, default_value_t = false)]
        ai_gate_gdrive_upload: bool,
        /// Only relevant if AI analysis is configured. When true, the
        /// `.../motion-analyzed` MQTT message is only published for clips
        /// the AI found relevant. Default false: always publish (with a
        /// `relevant` field) so downstream automations can filter
        /// themselves instead of risking a dropped notification.
        #[arg(long, default_value_t = false)]
        ai_gate_mqtt_analyzed: bool,
        /// Wall-clock convention for every filename this process writes
        /// (ring buffer/continuous segments, clips, snapshots) and the
        /// retention sweeps that later parse those names back — does NOT
        /// affect `local_time` in the JSON event log (always local) or the
        /// Google Drive day-folder grouping (always the real local day).
        /// `local` requires the container's own timezone to actually be
        /// set correctly (e.g. `TZ=Europe/Rome`) — see CLAUDE.md.
        #[arg(long, value_enum, default_value_t = FilenameTimezone::Local)]
        filename_timezone: FilenameTimezone,
    },
    /// One-time OAuth setup for Google Drive clip upload (`watch`/`listen`
    /// upload automatically once this has been run — see CLAUDE.md).
    /// Requires `GDRIVE_CLIENT_ID`/`GDRIVE_CLIENT_SECRET` in `.env`.
    GdriveLogin,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    let cli = Cli::parse();
    let config = Config::from_env()?;
    let client = ImouClient::new(config);

    match cli.command {
        Command::Token => {
            let token = client.access_token().await?;
            println!("{token}");
        }
        Command::Devices { limit } => {
            let devices = api::devices::list(&client, limit).await?;
            println!("{} device(s):", devices.count);
            for d in devices.device_list {
                println!("  {} (bindId {})", d.device_id, d.bind_id);
                for ch in d.channels {
                    println!("    channel {}: {}", ch.channel_id, ch.channel_name);
                }
            }
        }
        Command::Status { device_id } => {
            let status = api::devices::online_status(&client, &device_id).await?;
            println!("{device_id}: {}", status.on_line);
            for ch in status.channels {
                println!("  channel {}: {}", ch.channel_id, ch.on_line);
            }
        }
        Command::Live {
            device_id,
            channel_id,
            stream_id,
        } => {
            let info = api::live::bind(&client, &device_id, &channel_id, stream_id).await?;
            if let Some(status) = info.live_status {
                println!("live status: {status}");
            }
            for s in info.streams {
                println!("stream {}: {}", s.stream_id, s.hls);
            }
        }
        Command::Ptz {
            device_id,
            channel_id,
            direction,
            duration_ms,
        } => {
            api::ptz::r#move(&client, &device_id, &channel_id, direction, duration_ms).await?;
            println!("ok");
        }
        Command::Watch {
            interval_secs,
            events_file,
            clips_dir,
            buffer_dir,
            snapshots_dir,
            snapshot_count,
            continuous_dir,
            continuous_segment_minutes,
            continuous_retention_hours,
            pre_roll_secs,
            post_roll_secs,
            gdrive_retention_days,
            local_retention_days,
            ai_gate_gdrive_upload,
            ai_gate_mqtt_analyzed,
            filename_timezone,
        } => {
            watch::run(
                &client,
                Duration::from_secs(interval_secs),
                &events_file,
                &buffer_dir,
                &clips_dir,
                &snapshots_dir,
                snapshot_count,
                &continuous_dir,
                continuous_segment_minutes,
                continuous_retention_hours,
                Duration::from_secs(pre_roll_secs),
                Duration::from_secs(post_roll_secs),
                gdrive_retention_days,
                local_retention_days,
                ai_gate_gdrive_upload,
                ai_gate_mqtt_analyzed,
                filename_timezone,
            )
            .await?;
        }
        Command::Listen {
            callback_url,
            listen_addr,
            events_file,
            clips_dir,
            buffer_dir,
            snapshots_dir,
            snapshot_count,
            continuous_dir,
            continuous_segment_minutes,
            continuous_retention_hours,
            pre_roll_secs,
            post_roll_secs,
            max_push_latency_secs,
            gdrive_retention_days,
            local_retention_days,
            ai_gate_gdrive_upload,
            ai_gate_mqtt_analyzed,
            filename_timezone,
        } => {
            listen::run(
                &client,
                &callback_url,
                listen_addr,
                &events_file,
                &buffer_dir,
                &clips_dir,
                &snapshots_dir,
                snapshot_count,
                &continuous_dir,
                continuous_segment_minutes,
                continuous_retention_hours,
                Duration::from_secs(pre_roll_secs),
                Duration::from_secs(post_roll_secs),
                Duration::from_secs(max_push_latency_secs),
                gdrive_retention_days,
                local_retention_days,
                ai_gate_gdrive_upload,
                ai_gate_mqtt_analyzed,
                filename_timezone,
            )
            .await?;
        }
        Command::GdriveLogin => {
            let cfg = gdrive::config_from_env().ok_or_else(|| {
                anyhow::anyhow!(
                    "GDRIVE_CLIENT_ID / GDRIVE_CLIENT_SECRET must be set in .env before running gdrive-login"
                )
            })?;
            gdrive::run_login(&cfg).await?;
        }
    }

    Ok(())
}
