use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

use chrono::{NaiveDateTime, Utc};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::error::{ImouError, Result};
use crate::recorder::{self, FilenameTimezone};

/// Which corner each configured channel occupies (top-left, top-right,
/// bottom-left, bottom-right, in that fixed order) plus the composed
/// output's per-tile size/frame rate — see `resolve_grid_order` for how
/// `order` is decided and `build_grid_args` for how it's laid out.
pub struct GridConfig {
    pub tile_size: (u32, u32),
    pub fps: u32,
    pub order: [Option<String>; 4],
}

/// Resolves the 4 grid slots (TL,TR,BL,BR) from, in priority order: the
/// `--grid-order` flag, the `GRID_ORDER` env var, or — if neither is set —
/// an alphabetically-sorted `recorded_channels` as a deterministic
/// fallback. Deliberately never iterates `recorded_channels` (a
/// `HashSet<String>`) directly for ordering, since that has no stable
/// order across runs. A configured name absent from `recorded_channels`
/// becomes a permanent black tile (warned once, not an error) — same
/// fail-open convention as `local_config_for` returning `None`. More than
/// 4 names is a hard startup error (ambiguous, not silently truncated).
pub fn resolve_grid_order(
    flag: Option<&str>,
    env: Option<&str>,
    recorded_channels: &HashSet<String>,
) -> Result<[Option<String>; 4]> {
    let raw = flag.or(env);
    let names: Vec<String> = match raw {
        Some(s) => s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect(),
        None => {
            let mut sorted: Vec<String> = recorded_channels.iter().cloned().collect();
            sorted.sort();
            sorted.truncate(4);
            sorted
        }
    };

    if names.len() > 4 {
        return Err(ImouError::Config(format!(
            "--grid-order/GRID_ORDER names {} channels, but the grid layout only has 4 slots",
            names.len()
        )));
    }

    let mut order: [Option<String>; 4] = [None, None, None, None];
    for (i, name) in names.into_iter().enumerate() {
        if recorded_channels.contains(&name) {
            order[i] = Some(name);
        } else {
            eprintln!(
                "warning: grid slot {i} names '{name}', which has no local RTSP config — that tile will stay black"
            );
        }
    }
    Ok(order)
}

/// Rounds `ts` down to the nearest `segment_minutes` wall-clock boundary —
/// matches where `-segment_atclocktime 1` cuts a continuous-archive
/// segment, so segments written by independently-started/-restarted camera
/// processes that belong to the "same" window bucket to the same instant.
pub fn bucket_window(ts: NaiveDateTime, segment_minutes: u32) -> NaiveDateTime {
    let step = segment_minutes as i64 * 60;
    let epoch = ts.and_utc().timestamp();
    let bucketed = epoch.div_euclid(step) * step;
    chrono::DateTime::<Utc>::from_timestamp(bucketed, 0).expect("bucketed timestamp in range").naive_utc()
}

/// A staging segment is trustworthy input only once it's provably
/// finalized: either a newer sibling already exists for that camera
/// (proving ffmpeg rotated past it), or enough wall-clock time has elapsed
/// since its nominal start that it must have rotated regardless — the
/// latter guards a camera that dies right after writing its last segment,
/// which would otherwise never satisfy the first condition and strand that
/// segment forever.
pub fn is_completed(
    seg_ts: NaiveDateTime,
    has_newer_sibling: bool,
    segment_secs: u32,
    now: NaiveDateTime,
    margin: chrono::Duration,
) -> bool {
    has_newer_sibling || seg_ts + chrono::Duration::seconds(segment_secs as i64) + margin < now
}

