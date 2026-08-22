use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;
use uuid::Uuid;

use crate::error::{Result, VisionError};
use crate::frames::extract_frame_at;

const DEFAULT_MAX_COUNT: u8 = 1;
const DEFAULT_MIN_GAP_SECS: f64 = 2.0;
/// Compare every 4th decoded frame instead of every consecutive one —
/// verified live against a real clip: cuts analysis time from ~31s to
/// ~8s on a real ~97s clip while still cleanly finding the same real
/// motion peak. Motion worth flagging spans many frames, so skipping most
/// of them barely affects detection but
/// meaningfully cuts CPU time — the whole reason this feature exists is to
/// replace a VLM approach that pegged a weak CPU for minutes per clip.
const DEFAULT_DECIMATE: u32 = 4;

/// Tuning for `extract_snapshots`. No threshold field: unlike ffmpeg's
/// `scene` cut-detection filter (tried first, rejected — see below),
/// `signalstats` YAVG on a frame-difference image is a continuous motion
/// magnitude with no natural "is this real" cutoff to filter on. Every
/// analyzed clip has *some* frame with the highest score; that frame is
/// always taken, regardless of the score's absolute size.
pub struct SnapshotConfig {
    /// How many distinct snapshots to save per clip.
    pub max_count: u8,
    /// Minimum spacing (seconds) enforced between selected snapshots when
    /// `max_count > 1` — avoids picking several near-duplicate frames from
    /// the same brief moment.
    pub min_gap_secs: f64,
    /// Compare every Nth frame instead of every consecutive frame.
    pub decimate: u32,
}

impl Default for SnapshotConfig {
    fn default() -> Self {
        Self {
            max_count: DEFAULT_MAX_COUNT,
            min_gap_secs: DEFAULT_MIN_GAP_SECS,
            decimate: DEFAULT_DECIMATE,
        }
    }
}

/// Parses ffmpeg's `metadata=print:file=...` output from a
/// `tblend=all_mode=difference,signalstats` filter chain into
/// `(pts_time, YAVG)` pairs. `YAVG` here is the average luma of the
/// per-pixel `|frame_n - frame_n-1|` difference image — a direct motion
/// magnitude (0 = identical consecutive frames), not a heuristic score.
/// Pure function, no I/O — testable against a captured real sample.
///
/// Real format, one block per analyzed frame (captured live, see
/// `crates/imou-vision`'s tests — this is NOT the doc-example format,
/// it's what ffmpeg 5.1.9 actually printed):
/// ```text
/// frame:0    pts:14400   pts_time:0.16
/// lavfi.signalstats.YMIN=0
/// lavfi.signalstats.YLOW=0
/// lavfi.signalstats.YAVG=0.813486
/// ...
/// ```
fn parse_diff_scores(text: &str) -> Vec<(f64, f64)> {
    let mut scores = Vec::new();
    let mut current_pts: Option<f64> = None;

    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("frame:") {
            current_pts = rest
                .split("pts_time:")
                .nth(1)
                .and_then(|s| s.split_whitespace().next())
                .and_then(|s| s.parse::<f64>().ok());
        } else if let Some(rest) = line.strip_prefix("lavfi.signalstats.YAVG=")
            && let (Some(pts), Ok(yavg)) = (current_pts, rest.parse::<f64>())
        {
            scores.push((pts, yavg));
        }
    }

    scores
}

/// Greedily selects up to `max_count` timestamps by descending score,
/// skipping any candidate within `min_gap_secs` of an already-selected
/// one. Returns picks in chronological order. Pure function, no I/O.
fn select_top_timestamps(mut candidates: Vec<(f64, f64)>, max_count: u8, min_gap_secs: f64) -> Vec<f64> {
    candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let mut picked: Vec<f64> = Vec::new();
    for (ts, _score) in candidates {
        if picked.len() >= max_count as usize {
            break;
        }
        if picked.iter().all(|p| (p - ts).abs() >= min_gap_secs) {
            picked.push(ts);
        }
    }

    picked.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    picked
}

