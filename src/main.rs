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
        #[arg(long, default_value_t = 30)]
        pre_roll_secs: u64,
        #[arg(long, default_value_t = 60)]
        post_roll_secs: u64,
        /// How long to keep uploaded clips on Google Drive before deleting
        /// them (0 = keep forever). Only relevant if Google Drive upload is
        /// configured (see `gdrive-login`) — ignored otherwise.
        #[arg(long, default_value_t = 30)]
        gdrive_retention_days: u32,
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
            pre_roll_secs,
            post_roll_secs,
            gdrive_retention_days,
        } => {
            watch::run(
                &client,
                Duration::from_secs(interval_secs),
                &events_file,
                &buffer_dir,
                &clips_dir,
                Duration::from_secs(pre_roll_secs),
                Duration::from_secs(post_roll_secs),
                gdrive_retention_days,
            )
            .await?;
        }
        Command::Listen {
            callback_url,
            listen_addr,
            events_file,
            clips_dir,
            buffer_dir,
            pre_roll_secs,
            post_roll_secs,
            max_push_latency_secs,
            gdrive_retention_days,
        } => {
            listen::run(
                &client,
                &callback_url,
                listen_addr,
                &events_file,
                &buffer_dir,
                &clips_dir,
                Duration::from_secs(pre_roll_secs),
                Duration::from_secs(post_roll_secs),
                Duration::from_secs(max_push_latency_secs),
                gdrive_retention_days,
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
