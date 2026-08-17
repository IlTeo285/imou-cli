use thiserror::Error;

#[derive(Debug, Error)]
pub enum ImouError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON (de)serialization failed: {0}")]
    Json(#[from] serde_json::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Imou API error {code}: {msg}")]
    Api { code: String, msg: String },

    #[error("missing configuration: {0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, ImouError>;