/// For each of the 4 grid slots, finds the (already-filtered-to-completed)
/// staging segment whose start time is closest to `window`, within
/// `tolerance` — absorbing the few seconds of keyframe-boundary jitter
/// `-segment_atclocktime` leaves between independently-started camera
/// processes. A slot with no match in range becomes `None`, composed as a
/// black placeholder rather than failing the whole window.
pub fn match_window(
    per_slot_segments: &[Vec<(NaiveDateTime, PathBuf)>; 4],
    window: NaiveDateTime,
    tolerance: chrono::Duration,
) -> [Option<PathBuf>; 4] {
    std::array::from_fn(|i| {
        per_slot_segments[i]
            .iter()
            .filter(|(ts, _)| (*ts - window).num_seconds().abs() <= tolerance.num_seconds())
            .min_by_key(|(ts, _)| (*ts - window).num_seconds().abs())
            .map(|(_, path)| path.clone())
    })
}

/// Picks the next window for the composer to consume: normally
/// `last_consumed + segment_minutes`, but if the composer fell behind by
/// more than one window (a slow compose, a restart), jumps straight to the
/// newest fully-elapsed boundary instead of replaying every missed one —
/// the returned window may not be ready yet (the caller must still check
/// against `now` and wait).
pub fn next_window(last_consumed: Option<NaiveDateTime>, now: NaiveDateTime, segment_minutes: u32) -> NaiveDateTime {
    let step = chrono::Duration::minutes(segment_minutes as i64);
    let newest_ready = bucket_window(now - step - alignment_tolerance(segment_minutes * 60), segment_minutes);
    match last_consumed {
        None => newest_ready,
        Some(last) => {
            let candidate = last + step;
            if candidate < newest_ready {
                newest_ready
            } else {
                candidate
            }
        }
    }
}

/// Fails fast at startup if the ffmpeg on `PATH` has no `xstack` filter
/// (needs ffmpeg 4.1+) — without this, `--continuous-mode grid` would only
/// fail much later, per-window, inside `start_grid_composer`'s detached
/// task, with a much less obvious error. Verified live: Debian bookworm's
/// packaged ffmpeg (the base image `Dockerfile` installs from `apt`) does
/// carry `xstack` — see CLAUDE.md's "Grid continuous recording" section.
pub async fn check_xstack_available() -> Result<()> {
    let output = tokio::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-filters"])
        .output()
        .await
        .map_err(|e| ImouError::Config(format!("cannot run ffmpeg to check filter availability: {e}")))?;
    if String::from_utf8_lossy(&output.stdout).contains("xstack") {
        Ok(())
    } else {
        Err(ImouError::Config(
            "ffmpeg build on PATH has no 'xstack' filter (needs ffmpeg 4.1+) — required for --continuous-mode grid"
                .into(),
        ))
    }
}

/// `xstack`'s tiles are scaled to exactly `(w, h)` with no `-2`-style
/// auto-adjustment, and `libx264`'s default `yuv420p` output requires even
/// width/height — an odd `--grid-tile-size` would otherwise fail loudly
/// but only once ffmpeg actually runs for the first window, not at
/// startup. Pure function, no I/O.
pub fn validate_tile_size(w: u32, h: u32) -> Result<()> {
    if w == 0 || h == 0 || !w.is_multiple_of(2) || !h.is_multiple_of(2) {
        return Err(ImouError::Config(format!(
            "--grid-tile-size {w}x{h} is invalid — both dimensions must be even and non-zero (libx264's default yuv420p output requires it)"
        )));
    }
    Ok(())
}

/// Real cameras' `-segment_atclocktime` cut point is bounded by keyframe
/// availability, not wall-clock precision — **verified live, twice, at two
/// different scales**. First, against a quick 60s-segment test with 4 real
/// cameras: drift up to ~29s past the nominal boundary. Then, in actual
/// production with the real default `--continuous-segment-minutes 15`
/// (900s): drift up to **5m43s (343s)** past the nominal boundary,
/// *consistently*, not as one-off jitter — every window came back with
/// zero matching input on all four slots despite gigabytes of staging
/// segments actively accumulating. The first test's drift (~29s, ~48% of
/// that 60s segment) generalizes as a **proportion of segment length**,
/// not a fixed number of seconds — a camera's keyframe interval doesn't
/// shrink just because `segment_minutes` is configured longer, so a fixed
/// small cap (an earlier version of this function capped at 60s
/// regardless of segment length) silently reproduces the exact same
/// "zero matches ever" failure at the real 15-minute default, which is
/// exactly what happened live before this was corrected. `recorder::
/// FLUSH_MARGIN` (5s) — calibrated against a synthetic, frequent-keyframe
/// source — was never in the right ballpark for either scale. Used both
/// as extra wait time before a window is considered ready and as
/// `match_window`'s per-slot tolerance. Floored at 5s only to avoid a
/// degenerate zero tolerance for an absurdly small `segment_minutes`.
fn alignment_tolerance(segment_secs: u32) -> chrono::Duration {
    let half = chrono::Duration::seconds(segment_secs as i64) / 2;
    let floor = chrono::Duration::seconds(5);
    if half > floor {
        half
    } else {
        floor
    }
}

