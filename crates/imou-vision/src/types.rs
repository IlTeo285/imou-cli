use serde::{Deserialize, Serialize};

/// Coarse content category assigned to a clip. Deliberately a small, fixed
/// set rather than open-vocabulary output — `relevance::is_relevant` (and
/// any downstream automation) needs something it can match on, and a model
/// asked to "pick one of these six words" is far more reliable than one
/// asked to freeform-classify.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Human,
    Vehicle,
    Animal,
    Package,
    Empty,
    /// The model's output didn't match any known category (or wasn't valid
    /// JSON at all) — `description`/`raw_model_output` still carry whatever
    /// the model said, so nothing is lost, it's just not classified.
    Unknown,
}

impl std::fmt::Display for Category {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Category::Human => "human",
            Category::Vehicle => "vehicle",
            Category::Animal => "animal",
            Category::Package => "package",
            Category::Empty => "empty",
            Category::Unknown => "unknown",
        };
        f.write_str(s)
    }
}

/// The result of analyzing one clip's extracted frames.
#[derive(Debug, Clone, Serialize)]
pub struct AnalysisResult {
    pub category: Category,
    /// Free-text description from the model (e.g. "a person walking a dog
    /// past the front door").
    pub description: String,
    /// Whether this result should be treated as a real, actionable event —
    /// computed by `relevance::is_relevant` from `category`, not asserted
    /// by the model itself (a local vision-language model's own judgment of
    /// "is this relevant" is far less reliable than its judgment of "what
    /// is this").
    pub relevant: bool,
    /// The model's raw text output, kept for debugging/logging. Truncated
    /// to a sane length so a runaway/repetitive generation can't bloat the
    /// event log.
    pub raw_model_output: String,
}
