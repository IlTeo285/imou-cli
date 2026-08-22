use base64::Engine;
use serde::Serialize;
use serde_json::{json, Value};

use crate::config::VisionConfig;
use crate::error::Result;
use crate::response::parse_generate_response;
use crate::types::AnalysisResult;

const PROMPT: &str = "You are a security camera analyst. Look at the image(s), which are frames from one motion-triggered clip. Classify the content and describe briefly what is happening. Use category \"empty\" if nothing of note is in frame (e.g. just wind, shadows, or lighting changes).";

/// A JSON Schema, passed as Ollama's `format` field to constrain decoding
/// (Ollama's "structured outputs" feature — grammar-based, applies
/// uniformly regardless of the model). Verified live against a real
/// `moondream` instance: with the bare string `format: "json"` (just "some
/// valid JSON, shape unconstrained"), moondream reliably degenerated into
/// repeating a garbage key (`"distance_elapsed"`, ad infinitum) until it
/// hit its generation limit, 30+ seconds later, producing UNTERMINATED —
/// not just verbose — JSON (an open string with no closing brace) that
/// fails to parse at all despite starting with a perfectly good `category`/
/// `description`. Passing this schema instead constrains generation to
/// exactly these two fields, closing the object as soon as they're filled:
/// same test image, under 1 second, clean `done_reason: "stop"`.
fn response_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "category": {
                "type": "string",
                "enum": ["human", "vehicle", "animal", "package", "empty"]
            },
            "description": { "type": "string" }
        },
        "required": ["category", "description"]
    })
}

#[derive(Serialize)]
struct GenerateRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    images: &'a [String],
    format: Value,
    stream: bool,
}

/// Calls a local Ollama instance's `/api/generate` endpoint with a
/// vision-language model. Fire-and-forget/fail-open policy is the caller's
/// responsibility (see `recorder.rs`'s use of this) — this client returns
/// real `Result`s, same convention as `gdrive::upload_clip`.
pub struct VisionClient {
    http: reqwest::Client,
    config: VisionConfig,
}

impl VisionClient {
    pub fn new(config: VisionConfig) -> Self {
        Self {
            // Explicit timeout — this codebase has a documented incident
            // (see CLAUDE.md) where a network call with no timeout silently
            // hung a long-lived loop forever with no crash/error to signal
            // it; every HTTP client here sets one deliberately.
            http: reqwest::Client::builder()
                .timeout(config.timeout)
                .build()
                .expect("reqwest client with a timeout should always build"),
            config,
        }
    }

    pub(crate) fn config(&self) -> &VisionConfig {
        &self.config
    }

    /// Sends `frame_bytes` (already-read JPEG file contents) to the
    /// configured model and parses its classification.
    pub async fn analyze_frames(&self, frame_bytes: &[Vec<u8>]) -> Result<AnalysisResult> {
        let images: Vec<String> = frame_bytes
            .iter()
            .map(|b| base64::engine::general_purpose::STANDARD.encode(b))
            .collect();

        let url = format!("{}/api/generate", self.config.url.trim_end_matches('/'));
        let body = self
            .http
            .post(url)
            .json(&GenerateRequest {
                model: &self.config.model,
                prompt: PROMPT,
                images: &images,
                format: response_schema(),
                stream: false,
            })
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;

        parse_generate_response(&body)
    }
}