/// How long an unconsumed (or failed-to-compose) staging segment is kept
/// before being reaped — roughly two full windows of slack beyond the
/// composer's own tick cadence, bounding staging growth independently of
/// the user's (potentially much larger) `--continuous-retention-hours`,
/// and doubling as the cleanup path for a camera later removed from
/// `--grid-order`/config.
pub fn staging_ttl(segment_minutes: u32) -> StdDuration {
    StdDuration::from_secs(segment_minutes as u64 * 60 * 3)
}

/// Builds the `ffmpeg` argument vector composing up to 4 real inputs into
/// one 2x2 `xstack` grid — a missing slot becomes an infinite black
/// `lavfi` source. **Never truncates to the shortest available input**
/// (an earlier version did, via `xstack=...:shortest=1` — changed after
/// live production use showed a single flaky camera's short segment
/// cutting the *entire* grid video short, hiding perfectly good footage
/// from the other three cameras for that window). Instead, every real
/// input is padded with solid black (`tpad=stop_mode=add`) for a full
/// `segment_secs` past wherever its own real content ends, and the
/// output is then hard-capped to exactly `segment_secs` with `-t` — so a
/// camera that's missing entirely (`None` slot) or whose matched segment
/// ran short just shows black for whatever portion of the window it
/// didn't cover, while every other slot still plays in full. Pure
/// function — see the tests below for the exact shape this needs to be
/// verified live against (ffmpeg version, `xstack`/`tpad` availability,
/// real throughput).
pub fn build_grid_args(
    inputs: &[Option<PathBuf>; 4],
    tile_w: u32,
    tile_h: u32,
    fps: u32,
    segment_secs: u32,
    out_path: &Path,
) -> Vec<String> {
    let mut args: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "warning".into(), "-y".into()];

    for input in inputs {
        match input {
            Some(path) => {
                args.push("-i".into());
                args.push(path.to_string_lossy().into_owned());
            }
            None => {
                args.push("-f".into());
                args.push("lavfi".into());
                args.push("-i".into());
                args.push(format!("color=c=black:s={tile_w}x{tile_h}:r={fps}"));
            }
        }
    }

    let mut filter = String::new();
    for i in 0..4 {
        filter.push_str(&format!(
            "[{i}:v]scale={tile_w}:{tile_h},setsar=1,fps={fps},tpad=stop_mode=add:stop_duration={segment_secs}:color=black[v{i}];"
        ));
    }
    filter.push_str("[v0][v1][v2][v3]xstack=inputs=4:layout=0_0|w0_0|0_h0|w0_h0:fill=black[out]");

    args.push("-filter_complex".into());
    args.push(filter);
    args.push("-map".into());
    args.push("[out]".into());
    args.push("-t".into());
    args.push(segment_secs.to_string());
    args.push("-an".into());
    args.push("-c:v".into());
    args.push("libx264".into());
    args.push("-preset".into());
    args.push("ultrafast".into());
    args.push("-crf".into());
    args.push("28".into());
    args.push(out_path.to_string_lossy().into_owned());

    args
}