/// Finds the `max_count` most visually different moment(s) in `clip_path`
/// (consecutive-frame pixel difference, not a semantic model — see
/// `SnapshotConfig`'s doc note on why there's no relevance threshold) and
/// extracts each as a JPEG into `out_dir` (created if missing), named
/// `<file_prefix>_<n>.jpg` — naming is the caller's responsibility (e.g. an
/// alarm-id-based scheme), this crate has no opinion on it. Returns the
/// written paths in chronological order.
///
/// Rejected first attempt, worth remembering: ffmpeg's `select=gt(scene,X)`
/// (a hard scene-CUT detector, built for edited video) was tried first and
/// verified live to be unusable here — on one real clip it never exceeded
/// its own noise floor (~0.06-0.08, never a real spike), and on another
/// real clip it produced a scene score of exactly 0 for the ENTIRE clip
/// despite real motion partway through. The `tblend=difference` +
/// `signalstats` YAVG approach used here is a literal per-pixel motion
/// magnitude instead of a cut heuristic, and correctly picked out that
/// same moment on the second clip (max score ~3x the surrounding
/// baseline) — confirmed by eye against the extracted JPEG.
pub async fn extract_snapshots(
    clip_path: &Path,
    out_dir: &Path,
    file_prefix: &str,
    config: &SnapshotConfig,
) -> Result<Vec<PathBuf>> {
    let scores_file = std::env::temp_dir().join(format!("imou-snapshot-{}.txt", Uuid::new_v4()));

    let vf = format!(
        "select='not(mod(n\\,{}))',tblend=all_mode=difference,signalstats,metadata=print:file={}",
        config.decimate.max(1),
        scores_file.display()
    );

    let result = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "warning", "-i"])
        .arg(clip_path)
        .args(["-vf", &vf, "-f", "null", "-"])
        .stdin(Stdio::null())
        .output()
        .await;

    let text = tokio::fs::read_to_string(&scores_file).await.unwrap_or_default();
    let _ = tokio::fs::remove_file(&scores_file).await;

    let output = result?;
    if !output.status.success() {
        return Err(VisionError::Ffmpeg(format!(
            "ffmpeg frame-diff analysis failed for {}: {}",
            clip_path.display(),
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let candidates = parse_diff_scores(&text);
    if candidates.is_empty() {
        return Err(VisionError::NoFrames(format!(
            "no frame-diff scores produced for {}",
            clip_path.display()
        )));
    }

    let timestamps = select_top_timestamps(candidates, config.max_count.max(1), config.min_gap_secs);

    tokio::fs::create_dir_all(out_dir).await?;
    let mut written = Vec::with_capacity(timestamps.len());
    for (i, ts) in timestamps.iter().enumerate() {
        let out_path = out_dir.join(format!("{file_prefix}_{i}.jpg"));
        extract_frame_at(clip_path, &out_path, *ts).await?;
        written.push(out_path);
    }

    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured live from `ffmpeg -i <real clip> -vf
    // "select='not(mod(n\,4))',tblend=all_mode=difference,signalstats,metadata=print:file=..."`
    // against a real ~97s clip with real motion partway through — trimmed
    // to the interesting slice (a flat stretch, then the real peak) for a
    // compact fixture.
    const REAL_SAMPLE: &str = "\
frame:799  pts:2896830 pts_time:32.187
lavfi.signalstats.YMIN=0
lavfi.signalstats.YAVG=9.51703
lavfi.signalstats.YMAX=210
frame:899  pts:3260070 pts_time:36.223
lavfi.signalstats.YMIN=0
lavfi.signalstats.YAVG=7.65682
lavfi.signalstats.YMAX=198
frame:999  pts:3620160 pts_time:40.224
lavfi.signalstats.YMIN=0
lavfi.signalstats.YAVG=6.36619
lavfi.signalstats.YMAX=180
";

    #[test]
    fn parses_real_signalstats_output() {
        let scores = parse_diff_scores(REAL_SAMPLE);
        assert_eq!(scores, vec![(32.187, 9.51703), (36.223, 7.65682), (40.224, 6.36619)]);
    }

    #[test]
    fn ignores_unrelated_lines() {
        let scores = parse_diff_scores("not a frame line\nlavfi.signalstats.YAVG=1.0\n");
        assert!(scores.is_empty(), "YAVG with no preceding frame/pts_time must be dropped");
    }

    #[test]
    fn empty_input_yields_no_scores() {
        assert!(parse_diff_scores("").is_empty());
    }

    #[test]
    fn selects_single_highest_score() {
        let picks = select_top_timestamps(vec![(1.0, 0.5), (2.0, 9.0), (3.0, 3.0)], 1, 2.0);
        assert_eq!(picks, vec![2.0]);
    }

    #[test]
    fn selects_top_n_respecting_min_gap() {
        // 2.0 and 2.1 are both high but too close together (< min_gap) —
        // only the higher of the two should survive alongside the next
        // best candidate far enough away.
        let picks = select_top_timestamps(vec![(2.0, 9.0), (2.1, 8.9), (10.0, 5.0)], 2, 2.0);
        assert_eq!(picks, vec![2.0, 10.0]);
    }

    #[test]
    fn max_count_larger_than_candidates_returns_all() {
        let picks = select_top_timestamps(vec![(1.0, 1.0), (5.0, 2.0)], 5, 2.0);
        assert_eq!(picks, vec![1.0, 5.0]);
    }
}
