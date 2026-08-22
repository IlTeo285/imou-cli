use serde::Deserialize;

use crate::error::{Result, VisionError};
use crate::relevance::is_relevant;
use crate::types::{AnalysisResult, Category};

/// Caps how much of the model's raw text ends up in the event log/MQTT
/// payload — a runaway or repetitive generation shouldn't bloat either.
const MAX_RAW_OUTPUT_LEN: usize = 2000;

/// The outer envelope of an Ollama `/api/generate` response.
#[derive(Debug, Deserialize)]
pub(crate) struct OllamaGenerateResponse {
    #[serde(default)]
    pub response: String,
}

/// What we ask the model to produce, requested via Ollama's `format: "json"`
/// option. Fields are optional so a partially-conforming generation still
/// yields a usable (if `Unknown`-categorized) result instead of failing
/// outright.
#[derive(Debug, Default, Deserialize)]
struct ModelJson {
    #[serde(default)]
    category: String,
    #[serde(default)]
    description: String,
}

fn category_from_str(s: &str) -> Category {
    match s.trim().to_lowercase().as_str() {
        "human" | "person" | "people" => Category::Human,
        "vehicle" | "car" | "truck" | "bike" | "bicycle" => Category::Vehicle,
        "animal" | "pet" => Category::Animal,
        "package" | "parcel" | "delivery" => Category::Package,
        "empty" | "none" | "nothing" => Category::Empty,
        _ => Category::Unknown,
    }
}

/// Parses one Ollama `/api/generate` response body (requested with
/// `format: "json"`) into an `AnalysisResult`.
///
/// Non-obvious gotcha, why this isn't a single `serde_json::from_str`: with
/// `format: "json"`, Ollama's `response` field holds the model's structured
/// output as a JSON-encoded **string**, not a nested JSON object — so this
/// is a genuine double-decode (outer envelope, then the inner string).
///
/// A model that ignores the format instruction (non-JSON `response`, or one
/// missing/misnaming fields) degrades to `Category::Unknown` with the raw
/// text as the description, rather than erroring — analysis quality should
/// degrade gracefully, it must never block the clip pipeline.
pub fn parse_generate_response(body: &str) -> Result<AnalysisResult> {
    let envelope: OllamaGenerateResponse = serde_json::from_str(body)?;
    if envelope.response.trim().is_empty() {
        return Err(VisionError::EmptyModelOutput);
    }
    Ok(parse_model_output(&envelope.response))
}

fn parse_model_output(raw: &str) -> AnalysisResult {
    let raw_model_output: String = raw.chars().take(MAX_RAW_OUTPUT_LEN).collect();
    let parsed = serde_json::from_str::<ModelJson>(raw).unwrap_or_default();

    let category = category_from_str(&parsed.category);
    let description = if parsed.description.trim().is_empty() {
        raw_model_output.clone()
    } else {
        parsed.description
    };

    AnalysisResult {
        category,
        description,
        relevant: is_relevant(category),
        raw_model_output,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured live from a real `ollama run moondream` `/api/generate` call
    // against `http://localhost:11434`, using `client.rs`'s actual
    // JSON-Schema `format` (not the bare string `"json"` — see that
    // module's doc note on why) — only the structure and the inner JSON's
    // shape matter for this test, per this project's "verify against the
    // real API, not the doc's example" convention (see CLAUDE.md).
    const REAL_RESPONSE: &str = r#"{"model":"moondream","created_at":"2026-08-22T13:35:35.650981945Z","response":"{\"category\": \"empty\", \"description\": \"frames from one motion-triggered clip\" }","done":true,"done_reason":"stop","total_duration":922000000,"eval_count":19}"#;

    #[test]
    fn parses_real_ollama_response() {
        let result = parse_generate_response(REAL_RESPONSE).expect("real response must parse");
        assert_eq!(result.category, Category::Empty);
        assert_eq!(result.description, "frames from one motion-triggered clip");
        assert!(!result.relevant);
    }

    // Also captured live, from the SAME model/image, but with the bare
    // string `format: "json"` instead of the schema `client.rs` actually
    // sends: moondream degenerated into repeating a garbage key
    // ("distance_elapsed") until it hit its generation limit ~31 seconds
    // later, leaving the inner `response` string genuinely UNTERMINATED
    // (an open string, no closing brace) despite starting with a perfectly
    // good category/description — this is real, observed model behavior,
    // not a hypothetical. `serde_json::from_str` correctly refuses to parse
    // it as a whole (it isn't valid JSON), so this only degrades to
    // `Unknown` rather than silently fabricating a result; switching to a
    // JSON-Schema `format` (see `client.rs::response_schema`) is what
    // actually fixes the root cause, not this fallback.
    #[test]
    fn falls_back_to_unknown_on_a_real_captured_unterminated_response() {
        let body = r#"{"response":"{\"category\": \"human\", \"description\": \"A person is standing in front of the camera.\",\"frame\": 0,\"distance_elapsed\": 0.0,\"angle_elapsed\": 0.0,\"speed_elapsed\": 0.0,\"distance_elapsed\": 0.0,\"angle_elapsed","done":true,"done_reason":"length"}"#;
        let result = parse_generate_response(body).expect("outer envelope still parses");
        assert_eq!(result.category, Category::Unknown);
        assert!(!result.relevant);
    }

    #[test]
    fn falls_back_to_unknown_when_inner_response_is_not_json() {
        let body = r#"{"response":"sure, I see a person in the driveway.","done":true}"#;
        let result = parse_generate_response(body).expect("outer envelope still parses");
        assert_eq!(result.category, Category::Unknown);
        assert!(!result.relevant);
        assert_eq!(result.description, "sure, I see a person in the driveway.");
    }

    #[test]
    fn falls_back_to_unknown_for_an_unrecognized_category_word() {
        let body = r#"{"response":"{\"category\": \"spaceship\", \"description\": \"an alien craft\"}","done":true}"#;
        let result = parse_generate_response(body).expect("must parse");
        assert_eq!(result.category, Category::Unknown);
        assert_eq!(result.description, "an alien craft");
    }

    #[test]
    fn empty_category_maps_to_relevant_categories_correctly() {
        assert_eq!(category_from_str("Animal"), Category::Animal);
        assert_eq!(category_from_str("EMPTY"), Category::Empty);
        assert_eq!(category_from_str(""), Category::Unknown);
    }

    #[test]
    fn errors_on_empty_model_output() {
        let body = r#"{"response":"","done":true}"#;
        assert!(matches!(parse_generate_response(body), Err(VisionError::EmptyModelOutput)));
    }

    #[test]
    fn errors_on_malformed_outer_envelope() {
        assert!(parse_generate_response("not json at all").is_err());
    }
}
