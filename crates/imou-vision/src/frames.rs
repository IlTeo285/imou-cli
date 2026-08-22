use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

use crate::error::{Result, VisionError};

/// Don't sample within this many seconds of either end of the clip — the
/// pre/post-roll margins are often near-empty (motion is usually mid-clip),
/// and ffmpeg seeking exactly at duration can fail on some containers.
const EDGE_MARGIN_SECS: f64 = 1.0;

/// Picks `count` timestamps (seconds from clip start) evenly spaced across
/// `[EDGE_MARGIN_SECS, duration - EDGE_MARGIN_SECS]`. Falls back to the
/// clip's midpoint for very short clips where that range is empty or
/// inverted. Pure function — no I/O, so it's testable without ffmpeg.
fn pick_timestamps(duration: f64, count: u8) -> Vec<f64> {
    let count = count.max(1) as usize;
    let start = EDGE_MARGIN_SECS;
    let end = duration - EDGE_MARGIN_SECS;

    if end <= start {
        return vec![(duration / 2.0).max(0.0)];
    }
    if count == 1 {
        return vec![(start + end) / 2.0];
    }

    let span = end - start;
    (0..count)
        .map(|i| start + span * (i as f64) / ((count - 1) as f64))
        .collect()
}

async fn probe_duration_secs(clip_path: &Path) -> Result<f64> {
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
        .arg(clip_path)
        .stdin(Stdio::null())
        .output()
        .await?;

    if !output.status.success() {
        return Err(VisionError::Ffmpeg(format!(
            "ffprobe failed for {}: {}",
            clip_path.display(),
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<f64>()
        .map_err(|e| {
            VisionError::Ffmpeg(format!(
                "could not parse ffprobe duration for {}: {e}",
                clip_path.display()
            ))
        })
}

/// Extracts a single JPEG frame from `clip_path` at `timestamp` (seconds)
/// into `out_path` via `ffmpeg -ss <t> -frames:v 1`. Shared by
/// `extract_frames` (evenly-spaced sampling, for the Ollama VLM path) and
/// `snapshot::extract_snapshots` (score-selected sampling, for the
/// frame-diff path) — same single-frame extraction either way, only how
/// the timestamp is chosen differs.
pub(crate) async fn extract_frame_at(clip_path: &Path, out_path: &Path, timestamp: f64) -> Result<()> {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "warning", "-y", "-ss"])
        .arg(format!("{timestamp:.3}"))
        .arg("-i")
        .arg(clip_path)
        .args(["-frames:v", "1", "-q:v", "3"])
        .arg(out_path)
        .stdin(Stdio::null())
        .output()
        .await?;

    if !output.status.success() {
        return Err(VisionError::Ffmpeg(format!(
            "ffmpeg frame extraction failed at t={timestamp:.3}s for {}: {}",
            clip_path.display(),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

/// Extracts `frame_count` representative JPEG frames from `clip_path` into
/// `out_dir` (created if missing), evenly spaced per `pick_timestamps`.
/// Returns the written frame paths in timestamp order.
pub async fn extract_frames(clip_path: &Path, out_dir: &Path, frame_count: u8) -> Result<Vec<PathBuf>> {
    let duration = probe_duration_secs(clip_path).await?;
    let timestamps = pick_timestamps(duration, frame_count);

    tokio::fs::create_dir_all(out_dir).await?;

    let mut frames = Vec::with_capacity(timestamps.len());
    for (i, ts) in timestamps.iter().enumerate() {
        let out_path = out_dir.join(format!("frame_{i}.jpg"));
        if let Err(e) = extract_frame_at(clip_path, &out_path, *ts).await {
            eprintln!("warning: {e}");
            continue;
        }
        frames.push(out_path);
    }

    if frames.is_empty() {
        return Err(VisionError::NoFrames(format!(
            "no frames could be extracted from {}",
            clip_path.display()
        )));
    }

    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evenly_spaces_multiple_timestamps() {
        let ts = pick_timestamps(11.0, 3);
        assert_eq!(ts.len(), 3);
        assert_eq!(ts[0], EDGE_MARGIN_SECS);
        assert_eq!(ts[2], 10.0);
        assert!((ts[1] - 5.5).abs() < 1e-9);
    }

    #[test]
    fn single_timestamp_is_midpoint_of_the_trimmed_range() {
        let ts = pick_timestamps(11.0, 1);
        assert_eq!(ts, vec![5.5]);
    }

    #[test]
    fn very_short_clip_falls_back_to_overall_midpoint() {
        let ts = pick_timestamps(1.0, 3);
        assert_eq!(ts, vec![0.5]);
    }

    #[test]
    fn count_is_never_treated_as_zero() {
        let ts = pick_timestamps(11.0, 0);
        assert_eq!(ts.len(), 1);
    }
}
