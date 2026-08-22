use thiserror::Error;

#[derive(Debug, Error)]
pub enum VisionError {
    #[error("HTTP request to Ollama failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON (de)serialization failed: {0}")]
    Json(#[from] serde_json::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("ffmpeg/ffprobe failed: {0}")]
    Ffmpeg(String),

    #[error("no frames could be extracted from the clip: {0}")]
    NoFrames(String),

    #[error("Ollama returned an empty response")]
    EmptyModelOutput,
}

pub type Result<T> = std::result::Result<T, VisionError>;