/// Lists `<dir>/*.mp4` staging segments already provably `is_completed`
/// (see that function), sorted oldest-first so `has_newer_sibling` can be
/// derived from list position. A missing/unreadable directory (camera
/// never configured, or not recorded yet) yields an empty list rather than
/// an error — same fail-open posture as everything else in this pipeline.
async fn list_completed_segments(dir: &Path, segment_secs: u32, now: NaiveDateTime) -> Vec<(NaiveDateTime, PathBuf)> {
    let mut out = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return out;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("mp4") {
            continue;
        }
        if let Some(ts) = recorder::parse_segment_time(&path) {
            out.push((ts, path));
        }
    }
    out.sort_by_key(|(ts, _)| *ts);

    let len = out.len();
    out.into_iter()
        .enumerate()
        .filter(|(i, (ts, _))| is_completed(*ts, *i + 1 < len, segment_secs, now, alignment_tolerance(segment_secs)))
        .map(|(_, seg)| seg)
        .collect()
}

/// Sweeps every per-camera subdirectory under `staging_root` for segments
/// older than `ttl` — including subdirectories for a camera no longer in
/// the current `--grid-order`/config, which otherwise have no other path
/// to ever being cleaned up.
async fn sweep_staging(staging_root: &Path, ttl: StdDuration, tz: FilenameTimezone) -> Result<()> {
    let mut entries = match tokio::fs::read_dir(staging_root).await {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_dir() {
            recorder::cleanup_old_segments(&entry.path(), "mp4", ttl, tz).await?;
        }
    }
    Ok(())
}

