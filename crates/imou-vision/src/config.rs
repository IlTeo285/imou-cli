use std::time::Duration;

const DEFAULT_TIMEOUT_SECS: u64 = 60;
const DEFAULT_FRAME_COUNT: u8 = 2;
const MIN_FRAME_COUNT: u8 = 1;
const MAX_FRAME_COUNT: u8 = 3;

/// Ollama connection details plus how many frames to sample per clip.
/// Presence of `AI_OLLAMA_URL`/`AI_MODEL_NAME` in the environment is what
/// gates the whole feature — same "optional, no-op if unset" convention as
/// `gdrive::GDriveConfig`/`mqtt::MqttConfig`.
#[derive(Debug, Clone)]
pub struct VisionConfig {
    pub url: String,
    pub model: String,
    pub timeout: Duration,
    pub frame_count: u8,
}

/// Reads `AI_OLLAMA_URL`/`AI_MODEL_NAME` (both required together) plus
/// optional `AI_TIMEOUT_SECS`/`AI_FRAME_COUNT` from the environment. Returns
/// `None` if the required pair is missing — callers should treat that as
/// "AI analysis not configured," not as an error.
pub fn config_from_env() -> Option<VisionConfig> {
    let url = std::env::var("AI_OLLAMA_URL").ok()?;
    let model = std::env::var("AI_MODEL_NAME").ok()?;

    let timeout = std::env::var("AI_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(DEFAULT_TIMEOUT_SECS));

    let frame_count = std::env::var("AI_FRAME_COUNT")
        .ok()
        .and_then(|s| s.parse::<u8>().ok())
        .unwrap_or(DEFAULT_FRAME_COUNT)
        .clamp(MIN_FRAME_COUNT, MAX_FRAME_COUNT);

    Some(VisionConfig {
        url,
        model,
        timeout,
        frame_count,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    // `std::env::var` is process-global state shared across `cargo test`'s
    // parallel threads — serialize these tests with a lock so they can't
    // interleave and observe each other's env var mutations.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn clear_env() {
        unsafe {
            std::env::remove_var("AI_OLLAMA_URL");
            std::env::remove_var("AI_MODEL_NAME");
            std::env::remove_var("AI_TIMEOUT_SECS");
            std::env::remove_var("AI_FRAME_COUNT");
        }
    }

    #[test]
    fn none_when_required_vars_missing() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        assert!(config_from_env().is_none());
    }

    #[test]
    fn some_with_defaults_when_only_required_vars_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        unsafe {
            std::env::set_var("AI_OLLAMA_URL", "http://localhost:11434");
            std::env::set_var("AI_MODEL_NAME", "moondream");
        }
        let cfg = config_from_env().expect("required vars are set");
        assert_eq!(cfg.url, "http://localhost:11434");
        assert_eq!(cfg.model, "moondream");
        assert_eq!(cfg.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
        assert_eq!(cfg.frame_count, DEFAULT_FRAME_COUNT);
        clear_env();
    }

    #[test]
    fn frame_count_is_clamped() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        unsafe {
            std::env::set_var("AI_OLLAMA_URL", "http://localhost:11434");
            std::env::set_var("AI_MODEL_NAME", "moondream");
            std::env::set_var("AI_FRAME_COUNT", "99");
        }
        let cfg = config_from_env().expect("required vars are set");
        assert_eq!(cfg.frame_count, MAX_FRAME_COUNT);
        clear_env();
    }
}
