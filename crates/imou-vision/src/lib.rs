mod client;
mod config;
mod error;
mod frames;
mod relevance;
mod response;
pub mod snapshot;
mod types;

use std::path::Path;

use uuid::Uuid;

pub use client::VisionClient;
pub use config::{config_from_env, VisionConfig};
pub use error::{Result, VisionError};
pub use frames::extract_frames;
pub use relevance::is_relevant;
pub use snapshot::{extract_snapshots, SnapshotConfig};
pub use types::{AnalysisResult, Category};

/// Extracts representative frames from `clip_path`, sends them to the
/// configured Ollama model, and returns the classification. Scratch frame
/// files are written under a per-call temp directory that's removed
/// (best-effort) before returning, on both the success and error paths —
/// callers own no cleanup of their own.
pub async fn analyze_clip(client: &VisionClient, clip_path: &Path) -> Result<AnalysisResult> {
    let scratch_dir = std::env::temp_dir().join(format!("imou-vision-{}", Uuid::new_v4()));

    let result = analyze_clip_inner(client, clip_path, &scratch_dir).await;

    let _ = tokio::fs::remove_dir_all(&scratch_dir).await;
    result
}

async fn analyze_clip_inner(
    client: &VisionClient,
    clip_path: &Path,
    scratch_dir: &Path,
) -> Result<AnalysisResult> {
    let config = client.config();
    let frame_paths = frames::extract_frames(clip_path, scratch_dir, config.frame_count).await?;

    let mut frame_bytes = Vec::with_capacity(frame_paths.len());
    for path in &frame_paths {
        frame_bytes.push(tokio::fs::read(path).await?);
    }

    client.analyze_frames(&frame_bytes).await
}