/// Spawns the periodic grid compositor: once per group (not per camera),
/// ticks on `segment_minutes` wall-clock boundaries, composes each
/// fully-elapsed window's available camera footage into one grid `.mp4`
/// under `out_dir`, and deletes the consumed staging inputs on success.
/// Stops when `shutdown_rx` reports `true` — reuses the same
/// `tokio::sync::watch` shutdown convention as every other task
/// `recorder::start_all` spawns, so `shutdown_all` needs no changes to
/// also wait for this one.
pub fn start_grid_composer(
    staging_root: PathBuf,
    out_dir: PathBuf,
    segment_minutes: u32,
    grid: GridConfig,
    filename_tz: FilenameTimezone,
    mut shutdown_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = tokio::fs::create_dir_all(&out_dir).await {
            eprintln!("error: cannot create grid output dir {}: {e}", out_dir.display());
            return;
        }

        let ttl = staging_ttl(segment_minutes);
        let segment_secs = segment_minutes * 60;
        let mut last_consumed: Option<NaiveDateTime> = None;

        loop {
            if *shutdown_rx.borrow() {
                return;
            }

            if let Err(e) = sweep_staging(&staging_root, ttl, filename_tz).await {
                eprintln!("warning: grid staging sweep failed: {e}");
            }

            let now = recorder::now_naive(filename_tz);
            let window = next_window(last_consumed, now, segment_minutes);
            let ready_at =
                window + chrono::Duration::minutes(segment_minutes as i64) + alignment_tolerance(segment_secs);

            if ready_at > now {
                let Ok(d) = (ready_at - now).to_std() else {
                    // Negative/zero duration from a clock hiccup — just
                    // loop back around rather than erroring out.
                    continue;
                };
                tokio::select! {
                    _ = tokio::time::sleep(d) => {}
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            return;
                        }
                    }
                }
                continue; // recompute `now`/`window` fresh after waking
            }

            let mut per_slot: [Vec<(NaiveDateTime, PathBuf)>; 4] = Default::default();
            for (i, slot) in grid.order.iter().enumerate() {
                if let Some(name) = slot {
                    let dir = staging_root.join(name);
                    per_slot[i] = list_completed_segments(&dir, segment_secs, now).await;
                }
            }

            let inputs = match_window(&per_slot, window, alignment_tolerance(segment_secs));

            if inputs.iter().all(Option::is_none) {
                eprintln!("warning: grid window {window} has no available camera footage from any configured slot, skipping");
                last_consumed = Some(window);
                continue;
            }

            let out_path = out_dir.join(format!("seg_{}.mp4", window.format("%Y%m%dT%H%M%S")));
            let args = build_grid_args(&inputs, grid.tile_size.0, grid.tile_size.1, grid.fps, segment_secs, &out_path);

            match tokio::process::Command::new("ffmpeg").args(&args).output().await {
                Ok(o) if o.status.success() => {
                    println!("grid segment saved: {}", out_path.display());
                    for input in inputs.iter().flatten() {
                        let _ = tokio::fs::remove_file(input).await;
                    }
                }
                Ok(o) => {
                    eprintln!(
                        "warning: grid compose failed for window {window}, leaving staging inputs for the TTL sweep: {}",
                        String::from_utf8_lossy(&o.stderr)
                    );
                }
                Err(e) => eprintln!("warning: failed to spawn ffmpeg for grid window {window}: {e}"),
            }

            last_consumed = Some(window);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap()
    }

    fn channels(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn resolve_grid_order_prefers_flag_over_env_over_default() {
        let recorded = channels(&["a", "b", "c", "d"]);
        let order = resolve_grid_order(Some("a,b,c,d"), Some("d,c,b,a"), &recorded).unwrap();
        assert_eq!(order, [Some("a".into()), Some("b".into()), Some("c".into()), Some("d".into())]);
    }

    #[test]
    fn resolve_grid_order_falls_back_to_env_when_flag_absent() {
        let recorded = channels(&["a", "b", "c", "d"]);
        let order = resolve_grid_order(None, Some("d,c,b,a"), &recorded).unwrap();
        assert_eq!(order, [Some("d".into()), Some("c".into()), Some("b".into()), Some("a".into())]);
    }

    #[test]
    fn resolve_grid_order_falls_back_to_alphabetical_default() {
        let recorded = channels(&["backyard", "ingresso", "shed", "garage"]);
        let order = resolve_grid_order(None, None, &recorded).unwrap();
        assert_eq!(
            order,
            [Some("backyard".into()), Some("garage".into()), Some("ingresso".into()), Some("shed".into())]
        );
    }

    #[test]
    fn resolve_grid_order_pads_fewer_than_four_with_none() {
        let recorded = channels(&["a", "b"]);
        let order = resolve_grid_order(Some("a,b"), None, &recorded).unwrap();
        assert_eq!(order, [Some("a".into()), Some("b".into()), None, None]);
    }

    #[test]
    fn resolve_grid_order_treats_unconfigured_name_as_black_tile() {
        let recorded = channels(&["a"]);
        let order = resolve_grid_order(Some("a,ghost"), None, &recorded).unwrap();
        assert_eq!(order, [Some("a".into()), None, None, None]);
    }

    #[test]
    fn resolve_grid_order_errors_on_more_than_four_names() {
        let recorded = channels(&["a", "b", "c", "d", "e"]);
        assert!(resolve_grid_order(Some("a,b,c,d,e"), None, &recorded).is_err());
    }

    #[test]
    fn bucket_window_rounds_down_to_boundary() {
        assert_eq!(bucket_window(dt("2026-08-28 14:07:33"), 15), dt("2026-08-28 14:00:00"));
        assert_eq!(bucket_window(dt("2026-08-28 14:15:00"), 15), dt("2026-08-28 14:15:00"));
        assert_eq!(bucket_window(dt("2026-08-28 14:29:59"), 15), dt("2026-08-28 14:15:00"));
    }

    #[test]
    fn is_completed_true_when_newer_sibling_exists() {
        assert!(is_completed(dt("2026-08-28 14:00:00"), true, 900, dt("2026-08-28 14:00:01"), chrono::Duration::seconds(5)));
    }

    #[test]
    fn is_completed_true_when_enough_time_elapsed_without_sibling() {
        assert!(is_completed(dt("2026-08-28 14:00:00"), false, 900, dt("2026-08-28 14:15:06"), chrono::Duration::seconds(5)));
    }

    #[test]
    fn is_completed_false_when_neither_condition_holds() {
        assert!(!is_completed(dt("2026-08-28 14:00:00"), false, 900, dt("2026-08-28 14:10:00"), chrono::Duration::seconds(5)));
    }

    #[test]
    fn match_window_picks_nearest_within_tolerance_per_slot() {
        let window = dt("2026-08-28 14:00:00");
        let tolerance = chrono::Duration::seconds(5);
        let segments: [Vec<(NaiveDateTime, PathBuf)>; 4] = [
            vec![(dt("2026-08-28 14:00:02"), PathBuf::from("/a/seg1.mp4"))],
            vec![],
            vec![(dt("2026-08-28 13:59:59"), PathBuf::from("/c/seg1.mp4")), (dt("2026-08-28 14:20:00"), PathBuf::from("/c/seg2.mp4"))],
            vec![(dt("2026-08-28 14:00:30"), PathBuf::from("/d/seg1.mp4"))], // outside tolerance
        ];
        let matched = match_window(&segments, window, tolerance);
        assert_eq!(matched[0], Some(PathBuf::from("/a/seg1.mp4")));
        assert_eq!(matched[1], None);
        assert_eq!(matched[2], Some(PathBuf::from("/c/seg1.mp4")));
        assert_eq!(matched[3], None);
    }

    #[test]
    fn next_window_starts_at_newest_fully_elapsed_boundary_when_no_history() {
        // segment_minutes=15 -> alignment_tolerance is 450s (7m30s), so
        // "fully elapsed" now reaches back further than the old fixed-60s
        // margin did — see `alignment_tolerance`'s doc comment for why.
        let now = dt("2026-08-28 14:20:00");
        assert_eq!(next_window(None, now, 15), dt("2026-08-28 13:45:00"));
    }

    #[test]
    fn next_window_advances_by_one_step_normally() {
        let now = dt("2026-08-28 14:20:00");
        let last = dt("2026-08-28 14:00:00");
        assert_eq!(next_window(Some(last), now, 15), dt("2026-08-28 14:15:00"));
    }

    #[test]
    fn next_window_jumps_to_newest_when_backlogged() {
        // Fell behind by several windows: last consumed 3 windows ago.
        let now = dt("2026-08-28 15:20:00");
        let last = dt("2026-08-28 14:00:00");
        assert_eq!(next_window(Some(last), now, 15), dt("2026-08-28 14:45:00"));
    }

    #[test]
    fn alignment_tolerance_is_half_the_segment_for_the_real_15min_default() {
        // Verified live: real drift observed was 343s (5m43s) on a 900s
        // segment — comfortably under this 450s tolerance, whereas the
        // previous fixed-60s-cap version of this function would have
        // rejected it entirely, reproducing the exact production failure
        // this test now guards against regressing to.
        assert_eq!(alignment_tolerance(900), chrono::Duration::seconds(450));
    }

    #[test]
    fn alignment_tolerance_scales_down_for_short_test_segments() {
        assert_eq!(alignment_tolerance(60), chrono::Duration::seconds(30));
    }

    #[test]
    fn alignment_tolerance_has_a_5s_floor() {
        assert_eq!(alignment_tolerance(4), chrono::Duration::seconds(5));
    }

    #[test]
    fn validate_tile_size_accepts_even_dimensions() {
        assert!(validate_tile_size(960, 540).is_ok());
    }

    #[test]
    fn validate_tile_size_rejects_odd_dimensions() {
        assert!(validate_tile_size(961, 540).is_err());
        assert!(validate_tile_size(960, 541).is_err());
    }

    #[test]
    fn validate_tile_size_rejects_zero() {
        assert!(validate_tile_size(0, 540).is_err());
        assert!(validate_tile_size(960, 0).is_err());
    }

    #[test]
    fn staging_ttl_is_three_segment_windows() {
        assert_eq!(staging_ttl(15), StdDuration::from_secs(15 * 60 * 3));
    }

    #[test]
    fn build_grid_args_with_four_real_inputs() {
        let inputs = [
            Some(PathBuf::from("/staging/tl/seg_1.mp4")),
            Some(PathBuf::from("/staging/tr/seg_1.mp4")),
            Some(PathBuf::from("/staging/bl/seg_1.mp4")),
            Some(PathBuf::from("/staging/br/seg_1.mp4")),
        ];
        let args = build_grid_args(&inputs, 960, 540, 8, 900, Path::new("/grid/seg_1.mp4"));
        assert_eq!(
            args,
            vec![
                "-hide_banner", "-loglevel", "warning", "-y",
                "-i", "/staging/tl/seg_1.mp4",
                "-i", "/staging/tr/seg_1.mp4",
                "-i", "/staging/bl/seg_1.mp4",
                "-i", "/staging/br/seg_1.mp4",
                "-filter_complex",
                "[0:v]scale=960:540,setsar=1,fps=8,tpad=stop_mode=add:stop_duration=900:color=black[v0];[1:v]scale=960:540,setsar=1,fps=8,tpad=stop_mode=add:stop_duration=900:color=black[v1];[2:v]scale=960:540,setsar=1,fps=8,tpad=stop_mode=add:stop_duration=900:color=black[v2];[3:v]scale=960:540,setsar=1,fps=8,tpad=stop_mode=add:stop_duration=900:color=black[v3];[v0][v1][v2][v3]xstack=inputs=4:layout=0_0|w0_0|0_h0|w0_h0:fill=black[out]",
                "-map", "[out]",
                "-t", "900",
                "-an",
                "-c:v", "libx264",
                "-preset", "ultrafast",
                "-crf", "28",
                "/grid/seg_1.mp4",
            ]
        );
    }

    #[test]
    fn build_grid_args_with_partial_placeholders_keeps_slot_order() {
        let inputs = [
            Some(PathBuf::from("/staging/tl/seg_1.mp4")),
            None,
            None,
            Some(PathBuf::from("/staging/br/seg_1.mp4")),
        ];
        let args = build_grid_args(&inputs, 960, 540, 8, 900, Path::new("/grid/seg_1.mp4"));
        assert_eq!(
            &args[4..15],
            &[
                "-i", "/staging/tl/seg_1.mp4",
                "-f", "lavfi", "-i", "color=c=black:s=960x540:r=8",
                "-f", "lavfi", "-i", "color=c=black:s=960x540:r=8",
                "-i",
            ]
        );
        assert_eq!(args[15], "/staging/br/seg_1.mp4");
    }

    #[test]
    fn build_grid_args_with_one_real_input() {
        let inputs = [None, Some(PathBuf::from("/staging/tr/seg_1.mp4")), None, None];
        let args = build_grid_args(&inputs, 640, 360, 8, 900, Path::new("/grid/seg_1.mp4"));
        let i_count = args.iter().filter(|a| a.as_str() == "-i").count();
        assert_eq!(i_count, 4);
        assert!(args.contains(&"/staging/tr/seg_1.mp4".to_string()));
    }

    #[test]
    fn build_grid_args_never_truncates_to_shortest_input() {
        // Regression guard: an earlier version used `xstack=...:shortest=1`,
        // which cut the WHOLE grid short whenever any single camera's
        // segment was short — observed live in production. The fix pads
        // every real input with black (`tpad`) and hard-caps the output
        // duration with `-t`, never `shortest`.
        let inputs = [
            Some(PathBuf::from("/staging/tl/seg_1.mp4")),
            Some(PathBuf::from("/staging/tr/seg_1.mp4")),
            Some(PathBuf::from("/staging/bl/seg_1.mp4")),
            Some(PathBuf::from("/staging/br/seg_1.mp4")),
        ];
        let args = build_grid_args(&inputs, 960, 540, 8, 900, Path::new("/grid/seg_1.mp4"));
        assert!(!args.iter().any(|a| a.contains("shortest")));
        assert!(args.iter().any(|a| a.contains("tpad=stop_mode=add:stop_duration=900")));
        let t_pos = args.iter().position(|a| a == "-t").expect("-t flag must be present");
        assert_eq!(args[t_pos + 1], "900");
    }
}
